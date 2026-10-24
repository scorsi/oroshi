use std::io;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::metrics::{self, Code, Method, Module, Registry};

pub struct Server {
    main: TcpListener,
    metrics: Option<TcpListener>,
}

impl Server {
    pub async fn bind(config: &Config) -> io::Result<Self> {
        let main = TcpListener::bind(config.server.listen).await?;
        let metrics = match config.metrics.listen {
            Some(addr) => Some(TcpListener::bind(addr).await?),
            None => None,
        };
        tracing::info!(event = "listening", addr = %main.local_addr()?);
        if let Some(listener) = &metrics {
            tracing::info!(event = "metrics-listening", addr = %listener.local_addr()?);
        }
        Ok(Self { main, metrics })
    }

    pub async fn run(self) -> io::Result<()> {
        let registry = Arc::new(Registry::new());
        let app = Router::new()
            .route("/healthz", get(|| async { StatusCode::OK }))
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(middleware::from_fn_with_state(
                registry.clone(),
                count_request,
            ));
        let main = axum::serve(self.main, app);
        if let Some(listener) = self.metrics {
            let app = Router::new()
                .route("/metrics", get(exposition))
                .with_state(registry);
            tokio::select! {
                result = main => result,
                result = axum::serve(listener, app) => result,
            }
        } else {
            main.await
        }
    }
}

async fn count_request(
    State(registry): State<Arc<Registry>>,
    request: Request,
    next: Next,
) -> Response {
    let method = Method::from(request.method().as_str());
    let start = Instant::now();
    let response = next.run(request).await;
    // O0 has only health and fallback routes, both in the other module.
    registry
        .requests(
            Module::Other,
            method,
            Code::try_from(response.status().as_u16()).unwrap(),
        )
        .add(1);
    registry.duration(Module::Other).observe(start.elapsed());
    response
}

async fn exposition(
    State(registry): State<Arc<Registry>>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    (
        [(header::CONTENT_TYPE, metrics::CONTENT_TYPE)],
        registry.exposition(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinHandle;
    use tokio::time::{Duration, timeout};

    struct Running(JoinHandle<io::Result<()>>);
    impl Drop for Running {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn request(addr: SocketAddr, method: &str, path: &str) -> String {
        timeout(Duration::from_secs(5), async {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(
                    format!(
                        "{method} {path} HTTP/1.1\r\nHost: example.org\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        })
        .await
        .unwrap()
    }

    fn config(metrics: bool) -> Config {
        Config::parse(if metrics {
            "[server]\nlisten = '127.0.0.1:0'\n[metrics]\nlisten = '127.0.0.1:0'"
        } else {
            "[server]\nlisten = '127.0.0.1:0'"
        })
        .unwrap()
    }

    #[tokio::test]
    async fn health_fallback_and_metrics_share_request_counts() {
        let server = Server::bind(&config(true)).await.unwrap();
        let main = server.main.local_addr().unwrap();
        let metrics = server.metrics.as_ref().unwrap().local_addr().unwrap();
        assert_ne!(main, metrics);
        let _running = Running(tokio::spawn(server.run()));
        let before = request(metrics, "GET", "/metrics").await;
        assert!(before.starts_with("HTTP/1.1 200"));
        assert!(before.contains(&format!("content-type: {}\r\n", metrics::CONTENT_TYPE)));
        let counter = "oroshi_requests_total{module=\"other\",method=\"GET\",code=\"4xx\"}";
        assert!(before.contains(&format!("{counter} 0\n")));
        let health = request(main, "GET", "/healthz").await;
        assert!(health.starts_with("HTTP/1.1 200"));
        assert_eq!(health.split_once("\r\n\r\n").unwrap().1, "");
        for path in ["/unknown", "/nix/unknown", "/metrics"] {
            assert!(request(main, "GET", path).await.starts_with("HTTP/1.1 404"));
        }
        assert!(
            request(main, "POST", "/healthz")
                .await
                .starts_with("HTTP/1.1 405")
        );
        assert!(
            request(main, "HEAD", "/healthz")
                .await
                .starts_with("HTTP/1.1 200")
        );
        let after = request(metrics, "GET", "/metrics").await;
        assert!(after.contains(&format!("{counter} 3\n")));
        assert!(
            after.contains(
                "oroshi_requests_total{module=\"other\",method=\"GET\",code=\"2xx\"} 1\n"
            )
        );
        assert!(
            after.contains(
                "oroshi_requests_total{module=\"other\",method=\"HEAD\",code=\"2xx\"} 1\n"
            )
        );
        assert!(
            after.contains(
                "oroshi_requests_total{module=\"other\",method=\"other\",code=\"4xx\"} 1\n"
            )
        );
        assert!(after.contains("oroshi_request_duration_seconds_count{module=\"other\"} 6\n"));
        assert!(
            request(metrics, "GET", "/healthz")
                .await
                .starts_with("HTTP/1.1 404")
        );
    }

    #[tokio::test]
    async fn absent_metrics_configuration_binds_only_main_listener() {
        let server = Server::bind(&config(false)).await.unwrap();
        assert!(server.metrics.is_none());
        let main = server.main.local_addr().unwrap();
        let _running = Running(tokio::spawn(server.run()));
        assert!(
            request(main, "GET", "/healthz")
                .await
                .starts_with("HTTP/1.1 200")
        );
        assert!(
            request(main, "GET", "/metrics")
                .await
                .starts_with("HTTP/1.1 404")
        );
    }

    #[tokio::test]
    async fn occupied_listener_addresses_fail_startup() {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = config(false);
        config.server.listen = occupied.local_addr().unwrap();
        assert!(Server::bind(&config).await.is_err());
        config.server.listen = "127.0.0.1:0".parse().unwrap();
        config.metrics.listen = Some(occupied.local_addr().unwrap());
        assert!(Server::bind(&config).await.is_err());
    }
}
