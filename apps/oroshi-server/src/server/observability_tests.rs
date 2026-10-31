use super::upload_tests::{fixture, request};
use super::*;
use crate::log::tests::{Capture, schema};
use crate::metrics::UploadResult;
use axum::body::Body;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

pub(super) fn config() -> Config {
    Config::parse(&format!(
        "[log]\naccess = true\n[tokens.writer]\nsha256 = {:?}\nscopes = ['nix:write']\n\
         [tokens.reader]\nsha256 = {:?}\nscopes = ['nix:read']\n",
        format!("{:x}", Sha256::digest(b"secret")),
        format!("{:x}", Sha256::digest(b"reader-secret")),
    ))
    .unwrap()
}

pub(super) async fn logged_request(
    app: Router,
    path: &str,
    credential: Option<&str>,
    body: Body,
) -> (Response, Vec<Value>, String) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(bytes.clone()));
    let response = request(app, "PUT", path, credential, body)
        .with_subscriber(subscriber)
        .await;
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let lines = text
        .lines()
        .map(|line| {
            assert!(schema::valid(line), "{line}");
            serde_json::from_str(line).unwrap()
        })
        .collect();
    (response, lines, text)
}

fn assert_fields(line: &Value, fields: &[&str]) {
    let mut expected = vec!["v", "ts", "level", "event"];
    expected.extend(fields);
    expected.sort_unstable();
    let mut actual: Vec<_> = line
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    actual.sort_unstable();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn each_upload_refusal_has_one_schema_line_and_one_counter() {
    let (_root, store, _) = fixture();
    let registry = Arc::new(Registry::new());
    let app = app(store, registry.clone(), &config());
    // Parser diagnostics must not echo a submitted credential from the body.
    let cases = [
        ("/nix/bad.narinfo", "secret body", 400, "narinfo"),
        ("/nix/nar/bad.nar", "", 403, "nar"),
        ("/nix/unsupported/path", "", 403, "other"),
        ("/nix/", "", 403, "other"),
    ];
    for (index, (path, body, status, kind)) in cases.into_iter().enumerate() {
        let (response, lines, text) =
            logged_request(app.clone(), path, Some("secret"), Body::from(body)).await;
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(lines.len(), 1, "{text}");
        let line = &lines[0];
        assert_fields(line, &["module", "kind", "reason"]);
        assert_eq!(line["event"], "upload-refused");
        assert_eq!(line["level"], "warn");
        assert_eq!(line["module"], "nix");
        assert_eq!(line["kind"], kind);
        let reason = if kind == "narinfo" {
            "invalid-narinfo"
        } else {
            "bad-name"
        };
        assert_eq!(line["reason"], reason);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
            reason
        );
        assert!(!text.contains("secret"), "{text}");
        assert_eq!(
            registry.uploads(Module::Nix, UploadResult::Refused).get(),
            index as u64 + 1
        );
        assert_eq!(
            registry.uploads(Module::Nix, UploadResult::Committed).get(),
            0
        );
        assert_eq!(registry.auth_refusals(Module::Nix).get(), 0);
        assert_eq!(registry.upload_bytes(Module::Nix).get(), 0);
    }
}

#[tokio::test]
async fn auth_refusals_are_single_lines_and_never_log_token_material() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let (_root, store, _) = fixture();
    let registry = Arc::new(Registry::new());
    let app = app(store, registry.clone(), &config());
    let basic = format!("Basic {}", STANDARD.encode("ignored-user:unknown-secret"));
    let cases = [
        (None, 401, "unknown"),
        (Some("unknown-secret"), 401, "unknown"),
        (Some("Bearer unknown-secret"), 401, "unknown"),
        (Some(basic.as_str()), 401, "unknown"),
        (Some("Basic !!!"), 401, "unknown"),
        (Some("reader-secret"), 403, "reader"),
        (Some("Bearer reader-secret"), 403, "reader"),
        (Some("Basic dXNlcjpyZWFkZXItc2VjcmV0"), 403, "reader"),
    ];
    for (index, (credential, status, name)) in cases.into_iter().enumerate() {
        let (response, lines, text) = logged_request(
            app.clone(),
            "/nix/bad.narinfo",
            credential,
            Body::from("unknown-secret"),
        )
        .await;
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(
            response.headers().contains_key(header::WWW_AUTHENTICATE),
            status == 401
        );
        assert_eq!(lines.len(), 1, "{text}");
        let line = &lines[0];
        assert_fields(line, &["module", "token"]);
        assert_eq!(line["event"], "auth-refused");
        assert_eq!(
            line["level"],
            if credential.is_none() { "info" } else { "warn" }
        );
        assert_eq!(line["module"], "nix");
        assert_eq!(line["token"], name);
        for material in [
            "reader-secret",
            "unknown-secret",
            "ignored-user",
            "Bearer",
            "Basic",
            "Authorization",
            "dXNlcjpyZWFkZXItc2VjcmV0",
            "!!!",
            basic.as_str(),
        ] {
            assert!(!text.contains(material), "{text}");
        }
        assert_eq!(registry.auth_refusals(Module::Nix).get(), index as u64 + 1);
        assert_eq!(
            registry.uploads(Module::Nix, UploadResult::Refused).get(),
            0
        );
        assert_eq!(
            registry.uploads(Module::Nix, UploadResult::Committed).get(),
            0
        );
    }
}

