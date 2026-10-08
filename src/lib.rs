use std::{
    convert::Infallible, error::Error, fmt, future::Future, net::{IpAddr, SocketAddr}, pin::{pin, Pin}, sync::Arc, time::Duration
};

use bytes::Buf;
use arc_metrics::{helpers::{ActiveGauge, DurationIncMs, RegisterableMetric}, IntCounter, IntGauge};
use hyper::{body::{Body, Incoming}, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto::Builder,
};
use tokio::{net::TcpListener, sync::Semaphore};

#[cfg(feature = "metrics-server")]
pub mod prom_metrics_server;

/* re-export for downstream */
pub use bytes;
pub use http_body_util::{BodyExt, Full};
pub use hyper::{self, Request, Response, body, StatusCode, header, Error as HyperError};

pub trait HttpServerHandler: Sync + Send + 'static {
    type Body: Body<Data: Send + Sync, Error: Into<Box<dyn Error + Send + Sync>>> + Send;

    fn handle_request(
        self: Arc<Self>,
        source: IpAddr,
        request: Request<Incoming>,
    ) -> impl Future<Output = Response<Self::Body>> + Send;
}

pub struct HttpServer<H: HttpServerHandler> {
    handler: Arc<H>,
    settings: HttpServerSettings,
    metrics: Arc<HttpServerMetrics>,
}

#[derive(Default)]
pub struct HttpServerMetrics {
    pub tcp_waiting: IntGauge,
    pub tcp_sessions: IntGauge,

    pub tcp_blocked_waiting_count: IntCounter,
    pub tcp_blocked_waiting_duration_ms: IntCounter,
    pub tcp_rejected: IntCounter,

    pub tcp_connection_timeouts: IntCounter,
    pub tcp_connection_force_closed: IntCounter,

    pub tcp_accepts: IntCounter,
    pub tcp_duration_ms: IntCounter,

    pub http_requests: IntCounter,
    pub http_sessions: IntGauge,

    pub tcp_accept_errors: IntCounter,
    pub tcp_accept_errors_too_many_files: IntCounter,
    pub true_ip_parse_errors: IntCounter,
    pub http_serve_errors: IntCounter,
    #[cfg(feature = "tls")]
    pub tls_accept_errors: IntCounter,
    #[cfg(feature = "tls")]
    pub tls_accept_timeouts: IntCounter,
}

impl RegisterableMetric for HttpServerMetrics {
    fn register(&'static self, register: &mut arc_metrics::RegisterAction) {
        register.gauge("connections", &self.tcp_waiting)
            .attr("status", "waiting");
        register.gauge("connections", &self.tcp_sessions)
            .attr("status", "active");

        register.count("blocked_waiting_count", &self.tcp_blocked_waiting_count);
        register.count("blocked_waiting_duration_ms", &self.tcp_blocked_waiting_duration_ms);
        register.count("rejected_connections", &self.tcp_rejected);

        register.count("connection_timeouts", &self.tcp_connection_timeouts).attr("stage", "graceful");
        register.count("connection_timeouts", &self.tcp_connection_force_closed).attr("stage", "forced");

        register.count("tcp_count", &self.tcp_accepts);
        register.count("tcp_duration_ms", &self.tcp_duration_ms);

        register.count("http_request_count", &self.http_requests);
        register.gauge("http_sessions", &self.http_sessions);

        register.count("accept_error", &self.tcp_accept_errors_too_many_files).attr("reason", "too_many_files");
        register.count("accept_error", &self.tcp_accept_errors).attr("reason", "other");

        register.count("errors", &self.true_ip_parse_errors).attr("type", "true_ip_parse");
        register.count("errors", &self.http_serve_errors).attr("type", "http_serve");
        #[cfg(feature = "tls")]
        register.count("errors", &self.tls_accept_errors).attr("type", "tls_accept");
        #[cfg(feature = "tls")]
        register.count("errors", &self.tls_accept_timeouts).attr("type", "tls_accept_timeout");
    }
}

#[derive(Clone)]
pub struct HttpServerSettings {
    /// Max connections being served at once.
    pub max_parallel: Option<usize>,
    /// Max accepted connections waiting for a `max_parallel` slot. Connections
    /// accepted while the queue is full are closed immediately.
    pub max_waiting: usize,
    pub true_ip_header: Option<String>,
    pub keep_alive: bool,
    pub with_upgrades: bool,
    /// Time allowed for the TLS handshake and for each set of HTTP/1 request
    /// headers. Also closes HTTP/1 keep-alive connections idle for this long.
    pub header_read_timeout: Option<Duration>,
    /// Max connection lifetime. When reached the connection stops accepting
    /// new requests (HTTP/1 closes after the current response, HTTP/2 sends
    /// GOAWAY) and in-flight requests get `connection_grace_period` to finish
    /// before the connection is dropped.
    pub connection_timeout: Option<Duration>,
    pub connection_grace_period: Duration,
    pub http2_max_concurrent_streams: Option<u32>,
    #[cfg(feature = "tls")]
    pub tls: Option<HttpTls>,
}

