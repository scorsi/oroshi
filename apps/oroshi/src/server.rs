use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io;
use std::os::unix::net::UnixDatagram;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::HttpBody;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
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

    pub async fn run(self, config: &Config) -> io::Result<()> {
        let root = config.storage.dir.clone();
        let priority = config.nix.priority;
        let trusted_keys = config
            .nix
            .trusted_keys
            .iter()
            .map(|key| key.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?;
        let limits = crate::nix::UploadLimits::new(config);
        let (store, applied) = tokio::task::spawn_blocking(move || {
            let blobs = oroshi_store::BlobStore::open(&root)?;
            let (database, applied) =
                oroshi_store::Database::open(&root).map_err(io::Error::other)?;
            Ok::<_, io::Error>((
                Arc::new(crate::nix::Store {
                    blobs,
                    database,
                    priority,
                    trusted_keys,
                    limits,
                    uploads: Default::default(),
                }),
                applied,
            ))
        })
        .await
        .map_err(io::Error::other)??;
        log_migrations(&applied);
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let shutdown = async move {
            tokio::select! {
                _ = terminate.recv() => {},
                _ = interrupt.recv() => {},
            }
        };
        let registry = Arc::new(Registry::new());
        let app = app(store.clone(), registry.clone(), config);
        let metrics_app = Router::new()
            .route("/metrics", get(exposition))
            .with_state((registry, store));
        self.serve(
            app,
            metrics_app,
            shutdown,
            std::env::var_os("NOTIFY_SOCKET"),
        )
        .await
    }

    async fn serve(
        self,
        app: Router,
        metrics_app: Router,
        shutdown: impl Future<Output = ()>,
        notify_socket: Option<OsString>,
    ) -> io::Result<()> {
        let (stop, main_stop) = watch::channel(false);
        let metrics_stop = main_stop.clone();
        let main = serve_http(self.main, app, main_stop, Limits::default());
        let metrics = async move {
            if let Some(listener) = self.metrics {
                serve_http(listener, metrics_app, metrics_stop, Limits::default()).await?;
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

/// Connection limits that `axum::serve` (0.7) cannot set.
struct Limits {
    connections: usize,
    header_read_timeout: Duration,
    write_idle_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        // Each connection may hold a socket, a blob and a SQLite reader: keep well under LimitNOFILE.
        Self {
            connections: 1024,
            header_read_timeout: Duration::from_secs(30),
            // The upload idle timeout, applied to responses (ADR 0018).
            write_idle_timeout: crate::nix::UPLOAD_IDLE_TIMEOUT,
        }
    }
}

/// Serve HTTP/1 until `stop` changes, then drain in-flight connections.
async fn serve_http(
    listener: TcpListener,
    app: Router,
    mut stop: watch::Receiver<bool>,
    limits: Limits,
) -> io::Result<()> {
    let permits = Arc::new(tokio::sync::Semaphore::new(limits.connections));
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    loop {
        let accepted = tokio::select! {
            accepted = async {
                let permit = permits.clone().acquire_owned().await;
                (permit, listener.accept().await)
            } => accepted,
            _ = stop.changed() => break,
        };
        let (Ok(permit), Ok((stream, _))) = accepted else {
            // Accept errors such as EMFILE are transient: back off instead of exiting.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let connection = hyper::server::conn::http1::Builder::new()
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(limits.header_read_timeout)
            .serve_connection(
                hyper_util::rt::TokioIo::new(WriteIdle::new(stream, limits.write_idle_timeout)),
                hyper_util::service::TowerToHyperService::new(app.clone()),
            );
        let connection = graceful.watch(connection);
        tokio::spawn(async move {
            let _ = connection.await;
            drop(permit);
        });
    }
    drop(listener);
    graceful.shutdown().await;
    Ok(())
}

/// Fails a write that makes no progress for `idle`: a client that stops reading a NAR
/// download cannot hold its connection and open blob forever.
struct WriteIdle<S> {
    stream: S,
    idle: Duration,
    stalled: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> WriteIdle<S> {
    fn new(stream: S, idle: Duration) -> Self {
        Self {
            stream,
            idle,
            stalled: None,
        }
    }

    fn watch<T>(&mut self, cx: &mut Context<'_>, poll: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if poll.is_ready() {
            self.stalled = None;
            return poll;
        }
        let idle = self.idle;
        let stalled = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(idle)));
        match stalled.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "write idle timeout",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for WriteIdle<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for WriteIdle<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.stream).poll_write(cx, buf);
        self.watch(cx, poll)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let poll = Pin::new(&mut self.stream).poll_write_vectored(cx, bufs);
        self.watch(cx, poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let poll = Pin::new(&mut self.stream).poll_flush(cx);
        self.watch(cx, poll)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

pub(crate) fn lock_state(root: &std::path::Path) -> io::Result<std::fs::File> {
    let missing: Vec<_> = root
        .ancestors()
        .take_while(|path| !path.exists())
        .filter(|path| !path.as_os_str().is_empty())
        .collect();
    std::fs::create_dir_all(root)?;
    for path in missing.iter().rev() {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        std::fs::File::open(parent)?.sync_all()?;
    }
    check_state_dir(root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("oroshi.lock"))?;
    lock.try_lock()
        .map_err(|error| io::Error::other(format!("state directory lock: {error}")))?;
    Ok(lock)
}

/// Refuse a state tree that another user could redirect or rewrite (ADR 0014): the
/// directory and each managed subdirectory that exists must be a real directory owned by
/// this process's user and not writable by group or others.
pub(crate) fn check_state_dir(root: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let uid = process_uid()?;
    for name in ["", "tmp", "db", "blobs", "blobs/sha256"] {
        let path = if name.is_empty() {
            root.to_owned()
        } else {
            root.join(name)
        };
        let metadata = match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound && !name.is_empty() => continue,
            result => result?,
        };
        let problem = if !metadata.is_dir() {
            "is a symlink or not a directory"
        } else if metadata.uid() != uid {
            "is not owned by the service user"
        } else if metadata.mode() & 0o022 != 0 {
            "is group- or world-writable"
        } else {
            continue;
        };
        // Name the managed path, never the configured value.
        let label = if name.is_empty() { "" } else { "/" };
        return Err(io::Error::other(format!(
            "storage.dir{label}{name} {problem}"
        )));
    }
    Ok(())
}

/// The standard library exposes no geteuid: a file this process creates carries its uid.
fn process_uid() -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.subsec_nanos());
    let path = std::env::temp_dir().join(format!("oroshi-uid-{}-{nanos}", std::process::id()));
    let uid = std::fs::File::create_new(&path)?.metadata()?.uid();
    std::fs::remove_file(&path)?;
    Ok(uid)
}

fn log_migrations(applied: &[usize]) {
    for &version in applied {
        tracing::info!(event = "migration-applied", version);
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

fn app(store: Arc<crate::nix::Store>, registry: Arc<Registry>, config: &Config) -> Router {
    Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .nest(
            "/nix/",
            crate::nix::routes(
                store,
                Arc::new(crate::auth::Tokens::new(
                    config,
                    "nix:write",
                    Module::Nix,
                    registry.clone(),
                )),
                Arc::new(crate::auth::Tokens::new(
                    config,
                    "nix:pin",
                    Module::Nix,
                    registry.clone(),
                )),
                registry.clone(),
            ),
        )
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(middleware::from_fn_with_state(
            (registry, config.log.access),
            count_request,
        ))
}

async fn count_request(
    State((registry, access)): State<(Arc<Registry>, bool)>,
    request: Request,
    next: Next,
) -> Response {
    let method = Method::from(request.method().as_str());
    let read = matches!(request.method().as_str(), "GET" | "HEAD");
    let path = request.uri().path().to_owned();
    let module = if request
        .extensions()
        .get::<MatchedPath>()
        .is_some_and(|matched| matched.as_str().starts_with("/nix/"))
    {
        Module::Nix
    } else {
        Module::Other
    };
    let start = Instant::now();
    let response = next.run(request).await;
    registry
        .requests(
            module,
            method,
            Code::try_from(response.status().as_u16()).unwrap(),
        )
        .add(1);
    registry.duration(module).observe(start.elapsed());
    let bytes = if matches!(method, Method::Head) {
        0
    } else {
        response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .or_else(|| response.body().size_hint().exact())
            .unwrap_or(0)
    };
    registry.response_bytes(module).add(bytes);
    let refusal_logged = response
        .extensions()
        .get::<crate::auth::AuthRefused>()
        .is_some()
        || response
            .extensions()
            .get::<crate::nix::UploadRefused>()
            .is_some();
    if !refusal_logged
        && (access
            || !read
            || (response.status().is_client_error() && response.status() != StatusCode::NOT_FOUND)
            || response.status().is_server_error())
    {
        tracing::info!(
            event = "request",
            module = module.label(),
            method = method.label(),
            path,
            status = response.status().as_u16(),
            bytes,
            dur_us = start.elapsed().as_micros() as u64
        );
    }
    response
}

async fn exposition(
    State((registry, store)): State<(Arc<Registry>, Arc<crate::nix::Store>)>,
) -> Result<([(header::HeaderName, &'static str); 1], String), StatusCode> {
    let start = Instant::now();
    let ((bytes, blobs), gc) = tokio::task::spawn_blocking(move || {
        let totals = store.database.reader().and_then(|reader| {
            Ok(reader.query_row(
                "SELECT coalesce(sum(size), 0), count(*) FROM blob",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )?)
        })?;
        Ok::<_, oroshi_store::DatabaseError>((totals, store.database.gc_state()?))
    })
    .await
    .map_err(|error| metrics_failure(&error, start))?
    .map_err(|error| metrics_failure(&error, start))?;
    registry.stored_bytes.set(bytes);
    registry.blobs.set(blobs);
    registry.gc_runs_ok.set(gc.runs_success as i64);
    registry.gc_runs_error.set(gc.runs_error as i64);
    registry.gc_freed_bytes.set(gc.freed_bytes as i64);
    registry
        .gc_last_success
        .set(gc.last_success_at.unwrap_or(0));
    Ok((
        [(header::CONTENT_TYPE, metrics::CONTENT_TYPE)],
        registry.exposition(),
    ))
}

fn metrics_failure(error: &dyn std::fmt::Display, start: Instant) -> StatusCode {
    tracing::error!(event = "request", module = "other", method = "GET", path = "/metrics",
        status = 500_u16, bytes = 0_u64, dur_us = start.elapsed().as_micros() as u64, err = %error);
    StatusCode::INTERNAL_SERVER_ERROR
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
        let mut config = Config::parse(if metrics {
            "[server]\nlisten = '127.0.0.1:0'\n[metrics]\nlisten = '127.0.0.1:9000'"
        } else {
            "[server]\nlisten = '127.0.0.1:0'"
        })
        .unwrap();
        if metrics {
            config.metrics.listen.as_mut().unwrap().set_port(0);
        }
        config
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
            Router::new(),
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
    async fn idle_connections_time_out_and_release_their_connection_slot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_stop, stopped) = watch::channel(false);
        let app = Router::new().route("/healthz", get(|| async { StatusCode::OK }));
        let limits = Limits {
            connections: 1,
            header_read_timeout: Duration::from_millis(300),
            ..Limits::default()
        };
        let _running = Running(tokio::spawn(serve_http(listener, app, stopped, limits)));
        let start = Instant::now();
        let mut idle = tokio::net::TcpStream::connect(addr).await.unwrap();
        // The only slot is held by a client that never sends headers until the timeout closes it.
        let response = request(addr, "GET", "/healthz").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(start.elapsed() >= Duration::from_millis(300));
        let mut rest = Vec::new();
        timeout(Duration::from_secs(5), idle.read_to_end(&mut rest))
            .await
            .unwrap()
            .ok();
    }

    #[tokio::test]
    async fn health_fallback_and_metrics_share_request_counts() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = config(true);
        settings.storage.dir = directory.path().to_owned();
        let server = Server::bind(&settings).await.unwrap();
        let main = server.main.local_addr().unwrap();
        let metrics = server.metrics.as_ref().unwrap().local_addr().unwrap();
        assert_ne!(main, metrics);
        let _running = Running(tokio::spawn(async move { server.run(&settings).await }));
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
        assert!(after.contains(&format!("{counter} 2\n")));
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
        assert!(after.contains("oroshi_request_duration_seconds_count{module=\"other\"} 5\n"));
        assert!(
            request(metrics, "GET", "/healthz")
                .await
                .starts_with("HTTP/1.1 404")
        );
    }

    #[tokio::test]
    async fn absent_metrics_configuration_binds_only_main_listener() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = config(false);
        settings.storage.dir = directory.path().to_owned();
        let server = Server::bind(&settings).await.unwrap();
        assert!(server.metrics.is_none());
        let main = server.main.local_addr().unwrap();
        let _running = Running(tokio::spawn(async move { server.run(&settings).await }));
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

#[cfg(test)]
mod read_tests;

#[cfg(test)]
mod upload_tests;

#[cfg(test)]
mod narinfo_tests;

#[cfg(test)]
mod observability_tests;

#[cfg(test)]
mod roots_tests;

#[cfg(test)]
mod review_tests {
    use super::*;
    #[tokio::test]
    async fn stalled_writes_fail_after_the_idle_timeout_and_progress_resets_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let idle = Duration::from_millis(100);
        let (client, server) = tokio::io::duplex(64);
        let mut stalled = WriteIdle::new(server, idle);
        let started = Instant::now();
        let error = stalled.write_all(&[0; 1024]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= idle);
        drop(client);

        let (mut client, server) = tokio::io::duplex(64);
        let reader = tokio::spawn(async move {
            let mut total = 0;
            let mut buffer = [0; 64];
            loop {
                // Slower than the whole transfer would take, faster than the idle timeout.
                tokio::time::sleep(Duration::from_millis(20)).await;
                match client.read(&mut buffer).await.unwrap() {
                    0 => return total,
                    read => total += read,
                }
            }
        });
        let mut slow = WriteIdle::new(server, idle);
        slow.write_all(&[0; 1024]).await.unwrap();
        slow.shutdown().await.unwrap();
        drop(slow);
        assert_eq!(reader.await.unwrap(), 1024);
    }

    #[test]
    fn state_dir_refuses_symlinks_and_shared_write_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        drop(lock_state(&state).unwrap());
        let mode = |path: &std::path::Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        mode(&state, 0o770);
        assert!(
            lock_state(&state)
                .unwrap_err()
                .to_string()
                .contains("storage.dir is group")
        );
        mode(&state, 0o750);
        std::fs::create_dir(state.join("tmp")).unwrap();
        mode(&state.join("tmp"), 0o757);
        assert!(
            lock_state(&state)
                .unwrap_err()
                .to_string()
                .contains("storage.dir/tmp is")
        );
        mode(&state.join("tmp"), 0o700);
        std::os::unix::fs::symlink(root.path(), state.join("db")).unwrap();
        assert!(
            lock_state(&state)
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
        std::fs::remove_file(state.join("db")).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&state, &link).unwrap();
        assert!(lock_state(&link).is_err());
        assert!(lock_state(&state).is_ok());
    }

    #[test]
    fn state_lock_refuses_second_owner_without_cleaning_uploads_and_releases_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let lock = lock_state(root.path()).unwrap();
        let _blobs = oroshi_store::BlobStore::open(root.path()).unwrap();
        let upload = root.path().join("tmp/in-flight");
        std::fs::write(&upload, b"partial").unwrap();
        assert!(lock_state(root.path()).is_err());
        assert_eq!(std::fs::read(&upload).unwrap(), b"partial");
        drop(lock);
        assert!(lock_state(root.path()).is_ok());
    }
}
