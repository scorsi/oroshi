use super::*;
use crate::nix::tests::{FIXTURE, HASH, fixture};
use axum::body::{Body, to_bytes};
use std::convert::Infallible;
use std::sync::Mutex;
use tracing::{Event, Subscriber};
use tracing_subscriber::{layer::Context, prelude::*};

#[derive(Clone)]
struct Events(Arc<Mutex<Vec<String>>>);
impl<S: Subscriber> tracing_subscriber::Layer<S> for Events {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?} ", field.name()));
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

async fn request(
    mut app: impl axum::ServiceExt<Request, Response = Response, Error = Infallible>,
    method: &str,
    path: &str,
) -> (axum::http::response::Parts, axum::body::Bytes) {
    let response = app
        .call(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    (parts, to_bytes(body, usize::MAX).await.unwrap())
}

#[tokio::test]
async fn anonymous_http_reads_head_fallbacks_metrics_and_quiet_logging() {
    use tracing::instrument::WithSubscriber;
    let (_root, store, bytes, info) = fixture();
    for access in [false, true] {
        let registry = Arc::new(Registry::new());
        let events = Events(Arc::default());
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(events.clone()));
        let mut config = Config::default();
        config.log.access = access;
        let app = app(store.clone(), registry.clone(), &config);
        let cases = [
            (
                "GET",
                "/nix/nix-cache-info".into(),
                200,
                b"StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 17\n".as_slice(),
            ),
            (
                "GET",
                format!("/nix/{HASH}.narinfo"),
                200,
                FIXTURE.as_bytes(),
            ),
            ("HEAD", format!("/nix/{HASH}.narinfo"), 200, &[]),
            ("GET", format!("/nix/{}", info.url), 200, bytes.as_slice()),
            ("HEAD", format!("/nix/{}", info.url), 200, &[]),
            (
                "GET",
                "/nix/00000000000000000000000000000000.narinfo".into(),
                404,
                &[],
            ),
            ("GET", "/nix/bad.narinfo".into(), 404, &[]),
            ("GET", "/nix/nar/bad.nar".into(), 404, &[]),
            ("GET", "/nix/nar/%2e%2e%2fsecret".into(), 404, &[]),
            ("GET", "/nix/log/example".into(), 404, &[]),
            ("GET", "/nix/realisations/example".into(), 404, &[]),
            ("GET", "/nix/debuginfo/example".into(), 404, &[]),
            ("GET", format!("/nix/{HASH}.ls"), 404, &[]),
        ];
        for (method, path, status, body) in cases {
            let (response, actual) = request(app.clone(), method, &path)
                .with_subscriber(dispatch.clone())
                .await;
            assert_eq!(response.status.as_u16(), status, "{method} {path}");
            assert_eq!(actual.as_ref(), body, "{method} {path}");
            let headers = response.headers;
            if status == 200 {
                let size = if path.ends_with(".narinfo") {
                    FIXTURE.len()
                } else if path.contains("/nar/") {
                    bytes.len()
                } else {
                    51
                };
                assert_eq!(headers[header::CONTENT_LENGTH], size.to_string());
                if path.ends_with(".narinfo") {
                    assert!(headers[header::CONTENT_TYPE] == "text/x-nix-narinfo");
                }
                if path.contains("/nar/") {
                    assert!(headers[header::CONTENT_TYPE] == "application/x-nix-nar");
                }
            }
        }
        assert_eq!(
            registry
                .requests(Module::Nix, Method::Get, Code::Success)
                .get(),
            3
        );
        assert_eq!(
            registry
                .requests(Module::Nix, Method::Head, Code::Success)
                .get(),
            2
        );
        assert_eq!(
            registry
                .requests(Module::Nix, Method::Get, Code::ClientError)
                .get(),
            5
        );
        assert_eq!(
            registry.response_bytes(Module::Nix).get(),
            (51 + FIXTURE.len() + bytes.len()) as u64
        );
        let lines = events.0.lock().unwrap();
        assert_eq!(lines.len(), if access { 13 } else { 0 }, "{lines:?}");
        if access {
            for line in lines.iter() {
                assert!(line.contains("event=\"request\""), "{line}");
                for field in [
                    "module=", "method=", "path=", "status=", "bytes=", "dur_us=",
                ] {
                    assert!(line.contains(field), "{line}");
                }
            }
        }
    }
}

#[tokio::test]
async fn storage_failures_are_logged_even_with_quiet_reads() {
    use tracing::instrument::WithSubscriber;
    let (root, store, _, info) = fixture();
    let registry = Arc::new(Registry::new());
    let events = Events(Arc::default());
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(events.clone()));
    let app = app(store, registry.clone(), &Config::default());
    std::fs::remove_dir_all(root.path().join("blobs")).unwrap();
    let (response, _) = request(app, "GET", &format!("/nix/{}", info.url))
        .with_subscriber(dispatch)
        .await;
    assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        registry
            .requests(Module::Nix, Method::Get, Code::ServerError)
            .get(),
        1
    );
    let events = events.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].contains("status=500"));
}
