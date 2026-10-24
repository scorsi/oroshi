use super::upload_tests::{fixture, request, row_count};
use super::*;
use axum::body::{Body, to_bytes};
use oroshi_nix::{NarInfo, encode_base32, parse_store_path};
use sha2::{Digest, Sha256};

const LIBICONV: &str = crate::nix::tests::FIXTURE;
const HELLO: &str = include_str!(
    "../../../../crates/oroshi-nix/tests/fixtures/narinfo/cache-f6ivp5fiw1nwq690548fb9qr8p968mj5.narinfo"
);

fn path(info: &NarInfo) -> String {
    format!(
        "/nix/{}.narinfo",
        parse_store_path(&info.store_path).unwrap().hash
    )
}

async fn put(app: Router, info: &NarInfo) -> Response {
    request(
        app,
        "PUT",
        &path(info),
        Some("secret"),
        Body::from(info.to_string()),
    )
    .await
}

pub(super) async fn uploaded(app: Router, fixture: &str) -> NarInfo {
    let mut info: NarInfo = fixture.parse().unwrap();
    // Replace only unsigned transport fields: the real fixture signatures stay valid.
    let bytes = format!("compressed NAR for {}", info.store_path).into_bytes();
    info.file_hash = Some(Sha256::digest(&bytes).into());
    info.file_size = Some(bytes.len() as u64);
    info.url = format!("nar/{}.nar.xz", encode_base32(&info.file_hash.unwrap()));
    assert_eq!(
        request(
            app,
            "PUT",
            &format!("/nix/{}", info.url),
            Some("secret"),
            Body::from(bytes)
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    info
}

async fn read(app: Router, info: &NarInfo) -> NarInfo {
    let response = request(app, "GET", &path(info), None, Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/x-nix-narinfo"
    );
    std::str::from_utf8(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
        .unwrap()
        .parse()
        .unwrap()
}

async fn refused(response: Response, status: StatusCode, reason: &str) {
    assert_eq!(response.status(), status);
    assert!(
        response
            .extensions()
            .get::<crate::nix::UploadRefused>()
            .is_some()
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(
        std::str::from_utf8(&body).unwrap().contains(reason),
        "{body:?}"
    );
}

#[tokio::test]
async fn signed_closure_commits_self_reference_and_is_readable_after_reopen() {
    let (root, store, app) = fixture();
    let libiconv = uploaded(app.clone(), LIBICONV).await;
    let hello = uploaded(app.clone(), HELLO).await;
    assert_eq!(put(app.clone(), &libiconv).await.status(), StatusCode::OK);
    assert_eq!(put(app.clone(), &hello).await.status(), StatusCode::OK);
    assert_eq!(read(app.clone(), &libiconv).await, libiconv);
    assert_eq!(read(app.clone(), &hello).await, hello);
    assert_eq!(row_count(&store, "reference"), 2);
    let reopened = oroshi_store::Database::open(root.path()).unwrap().0;
    assert_eq!(
        reopened
            .narinfo(parse_store_path(&hello.store_path).unwrap().hash)
            .unwrap()
            .unwrap(),
        hello.to_string()
    );
}

#[tokio::test]
async fn signatures_merge_without_duplicates_or_metadata_changes() {
    let (_root, store, app) = fixture();
    let full = uploaded(app.clone(), LIBICONV).await;
    let mut initial = full.clone();
    initial
        .signatures
        .retain(|sig| sig.starts_with("test.example.org-1:"));
    assert_eq!(put(app.clone(), &initial).await.status(), StatusCode::OK);
    // The upstream signature is valid but its key is not trusted here: it is not merged.
    for _ in 0..2 {
        assert_eq!(put(app.clone(), &full).await.status(), StatusCode::OK);
    }
    assert_eq!(read(app.clone(), &initial).await, initial);
    drop(app);
    let mut store = store;
    Arc::get_mut(&mut store).unwrap().trusted_keys.push(
        include_str!("../../../../crates/oroshi-nix/tests/fixtures/narinfo/cache.nixos.org-1.pub")
            .trim()
            .parse()
            .unwrap(),
    );
    let config = Config::parse(&format!(
        "[tokens.writer]\nsha256 = {:?}\nscopes = ['nix:write']",
        format!("{:x}", Sha256::digest(b"secret")),
    ))
    .unwrap();
    let app = super::app(store.clone(), Arc::new(Registry::new()), &config);
    for _ in 0..2 {
        assert_eq!(put(app.clone(), &full).await.status(), StatusCode::OK);
    }
    let mut expected = initial;
    expected.merge_signatures(&full).unwrap();
    assert_eq!(expected.signatures.len(), 2);
    assert_eq!(read(app.clone(), &expected).await, expected);
    assert_eq!(row_count(&store, "narinfo"), 1);
    assert_eq!(row_count(&store, "reference"), 1);
}

#[tokio::test]
async fn compression_and_content_address_are_validated_before_commit() {
    let (_root, store, app) = fixture();
    let original = uploaded(app.clone(), LIBICONV).await;
    for change in 0..4 {
        let mut info = original.clone();
        match change {
            0 => info.compression = Some("zstd".into()),
            1 => info.compression = Some("gzip".into()),
            2 => info.compression = None,
            _ => info.ca = Some("fixed:sha256:example".into()),
        }
        refused(
            put(app.clone(), &info).await,
            StatusCode::BAD_REQUEST,
            "invalid-narinfo",
        )
        .await;
    }
    assert_eq!(row_count(&store, "narinfo"), 0);
    assert_eq!(put(app, &original).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn bad_store_hash_and_url_hash_mismatch_are_refused() {
    let (_root, store, app) = fixture();
    let info = uploaded(app.clone(), LIBICONV).await;
    for name in [
        "bad",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        "00000000000000000000000000000000",
    ] {
        refused(
            request(
                app.clone(),
                "PUT",
                &format!("/nix/{name}.narinfo"),
                Some("secret"),
                Body::from(info.to_string()),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "hash",
        )
        .await;
    }
    let invalid = info.to_string().replace("/nix/store/", "/invalid/store/");
    refused(
        request(
            app,
            "PUT",
            &path(&info),
            Some("secret"),
            Body::from(invalid),
        )
        .await,
        StatusCode::BAD_REQUEST,
        "invalid-narinfo",
    )
    .await;
    assert_eq!(row_count(&store, "narinfo"), 0);
}

#[tokio::test]
async fn malformed_nar_urls_and_missing_nars_are_refused() {
    let (_root, store, app) = fixture();
    let mut info: NarInfo = LIBICONV.parse().unwrap();
    refused(
        put(app.clone(), &info).await,
        StatusCode::BAD_REQUEST,
        "missing-nar",
    )
    .await;
    for url in [
        "https://example.org/nar/file.nar",
        "/nar/file.nar",
        "nar/../file.nar",
        "nar/bad.nar",
        "nar/file.nar.gz",
    ] {
        info.url = url.into();
        refused(
            put(app.clone(), &info).await,
            StatusCode::BAD_REQUEST,
            "bad-name",
        )
        .await;
    }
    assert_eq!(row_count(&store, "narinfo"), 0);
}

#[tokio::test]
async fn file_hash_and_size_must_be_present_and_match_uploaded_nar() {
    let (_root, store, app) = fixture();
    let original = uploaded(app.clone(), LIBICONV).await;
    for change in 0..4 {
        let mut info = original.clone();
        match change {
            0 => info.file_hash = None,
            1 => info.file_hash = Some([0; 32]),
            2 => info.file_size = None,
            _ => info.file_size = Some(info.file_size.unwrap() + 1),
        }
        refused(
            put(app.clone(), &info).await,
            StatusCode::BAD_REQUEST,
            if change < 2 {
                "hash-mismatch"
            } else {
                "file-size-mismatch"
            },
        )
        .await;
    }
    assert_eq!(row_count(&store, "narinfo"), 0);
}

#[tokio::test]
async fn missing_nar_hash_and_size_and_malformed_body_are_refused() {
    let (_root, store, app) = fixture();
    let info = uploaded(app.clone(), LIBICONV).await;
    for field in ["NarHash", "NarSize"] {
        let body = info
            .to_string()
            .lines()
            .filter(|line| !line.starts_with(field))
            .collect::<Vec<_>>()
            .join("\n");
        refused(
            request(
                app.clone(),
                "PUT",
                &path(&info),
                Some("secret"),
                Body::from(body),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "invalid-narinfo",
        )
        .await;
    }
    for body in [b"malformed".as_slice(), &[255]] {
        assert_eq!(
            request(
                app.clone(),
                "PUT",
                &path(&info),
                Some("secret"),
                Body::from(body.to_vec())
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(row_count(&store, "narinfo"), 0);
}

#[tokio::test]
async fn missing_reference_rolls_back_then_topological_upload_succeeds() {
    let (_root, store, app) = fixture();
    let hello = uploaded(app.clone(), HELLO).await;
    refused(
        put(app.clone(), &hello).await,
        StatusCode::BAD_REQUEST,
        "reference",
    )
    .await;
    assert_eq!(row_count(&store, "narinfo"), 0);
    assert_eq!(row_count(&store, "reference"), 0);
    let libiconv = uploaded(app.clone(), LIBICONV).await;
    assert_eq!(put(app.clone(), &libiconv).await.status(), StatusCode::OK);
    assert_eq!(put(app.clone(), &hello).await.status(), StatusCode::OK);
    assert_eq!(read(app, &hello).await, hello);
}

#[tokio::test]
async fn unsigned_untrusted_and_invalid_signatures_are_refused() {
    let (_root, store, app) = fixture();
    let original = uploaded(app.clone(), LIBICONV).await;
    for change in 0..4 {
        let mut info = original.clone();
        match change {
            0 => info.signatures.clear(),
            1 => info
                .signatures
                .retain(|sig| sig.starts_with("cache.nixos.org-1:")),
            2 => info.nar_size += 1,
            _ => info.signatures.push("malformed".into()),
        }
        refused(
            put(app.clone(), &info).await,
            StatusCode::BAD_REQUEST,
            if change == 0 {
                "unsigned"
            } else {
                "untrusted-key"
            },
        )
        .await;
    }
    assert_eq!(row_count(&store, "narinfo"), 0);
}

#[tokio::test]
async fn non_signature_conflicts_return_409_without_changing_content() {
    let (_root, store, app) = fixture();
    let original = uploaded(app.clone(), LIBICONV).await;
    assert_eq!(put(app.clone(), &original).await.status(), StatusCode::OK);
    for change in 0..10 {
        let mut info = original.clone();
        match change {
            0 => info.compression = Some("none".into()),
            1 => info.deriver = None,
            2 => info.ca = Some("fixed:sha256:example".into()),
            3 => info.file_size = Some(info.file_size.unwrap() + 1),
            4 => info.file_hash = Some([0; 32]),
            5 => info.nar_size += 1,
            6 => info.nar_hash = [0; 32],
            7 => info.references.clear(),
            8 => info.store_path.push_str("-other"),
            _ => info.url.push_str(".missing"),
        }
        refused(
            put(app.clone(), &info).await,
            StatusCode::CONFLICT,
            "conflict",
        )
        .await;
        assert_eq!(read(app.clone(), &original).await, original);
    }
    assert_eq!(row_count(&store, "reference"), 1);
}

#[tokio::test]
async fn narinfo_and_ignored_cache_info_require_write_scope() {
    let (_root, store, app) = fixture();
    let info = uploaded(app.clone(), LIBICONV).await;
    for path in [
        path(&info),
        "/nix/nix-cache-info".into(),
        "/nix/".into(),
        "/nix/log/file".into(),
    ] {
        for (credential, status) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("unknown"), StatusCode::UNAUTHORIZED),
            (Some("reader"), StatusCode::FORBIDDEN),
        ] {
            let response = request(
                app.clone(),
                "PUT",
                &path,
                credential,
                Body::from(info.to_string()),
            )
            .await;
            assert_eq!(response.status(), status);
            if status == StatusCode::UNAUTHORIZED {
                assert_eq!(
                    response.headers()[header::WWW_AUTHENTICATE],
                    "Basic realm=\"oroshi\""
                );
            }
        }
    }
    assert_eq!(row_count(&store, "narinfo"), 0);
}

#[tokio::test]
async fn cache_info_put_is_ignored_and_other_puts_are_forbidden() {
    let (_root, store, app) = fixture();
    let before = request(
        app.clone(),
        "GET",
        "/nix/nix-cache-info",
        None,
        Body::empty(),
    )
    .await;
    let before = to_bytes(before.into_body(), usize::MAX).await.unwrap();
    let response = request(
        app.clone(),
        "PUT",
        "/nix/nix-cache-info",
        Some("secret"),
        Body::from("ignored"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    let after = request(
        app.clone(),
        "GET",
        "/nix/nix-cache-info",
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(
        to_bytes(after.into_body(), usize::MAX).await.unwrap(),
        before
    );
    for path in [
        "/nix/",
        "/nix/other",
        "/nix/log/file",
        "/nix/realisations/item",
        "/nix/nar/nested/file",
    ] {
        assert_eq!(
            request(app.clone(), "PUT", path, Some("secret"), Body::empty())
                .await
                .status(),
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    assert_eq!(row_count(&store, "narinfo"), 0);
    assert_eq!(row_count(&store, "nar"), 0);
}

#[tokio::test]
async fn unsupported_content_encoding_returns_415_and_identity_is_accepted() {
    let (_root, store, app) = fixture();
    let info = uploaded(app.clone(), LIBICONV).await;
    for path in [
        path(&info),
        format!("/nix/{}", info.url),
        "/nix/nix-cache-info".into(),
    ] {
        for encoding in ["gzip", "br", "identity, gzip", ""] {
            let request = Request::builder()
                .method("PUT")
                .uri(&path)
                .header(header::AUTHORIZATION, "secret")
                .header(header::CONTENT_ENCODING, encoding)
                .body(Body::from(info.to_string()))
                .unwrap();
            refused(
                send(app.clone(), request).await,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported-encoding",
            )
            .await;
        }
    }
    assert_eq!(row_count(&store, "narinfo"), 0);
    let request = Request::builder()
        .method("PUT")
        .uri(path(&info))
        .header(header::AUTHORIZATION, "secret")
        .header(header::CONTENT_ENCODING, "identity")
        .header(header::CONTENT_ENCODING, "gzip")
        .body(Body::from(info.to_string()))
        .unwrap();
    assert_eq!(
        send(app.clone(), request).await.status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let request = Request::builder()
        .method("PUT")
        .uri(path(&info))
        .header(header::AUTHORIZATION, "secret")
        .header(header::CONTENT_ENCODING, "identity")
        .body(Body::from(info.to_string()))
        .unwrap();
    assert_eq!(send(app.clone(), request).await.status(), StatusCode::OK);
}

async fn send(
    mut app: impl axum::ServiceExt<Request, Response = Response, Error = std::convert::Infallible>,
    request: Request,
) -> Response {
    app.call(request).await.unwrap()
}

#[tokio::test]
async fn configured_upstream_key_accepts_upstream_only_and_empty_trust_refuses_it() {
    let (_root, mut store, router) = fixture();
    drop(router);
    let config = Config::parse(&format!(
        "[tokens.writer]\nsha256 = {:?}\nscopes = ['nix:write']\n[nix]\ntrusted_keys = [{:?}]",
        format!("{:x}", Sha256::digest(b"secret")),
        include_str!("../../../../crates/oroshi-nix/tests/fixtures/narinfo/cache.nixos.org-1.pub")
            .trim(),
    ))
    .unwrap();
    Arc::get_mut(&mut store).unwrap().trusted_keys = config
        .nix
        .trusted_keys
        .iter()
        .map(|key| key.parse().unwrap())
        .collect();
    let router = app(store.clone(), Arc::new(Registry::new()), &config);
    let mut info = uploaded(router.clone(), LIBICONV).await;
    info.signatures
        .retain(|sig| sig.starts_with("cache.nixos.org-1:"));
    assert_eq!(put(router.clone(), &info).await.status(), StatusCode::OK);
    assert_eq!(read(router.clone(), &info).await, info);
    drop(router);
    Arc::get_mut(&mut store).unwrap().trusted_keys.clear();
    let router = app(store, Arc::new(Registry::new()), &config);
    refused(
        put(router, &info).await,
        StatusCode::BAD_REQUEST,
        "untrusted-key",
    )
    .await;
}

#[tokio::test]
async fn narinfo_body_cap_accepts_64_kib_and_refuses_one_more_without_reading_the_tail() {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, ReadBuf};
    use tokio_util::io::ReaderStream;
    struct CappedBody(usize);
    impl AsyncRead for CappedBody {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            assert!(self.0 > 0, "read beyond 64 KiB + 1");
            let len = buf.remaining().min(self.0);
            buf.put_slice(&vec![b'x'; len]);
            self.0 -= len;
            Poll::Ready(Ok(()))
        }
    }
    let (_root, store, app) = fixture();
    let info = uploaded(app.clone(), LIBICONV).await;
    // Pad with well-formed untrusted signatures (kept on a first upload) to the exact boundary.
    let mut wire = info.to_string();
    let signature = |name: &str| format!("Sig: {name}:{}\n", "A".repeat(86) + "==");
    while 64 * 1024 - wire.len() > 2 * signature("pad").len() {
        wire.push_str(&signature("pad"));
    }
    let name = "p".repeat(64 * 1024 - wire.len() - signature("").len());
    wire.push_str(&signature(&name));
    assert_eq!(wire.len(), 64 * 1024);
    assert_eq!(
        request(
            app.clone(),
            "PUT",
            &path(&info),
            Some("secret"),
            Body::from(wire)
        )
        .await
        .status(),
        StatusCode::OK
    );
    for body in [
        Body::from(vec![b'x'; 64 * 1024 + 1]),
        Body::from_stream(ReaderStream::new(CappedBody(64 * 1024 + 1))),
    ] {
        refused(
            request(app.clone(), "PUT", &path(&info), Some("secret"), body).await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "too-large",
        )
        .await;
    }
    assert_eq!(row_count(&store, "narinfo"), 1);
}