#[cfg(feature = "tls")]
#[derive(Clone)]
pub enum HttpTls {
    WithBytes { cert: Vec<u8>, key: Vec<u8> },
    WithPemPath { path: String },
}

impl Default for HttpServerSettings {
    fn default() -> Self {
        Self {
            max_parallel: Some(200),
            max_waiting: 100,
            true_ip_header: None,
            keep_alive: true,
            with_upgrades: false,
            header_read_timeout: Some(Duration::from_secs(30)),
            connection_timeout: Some(Duration::from_secs(300)),
            connection_grace_period: Duration::from_secs(30),
            http2_max_concurrent_streams: Some(32),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

impl<H: HttpServerHandler> HttpServer<H> {
    pub fn new(handler: Arc<H>, settings: HttpServerSettings) -> Self {
        HttpServer {
            handler,
            settings,
            metrics: Arc::new(HttpServerMetrics::default()),
        }
    }

    pub fn get_metrics(&self) -> &Arc<HttpServerMetrics> {
        &self.metrics
    }

    pub async fn start(self, listen_addr: SocketAddr) -> std::io::Result<()> {
        let tcp_listener = TcpListener::bind(listen_addr).await?;
        self.serve(tcp_listener).await
    }

    pub async fn serve(self, tcp_listener: TcpListener) -> std::io::Result<()> {
        #[cfg(feature = "tls")]
        let tls = Arc::new(if let Some(tls) = &self.settings.tls {
            tls_friend::install_crypto();

            let acceptor = match tls {
                HttpTls::WithBytes { cert, key } => tls_friend::tls_setup::TlsSetup::build_server(key, cert),
                HttpTls::WithPemPath { path } => tls_friend::tls_setup::TlsSetup::load_server(path).await,
            }?.into_acceptor()?;

            Some(acceptor)
        } else { None });

        tracing::info!(listen_addr = ?tcp_listener.local_addr(), "starting http server");

        let metrics = self.metrics;
        let settings = Arc::new(self.settings);
        let sem = settings
            .max_parallel
            .map(|v| Arc::new(Semaphore::new(v)));
        let waiting_sem = Arc::new(Semaphore::new(settings.max_waiting));

        loop {
            let (stream, addr) = match tcp_listener.accept().await {
                Ok(x) => x,
                Err(error) => {
                    let counter = 'counter: {
                        #[cfg(target_family = "unix")]
                        {
                            if let Some(24) = error.raw_os_error() {
                                break 'counter &metrics.tcp_accept_errors_too_many_files;
                            }
                        }
                        &metrics.tcp_accept_errors
                    };
                    counter.inc();

                    tracing::error!(?error, "tcp failed to accept");

                    /* avoid spinning while out of file descriptors */
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };

            /* bound the number of connections queued for a slot before spawning a task */
            let (parallel_guard, waiting_guard) = match &sem {
                None => (None, None),
                Some(sem) => match Arc::clone(sem).try_acquire_owned() {
                    Ok(guard) => (Some(guard), None),
                    Err(_) => match Arc::clone(&waiting_sem).try_acquire_owned() {
                        Ok(guard) => (None, Some(guard)),
                        Err(_) => {
                            metrics.tcp_rejected.inc();
                            continue;
                        }
                    },
                },
            };

            let sem = sem.clone();
            let metrics = metrics.clone();
            let handler = self.handler.clone();
            let settings = settings.clone();

            #[cfg(feature = "tls")]
            let tls = tls.clone();

            tokio::spawn(async move {
                let _parallel_guard = 'block: {
                    if parallel_guard.is_some() {
                        break 'block parallel_guard;
                    }
                    let Some(sem) = sem else { break 'block None };

                    metrics.tcp_blocked_waiting_count.inc();
                    let _waiting_count = ActiveGauge::new(&metrics, |m| &m.tcp_waiting);
                    let _waiting_duration = DurationIncMs::new(&metrics, |m| &m.tcp_blocked_waiting_duration_ms);
                    let guard = sem.acquire_owned().await.expect("Semaphore closed?");
                    drop(waiting_guard);

                    Some(guard)
                };

                metrics.tcp_accepts.inc();

                let _session_metric = ActiveGauge::new(&metrics, |m| &m.tcp_sessions);
                let _duration_metric = DurationIncMs::new(&metrics, |m| &m.tcp_duration_ms);

                let mut builder = Builder::new(TokioExecutor::new());
                builder.http1()
                    .timer(TokioTimer::new())
                    .keep_alive(settings.keep_alive)
                    .header_read_timeout(settings.header_read_timeout);
                builder.http2()
                    .timer(TokioTimer::new())
                    .max_concurrent_streams(settings.http2_max_concurrent_streams);

                let handle = |req: Request<Incoming>| {
                    let handler = handler.clone();
                    let metrics = metrics.clone();

                    metrics.http_requests.inc();

                    let source_ip = if let Some(true_ip_header) = &settings.true_ip_header {
                        let true_ip_opt = req
                            .headers()
                            .get(true_ip_header)
                            .and_then(|ip| ip.to_str().ok())
                            .and_then(|ip| ip.parse::<IpAddr>().ok());

                        match true_ip_opt {
                            Some(v) => v,
                            None => {
                                metrics.true_ip_parse_errors.inc();
                                addr.ip()
                            }
                        }
                    } else {
                        addr.ip()
                    };

                    async move {
                        let _http_session_metric = ActiveGauge::new(&metrics, |m| &m.http_sessions);
                        let res = handler.handle_request(source_ip, req).await;
                        Ok::<_, Infallible>(res)
                    }
                };

                #[cfg(feature = "tls")]
                let stream = match &*tls {
                    Some(tls) => tls_friend::tls_streams::ServerStream::TlsStream({
                        let accept = tls.accept(stream);
                        let accepted = match settings.header_read_timeout {
                            Some(timeout) => match tokio::time::timeout(timeout, accept).await {
                                Ok(v) => v,
                                Err(_) => {
                                    metrics.tls_accept_timeouts.inc();
                                    return;
                                }
                            },
                            None => accept.await,
                        };

                        match accepted {
                            Ok(v) => v,
                            Err(error) => {
                                tracing::error!(?error, "failed to accept new tls stream");
                                metrics.tls_accept_errors.inc();
                                return;
                            }
                        }
                    }),
                    None => tls_friend::tls_streams::ServerStream::TcpStream(stream),
                };

                let result = if settings.with_upgrades {
                    let conn = pin!(builder.serve_connection_with_upgrades(TokioIo::new(stream), service_fn(handle)));
                    drive_connection(conn, |conn| conn.graceful_shutdown(), &settings, &metrics).await
                } else {
                    let conn = pin!(builder.serve_connection(TokioIo::new(stream), service_fn(handle)));
                    drive_connection(conn, |conn| conn.graceful_shutdown(), &settings, &metrics).await
                };

                if let Err(e) = result {
                    tracing::error!(?e, %addr, "failed to serve request");
                    metrics.http_serve_errors.inc();
                }
            });
        }
    }
}

async fn drive_connection<C>(
    mut conn: Pin<&mut C>,
    graceful_shutdown: impl FnOnce(Pin<&mut C>),
    settings: &HttpServerSettings,
    metrics: &HttpServerMetrics,
) -> Result<(), Box<dyn Error + Send + Sync>>
where
    C: Future<Output = Result<(), Box<dyn Error + Send + Sync>>>,
{
    let Some(timeout) = settings.connection_timeout else {
        return conn.await;
    };

    if let Ok(result) = tokio::time::timeout(timeout, conn.as_mut()).await {
        return result;
    }

    metrics.tcp_connection_timeouts.inc();
    graceful_shutdown(conn.as_mut());

    match tokio::time::timeout(settings.connection_grace_period, conn).await {
        Ok(result) => result,
        Err(_) => {
            metrics.tcp_connection_force_closed.inc();
            Ok(())
        }
    }
}

#[derive(Debug)]
pub enum ReadBodyError<E> {
    Body(E),
    TooLarge,
}

impl<E: fmt::Display> fmt::Display for ReadBodyError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadBodyError::Body(error) => write!(f, "failed to read body: {}", error),
            ReadBodyError::TooLarge => write!(f, "body too large"),
        }
    }
}

