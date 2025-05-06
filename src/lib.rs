use std::{
    convert::Infallible, error::Error, future::Future, net::{IpAddr, SocketAddr}, sync::Arc, time::Instant
};

use hyper::{body::{Body, Incoming}, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use prometheus::{IntCounter, IntGauge};
use tokio::{net::TcpListener, sync::Semaphore};

/* re-export for downstream */
pub use bytes;
pub use http_body_util::{BodyExt, Full};
pub use hyper::{Request, Response, body, StatusCode, header, Error as HyperError};

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
    metrics: Option<Arc<HttpServerMetrics>>,
}

pub struct HttpServerMetrics {
    pub tcp_accepts: IntCounter,
    pub tcp_waiting: IntGauge,
    pub tcp_sessions: IntGauge,
    pub tcp_duration_ms: IntCounter,
    pub http_requests: IntCounter,
    pub http_sessions: IntGauge,
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
            metrics: None,
        }
    }

    pub fn set_metrics(&mut self, metrics: HttpServerMetrics) {
        self.metrics.replace(Arc::new(metrics));
    }

    pub async fn start(self, listen_addr: SocketAddr) -> std::io::Result<()> {
        #[cfg(feature = "tls")]
        let tls = Arc::new(if let Some(tls) = &self.settings.tls {
            tls_friend::install_crypto();

            let acceptor = match tls {
                HttpTls::WithBytes { cert, key } => tls_friend::tls_setup::TlsSetup::build_server(cert, key),
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
                Err(e) => {
                    eprintln!("failed to accept connection: {e}");
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
                let _parallel_guard = match sem {
                    Some(v) => {
                        let _wait_metric =
                            metrics.as_ref().map(|v| ActiveGauge::new(&v.tcp_waiting));
                        Some(v.acquire_owned().await)
                    }
                    None => None,
                };

                if let Some(metrics) = &metrics {
                    metrics.tcp_accepts.inc()
                };
                let _session_metric = metrics.as_ref().map(|v| ActiveGauge::new(&v.tcp_sessions));
                let _duration_metric = metrics
                    .as_ref()
                    .map(|v| DurationInc::new(&v.tcp_duration_ms));

                let mut builder = Builder::new(TokioExecutor::new());
                builder.http1()
                    .keep_alive(self.settings.keep_alive);

                let handle = |req: Request<Incoming>| {
                    let handler = handler.clone();

                    if let Some(metrics) = &metrics {
                        metrics.http_requests.inc()
                    };

                    let _http_session_metric =
                        metrics.as_ref().map(|v| ActiveGauge::new(&v.http_sessions));

                    let source_ip = if let Some(true_ip_header) = &*true_ip_header {
                        req
                            .headers()
                            .get(true_ip_header)
                            .and_then(|ip| ip.to_str().ok())
                            .and_then(|ip| ip.parse::<IpAddr>().ok())
                            .unwrap_or_else(|| addr.ip())
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
                }
            });
        }
    }
}

struct ActiveGauge(IntGauge);

impl ActiveGauge {
    pub fn new(gauge: &IntGauge) -> Self {
        let gauge = gauge.clone();
        gauge.inc();
        ActiveGauge(gauge)
    }
}

impl Drop for ActiveGauge {
    fn drop(&mut self) {
        self.0.dec();
    }
}

struct DurationInc {
    start: Instant,
    count: IntCounter,
}

impl DurationInc {
    pub fn new(count: &IntCounter) -> Self {
        let count = count.clone();

        DurationInc {
            start: Instant::now(),
            count,
        }
    }
}

impl Drop for DurationInc {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed().as_millis() as u64;
        self.count.inc_by(elapsed as _);
    }
}