#[tokio::test]
async fn commits_count_actual_bytes_and_scrapes_refresh_deduplicated_database_gauges() {
    let (root, store, _) = fixture();
    let registry = Arc::new(Registry::new());
    let app = app(store.clone(), registry.clone(), &config());
    let payload = b"compressed NAR";
    let hash = oroshi_nix::encode_base32(&Sha256::digest(payload));
    for (index, suffix) in [".nar", ".nar.xz", ".nar"].into_iter().enumerate() {
        let name = format!("{hash}{suffix}");
        let (response, lines, text) = logged_request(
            app.clone(),
            &format!("/nix/nar/{name}"),
            Some("Bearer secret"),
            Body::from(payload.as_slice()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let committed: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "upload-committed")
            .collect();
        assert_eq!(committed.len(), 1, "{text}");
        let line = committed[0];
        assert_fields(line, &["module", "kind", "name", "bytes"]);
        assert_eq!(line["level"], "info");
        assert_eq!(line["module"], "nix");
        assert_eq!(line["kind"], "nar");
        assert_eq!(line["name"], name);
        assert_eq!(line["bytes"], payload.len());
        assert!(!text.contains("secret"));
        assert_eq!(
            registry.uploads(Module::Nix, UploadResult::Committed).get(),
            index as u64 + 1
        );
        assert_eq!(
            registry.upload_bytes(Module::Nix).get(),
            (index as u64 + 1) * payload.len() as u64
        );
    }
    // A fresh registry and reopened database must expose persisted totals, not process deltas.
    let reopened = Arc::new(crate::nix::Store {
        blobs: oroshi_store::BlobStore::open(root.path()).unwrap(),
        database: oroshi_store::Database::open(root.path()).unwrap().0,
        priority: 30,
        trusted_keys: Vec::new(),
        limits: crate::nix::UploadLimits::new(&Config::default()),
        uploads: Default::default(),
    });
    let registry = Arc::new(Registry::new());
    for expected in [payload.len(), 0] {
        let (_, text) = exposition(State((registry.clone(), reopened.clone())))
            .await
            .unwrap();
        assert!(
            text.contains(&format!("oroshi_stored_bytes {expected}\n")),
            "{text}"
        );
        assert!(text.contains(&format!("oroshi_blobs {}\n", usize::from(expected != 0))));
        let runs = usize::from(expected == 0);
        assert!(text.contains(&format!("oroshi_gc_runs_total{{result=\"ok\"}} {runs}\n")));
        assert!(text.contains(&format!(
            "oroshi_gc_runs_total{{result=\"error\"}} {runs}\n"
        )));
        assert!(text.contains(&format!("oroshi_gc_freed_bytes_total {}\n", 7 * runs)));
        assert!(text.contains(&format!(
            "oroshi_gc_last_success_timestamp_seconds {}\n",
            1_700_000_000 * runs
        )));
        if expected != 0 {
            // Persisted by `oroshi-server gc`, read back on the next scrape.
            reopened
                .database
                .record_gc_run(true, 3, 1_700_000_000)
                .unwrap();
            reopened
                .database
                .record_gc_run(false, 4, 1_800_000_000)
                .unwrap();
            reopened
                .database
                .writer()
                .unwrap()
                .execute_batch("DELETE FROM nar; DELETE FROM blob;")
                .unwrap();
        }
    }
    reopened
        .database
        .writer()
        .unwrap()
        .execute_batch("DROP TABLE blob;")
        .unwrap();
    assert_eq!(
        exposition(State((registry, reopened))).await.unwrap_err(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn narinfo_commits_log_store_names_and_count_wire_bytes() {
    let (_root, store, _) = fixture();
    let registry = Arc::new(Registry::new());
    let app = app(store, registry.clone(), &config());
    let info = super::narinfo_tests::uploaded(app.clone(), crate::nix::tests::FIXTURE).await;
    let wire = info.to_string();
    for count in 1..=2 {
        let (response, lines, text) = logged_request(
            app.clone(),
            &format!("/nix/{}.narinfo", crate::nix::tests::HASH),
            Some("secret"),
            Body::from(wire.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let lines: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "upload-committed")
            .collect();
        assert_eq!(lines.len(), 1, "{text}");
        let line = lines[0];
        assert_fields(line, &["module", "kind", "name", "bytes"]);
        assert_eq!(line["level"], "info");
        assert_eq!(line["module"], "nix");
        assert_eq!(line["kind"], "narinfo");
        assert_eq!(line["name"], info.store_path);
        assert_eq!(line["bytes"], wire.len());
        assert!(!text.contains("secret"));
        assert_eq!(
            registry.uploads(Module::Nix, UploadResult::Committed).get(),
            count + 1
        );
        assert_eq!(
            registry.upload_bytes(Module::Nix).get(),
            info.file_size.unwrap() + count * wire.len() as u64
        );
    }
}

#[tokio::test]
async fn failed_metrics_read_logs_one_error_request_with_integer_status() {
    let (_root, store, _) = fixture();
    store
        .database
        .writer()
        .unwrap()
        .execute_batch("DROP TABLE blob;")
        .unwrap();
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(bytes.clone()));
    assert_eq!(
        exposition(State((Arc::new(Registry::new()), store)))
            .with_subscriber(subscriber)
            .await
            .unwrap_err(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(schema::valid(text.trim()));
    let line: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(line["event"], "request");
    assert_eq!(line["level"], "error");
    assert_eq!(line["status"].as_u64(), Some(500));
    assert_eq!(line["path"], "/metrics");
    assert!(line["err"].as_str().unwrap().contains("blob"));
}

#[test]
fn migration_versions_are_logged_once_each_as_integers() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(bytes.clone()));
    tracing::subscriber::with_default(subscriber, || {
        log_migrations(&[1, 2]);
        log_migrations(&[]);
    });
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let lines: Vec<_> = text
        .lines()
        .map(|line| {
            assert!(schema::valid(line));
            serde_json::from_str::<Value>(line).unwrap()
        })
        .collect();
    assert_eq!(lines.len(), 2);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line["event"], "migration-applied");
        assert_eq!(line["version"].as_u64(), Some(index as u64 + 1));
    }
}

#[tokio::test]
async fn request_event_keeps_http_status_an_integer() {
    let (_root, store, _) = fixture();
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(bytes.clone()));
    request(
        app(store, Arc::new(Registry::new()), &config()),
        "GET",
        "/healthz",
        None,
        Body::empty(),
    )
    .with_subscriber(subscriber)
    .await;
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let line: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(line["event"], "request");
    assert_eq!(line["status"].as_u64(), Some(200));
}

#[tokio::test]
async fn request_and_refusal_lines_record_the_client() {
    let (_root, store, _) = fixture();
    let app = app(store, Arc::new(Registry::new()), &config());
    let cases = [
        ("/nix/x.narinfo", None, "auth-refused"),
        ("/nix/bad.narinfo", Some("secret"), "upload-refused"),
        ("/nix/nar/missing.nar", Some("secret"), "request"),
    ];
    for (path, credential, event) in cases {
        let method = if event == "request" { "GET" } else { "PUT" };
        let mut request = Request::builder().method(method).uri(path);
        if let Some(credential) = credential {
            request = request.header(header::AUTHORIZATION, credential);
        }
        let mut request = request.body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(Client("2001:db8::7".parse().unwrap()));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(bytes.clone()));
        super::upload_tests::send(app.clone(), request)
            .with_subscriber(subscriber)
            .await;
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let line: Value = serde_json::from_str(text.lines().next().expect(event)).unwrap();
        assert!(schema::valid(text.lines().next().unwrap()), "{text}");
        assert_eq!(line["event"], event, "{text}");
        assert_eq!(line["client"], "2001:db8::7", "{text}");
    }
}