impl<E: Error + 'static> Error for ReadBodyError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ReadBodyError::Body(error) => Some(error),
            ReadBodyError::TooLarge => None,
        }
    }
}

/// Reads a body into memory, failing with [`ReadBodyError::TooLarge`] once more than
/// `max_bytes` would be buffered. A `Content-Length` over the limit is rejected
/// before anything is read. Callers should also wrap this in a timeout to bound
/// slow uploads.
pub async fn read_body_limited<B>(body: B, max_bytes: usize) -> Result<Vec<u8>, ReadBodyError<B::Error>>
where
    B: Body,
{
    let size_hint = body.size_hint();
    if size_hint.lower() > max_bytes as u64 {
        return Err(ReadBodyError::TooLarge);
    }

    let initial_capacity = size_hint.upper().unwrap_or(0).min(max_bytes as u64) as usize;
    let mut out = Vec::with_capacity(initial_capacity);

    let mut body = pin!(body);
    while let Some(frame) = body.frame().await {
        let Ok(mut data) = frame.map_err(ReadBodyError::Body)?.into_data() else {
            continue;
        };

        if max_bytes - out.len() < data.remaining() {
            return Err(ReadBodyError::TooLarge);
        }

        while data.has_remaining() {
            let chunk = data.chunk();
            out.extend_from_slice(chunk);
            let len = chunk.len();
            data.advance(len);
        }
    }

    Ok(out)
}
