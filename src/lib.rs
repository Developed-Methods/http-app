use std::{
    convert::Infallible, error::Error, future::Future, net::{IpAddr, SocketAddr}, sync::Arc, time::Instant
};

use arc_metrics::{helpers::{ActiveGauge, DurationIncMs, RegisterableMetric}, IntCounter, IntGauge};
use hyper::{body::{Body, Incoming}, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
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
}

impl RegisterableMetric for HttpServerMetrics {
    fn register(&'static self, register: &mut arc_metrics::RegisterAction) {
        register.gauge("connections", &self.tcp_waiting)
            .attr("status", "waiting");
        register.gauge("connections", &self.tcp_sessions)
            .attr("status", "active");

        register.count("blocked_waiting_count", &self.tcp_blocked_waiting_count);
        register.count("blocked_waiting_duration_ms", &self.tcp_blocked_waiting_duration_ms);

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
    }
}

pub struct HttpServerSettings {
    pub max_parallel: Option<usize>,
    pub true_ip_header: Option<String>,
    pub keep_alive: bool,
    pub with_upgrades: bool,
    #[cfg(feature = "tls")]
    pub tls: Option<HttpTls>,
}

#[cfg(feature = "tls")]
pub enum HttpTls {
    WithBytes { cert: Vec<u8>, key: Vec<u8> },
    WithPemPath { path: String },
}

impl Default for HttpServerSettings {
    fn default() -> Self {
        Self {
            max_parallel: Some(200),
            true_ip_header: None,
            keep_alive: true,
            with_upgrades: false,
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
        #[cfg(feature = "tls")]
        let tls = Arc::new(if let Some(tls) = &self.settings.tls {
            tls_friend::install_crypto();

            let acceptor = match tls {
                HttpTls::WithBytes { cert, key } => tls_friend::tls_setup::TlsSetup::build_server(key, cert),
                HttpTls::WithPemPath { path } => tls_friend::tls_setup::TlsSetup::load_server(&path).await,
            }?.into_acceptor()?;

            Some(acceptor)
        } else { None });

        tracing::info!(%listen_addr, "starting http server");

        let metrics = self.metrics;
        let tcp_listener = TcpListener::bind(listen_addr).await?;
        let sem = self
            .settings
            .max_parallel
            .map(|v| Arc::new(Semaphore::new(v)));

        let true_ip_header = Arc::new(self.settings.true_ip_header);

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
                    continue;
                }
            };

            let sem = sem.clone();
            let metrics = metrics.clone();
            let handler = self.handler.clone();
            let true_ip_header = true_ip_header.clone();

            #[cfg(feature = "tls")]
            let tls = tls.clone();

            tokio::spawn(async move {
                let _parallel_guard = 'block: {
                    let Some(sem) = sem.clone() else { break 'block None };

                    /* try non blocking first so we only update metrics if we're block */
                    if let Ok(guard) = Arc::clone(&sem).try_acquire_owned() {
                        break 'block Some(guard);
                    }

                    metrics.tcp_blocked_waiting_count.inc();
                    let _waiting_count = ActiveGauge::new(&metrics, |m| &m.tcp_waiting);
                    let _waiting_duration = DurationIncMs::new(&metrics, |m| &m.tcp_blocked_waiting_duration_ms);
                    let guard = sem.acquire_owned().await.expect("Semaphore closed?");

                    Some(guard)
                };

                metrics.tcp_accepts.inc();

                let _session_metric = ActiveGauge::new(&metrics, |m| &m.tcp_sessions);
                let _duration_metric = DurationIncMs::new(&metrics, |m| &m.tcp_duration_ms);

                let mut builder = Builder::new(TokioExecutor::new());
                builder.http1().keep_alive(self.settings.keep_alive);

                let handle = |req: Request<Incoming>| {
                    let handler = handler.clone();

                    metrics.http_requests.inc();
                    let _http_session_metric = ActiveGauge::new(&metrics, |m| &m.http_sessions);

                    let source_ip = if let Some(true_ip_header) = &*true_ip_header {
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
                        let res = handler.handle_request(source_ip, req).await;
                        Ok::<_, Infallible>(res)
                    }
                };

                #[cfg(feature = "tls")]
                let stream = match &*tls {
                    Some(tls) => tls_friend::tls_streams::ServerStream::TlsStream({
                        match tls.accept(stream).await {
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

                let result = if self.settings.with_upgrades {
                    builder.serve_connection_with_upgrades(TokioIo::new(stream), service_fn(handle)).await
                } else {
                    builder.serve_connection(TokioIo::new(stream), service_fn(handle)).await
                };

                if let Err(e) = result {
                    tracing::error!(?e, %addr, "failed to serve request");
                    metrics.http_serve_errors.inc();
                }
            });
        }
    }
}

