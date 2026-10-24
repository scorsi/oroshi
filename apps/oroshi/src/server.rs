use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io;
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

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
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let shutdown = async move {
            tokio::select! {
                _ = terminate.recv() => {},
                _ = interrupt.recv() => {},
            }
        };
        let registry = Arc::new(Registry::new());
        let app = Router::new()
            .route("/healthz", get(|| async { StatusCode::OK }))
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(middleware::from_fn_with_state(
                registry.clone(),
                count_request,
            ));
        self.serve(app, registry, shutdown, std::env::var_os("NOTIFY_SOCKET"))
            .await
    }

    async fn serve(
        self,
        app: Router,
        registry: Arc<Registry>,
        shutdown: impl Future<Output = ()>,
        notify_socket: Option<OsString>,
    ) -> io::Result<()> {
        let (stop, mut main_stop) = watch::channel(false);
        let mut metrics_stop = main_stop.clone();
        let main = axum::serve(self.main, app).with_graceful_shutdown(async move {
            let _ = main_stop.changed().await;
        });
        let metrics = async move {
            if let Some(listener) = self.metrics {
                let app = Router::new()
                    .route("/metrics", get(exposition))
                    .with_state(registry);
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = metrics_stop.changed().await;
                    })
                    .await?;
            }
            Ok::<_, io::Error>(())
        };
        let servers = async { tokio::try_join!(main, metrics).map(|_| ()) };
        notify(notify_socket.as_deref(), "READY=1")?;
        tracing::info!(event = "ready");
        tokio::pin!(servers);
        tokio::select! {
            result = &mut servers => result,
            () = shutdown => {
                tracing::info!(event = "stopping");
                let notified = notify(notify_socket.as_deref(), "STOPPING=1");
                stop.send_replace(true);
                servers.await?;
                notified
            }
        }
    }
}

fn notify(socket: Option<&OsStr>, state: &str) -> io::Result<()> {
    let Some(socket) = socket else {
        return Ok(());
    };
    let datagram = UnixDatagram::unbound()?;
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::net::SocketAddr;

        if let Some(name) = socket.as_bytes().strip_prefix(b"@") {
            let addr = SocketAddr::from_abstract_name(name)?;
            return datagram.send_to_addr(state.as_bytes(), &addr).map(|_| ());
        }
    }
    datagram.send_to(state.as_bytes(), socket).map(|_| ())
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

    #[test]
    fn notification_without_socket_is_a_noop() {
        notify(None, "READY=1").unwrap();
        notify(None, "STOPPING=1").unwrap();
    }

    #[test]
    fn notification_to_missing_socket_reports_error() {
        assert!(notify(Some(OsStr::new("")), "READY=1").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn notifications_support_abstract_sockets() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::SocketAddr;

        let name = format!("oroshi-notify-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let receiver = UnixDatagram::bind_addr(&addr).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let socket = OsString::from(format!("@{name}"));
        for state in ["READY=1", "STOPPING=1"] {
            notify(Some(&socket), state).unwrap();
            let mut buffer = [0; 64];
            let size = receiver.recv(&mut buffer).unwrap();
            assert_eq!(&buffer[..size], state.as_bytes());
        }
    }

    #[tokio::test]
    async fn shutdown_drains_an_in_flight_request() {
        let server = Server::bind(&config(true)).await.unwrap();
        let main = server.main.local_addr().unwrap();
        let metrics = server.metrics.as_ref().unwrap().local_addr().unwrap();
        let (entered, mut started) = watch::channel(false);
        let (release, finished) = watch::channel(false);
        let app = Router::new().route(
            "/blocked",
            get(move || {
                let entered = entered.clone();
                let mut finished = finished.clone();
                async move {
                    entered.send_replace(true);
                    finished.changed().await.unwrap();
                    "completed"
                }
            }),
        );
        let (stop, mut shutdown) = watch::channel(false);
        let mut running = Running(tokio::spawn(server.serve(
            app,
            Arc::new(Registry::new()),
            async move { shutdown.changed().await.unwrap() },
            None,
        )));
        let response = tokio::spawn(async move { request(main, "GET", "/blocked").await });
        timeout(Duration::from_secs(5), started.changed())
            .await
            .unwrap()
            .unwrap();
        stop.send_replace(true);
        assert!(
            timeout(Duration::from_millis(50), &mut running.0)
                .await
                .is_err()
        );
        release.send_replace(true);
        let response = timeout(Duration::from_secs(5), response)
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.ends_with("completed"));
        timeout(Duration::from_secs(5), &mut running.0)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        for addr in [main, metrics] {
            assert!(tokio::net::TcpStream::connect(addr).await.is_err());
        }
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
