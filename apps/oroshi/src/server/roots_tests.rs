use super::narinfo_tests::{put, uploaded};
use super::upload_tests::{fixture, request, row_count};
use super::*;
use crate::log::tests::{Capture, schema};
use axum::body::{Body, to_bytes};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

const HELLO: &str = include_str!(
    "../../../../crates/oroshi-nix/tests/fixtures/narinfo/cache-f6ivp5fiw1nwq690548fb9qr8p968mj5.narinfo"
);

/// A store holding libiconv and hello, and an app with a writer and a pinner token.
async fn with_paths() -> (
    tempfile::TempDir,
    Arc<crate::nix::Store>,
    Router,
    [String; 2],
) {
    let (root, store, _) = fixture();
    let config = Config::parse(&format!(
        "[tokens.writer]\nsha256 = {:?}\nscopes = ['nix:write']\n\
         [tokens.pinner]\nsha256 = {:?}\nscopes = ['nix:pin']\n",
        format!("{:x}", Sha256::digest(b"secret")),
        format!("{:x}", Sha256::digest(b"pin-secret")),
    ))
    .unwrap();
    let app = app(store.clone(), Arc::new(Registry::new()), &config);
    let mut paths = Vec::new();
    for fixture in [crate::nix::tests::FIXTURE, HELLO] {
        let info = uploaded(app.clone(), fixture).await;
        assert_eq!(put(app.clone(), &info).await.status(), StatusCode::OK);
        paths.push(info.store_path);
    }
    (root, store, app, paths.try_into().unwrap())
}

async fn logged(
    app: Router,
    method: &str,
    path: &str,
    credential: Option<&str>,
    body: &str,
) -> (StatusCode, String, Vec<Value>) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(bytes.clone()));
    let response = request(app, method, path, credential, Body::from(body.to_owned()))
        .with_subscriber(subscriber)
        .await;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let lines = String::from_utf8(bytes.lock().unwrap().clone())
        .unwrap()
        .lines()
        .map(|line| {
            assert!(schema::valid(line), "{line}");
            serde_json::from_str(line).unwrap()
        })
        .collect();
    (status, String::from_utf8(body.to_vec()).unwrap(), lines)
}

fn roots(store: &crate::nix::Store) -> Vec<(String, String)> {
    store
        .database
        .reader()
        .unwrap()
        .prepare("SELECT name, store_path FROM root ORDER BY name, store_path")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn roots_need_the_pin_scope() {
    let (_root, store, app, [libiconv, _]) = with_paths().await;
    for (credential, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("secret"), StatusCode::FORBIDDEN),
    ] {
        for method in ["PUT", "DELETE"] {
            let (got, _, lines) = logged(
                app.clone(),
                method,
                "/nix/_oroshi/roots/ci",
                credential,
                &libiconv,
            )
            .await;
            assert_eq!(got, status);
            assert!(lines.iter().any(|line| line["event"] == "auth-refused"));
        }
    }
    assert_eq!(row_count(&store, "root"), 0);
    // There is no read endpoint (ADR 0023).
    let (status, _, _) = logged(app, "GET", "/nix/_oroshi/roots/ci", None, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_bodies_and_names_are_refused_and_keep_the_previous_content() {
    let (_root, store, app, [libiconv, _]) = with_paths().await;
    let pin = Some("pin-secret");
    let (status, _, _) = logged(app.clone(), "PUT", "/nix/_oroshi/roots/ci", pin, &libiconv).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let absent = "/nix/store/00000000000000000000000000000000-absent";
    let long = "a".repeat(65);
    for (path, body, status, reason) in [
        (
            "ci",
            format!("{libiconv}\n{absent}\n"),
            StatusCode::BAD_REQUEST,
            "missing-reference",
        ),
        (
            "ci",
            format!("{libiconv}\nnot-a-path\n"),
            StatusCode::BAD_REQUEST,
            "bad-name",
        ),
        ("ci", "\n\n".into(), StatusCode::BAD_REQUEST, "invalid-body"),
        (
            "ci",
            "x".repeat(1024 * 1024 + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
            "too-large",
        ),
        (
            "bad%20name",
            libiconv.clone(),
            StatusCode::BAD_REQUEST,
            "bad-name",
        ),
        (
            long.as_str(),
            libiconv.clone(),
            StatusCode::BAD_REQUEST,
            "bad-name",
        ),
    ] {
        let (got, text, lines) = logged(
            app.clone(),
            "PUT",
            &format!("/nix/_oroshi/roots/{path}"),
            pin,
            &body,
        )
        .await;
        assert_eq!((got, text.as_str()), (status, reason));
        let refused: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "upload-refused")
            .collect();
        assert_eq!(refused.len(), 1, "{lines:?}");
        assert_eq!(refused[0]["kind"], "root");
        assert_eq!(refused[0]["reason"], reason);
    }
    assert_eq!(roots(&store), [("ci".to_owned(), libiconv)]);
}

#[tokio::test]
async fn put_replaces_a_root_and_delete_removes_it() {
    let (_root, store, app, [libiconv, hello]) = with_paths().await;
    let pin = Some("pin-secret");
    let (status, _, lines) =
        logged(app.clone(), "PUT", "/nix/_oroshi/roots/ci", pin, &libiconv).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let set: Vec<_> = lines.iter().filter(|l| l["event"] == "root-set").collect();
    assert_eq!(
        (set[0]["name"].as_str(), set[0]["paths"].as_u64()),
        (Some("ci"), Some(1))
    );
    // Repeated paths count once; CRLF and empty lines are accepted.
    let body = format!("{hello}\r\n\n{libiconv}\n{hello}");
    let (status, _, lines) = logged(app.clone(), "PUT", "/nix/_oroshi/roots/ci", pin, &body).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let set: Vec<_> = lines.iter().filter(|l| l["event"] == "root-set").collect();
    assert_eq!(set[0]["paths"], 2);
    let (status, _, _) = logged(
        app.clone(),
        "PUT",
        "/nix/_oroshi/roots/other.v1",
        pin,
        &hello,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let mut expected = vec![
        ("ci".to_owned(), hello.clone()),
        ("ci".to_owned(), libiconv.clone()),
        ("other.v1".to_owned(), hello.clone()),
    ];
    expected.sort();
    assert_eq!(roots(&store), expected);

    let (status, _, lines) = logged(app.clone(), "DELETE", "/nix/_oroshi/roots/ci", pin, "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let deleted: Vec<_> = lines
        .iter()
        .filter(|l| l["event"] == "root-deleted")
        .collect();
    assert_eq!(deleted[0]["name"], "ci");
    assert_eq!(roots(&store), [("other.v1".to_owned(), hello)]);
    let (status, _, lines) = logged(app, "DELETE", "/nix/_oroshi/roots/ci", pin, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(lines.iter().all(|line| line["event"] != "upload-refused"));
}
