use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};

use arc_metrics::{helpers::RegisterableMetric, PromMetricRegistry, RegisterAction};
use bytes::Bytes;
use http_body_util::Full;
use hyper::StatusCode;

use crate::HttpServerHandler;

pub struct PromMetricsServer {
    init: PromMetricRegistry,
    rw_lock: std::sync::RwLock<PromMetricRegistry>,
    async_rw_lock: tokio::sync::RwLock<PromMetricRegistry>,
    last_write_size: AtomicUsize,
}

impl PromMetricsServer {
    pub fn new(reg: PromMetricRegistry) -> Arc<Self> {
        Arc::new(PromMetricsServer {
            init: reg,
            rw_lock: std::sync::RwLock::new(PromMetricRegistry::new()),
            async_rw_lock: tokio::sync::RwLock::new(PromMetricRegistry::new()),
            last_write_size: AtomicUsize::new(0),
        })
    }

    pub fn register_fn<T: 'static>(&self, metrics: &Arc<T>, register: impl FnOnce(&'static T, &mut RegisterAction<'_>)) {
        let mut lock = self.rw_lock.write().expect("failed to lock");
        lock.register_fn(metrics, register);
    }

    pub async fn register_async_fn<T: 'static>(&self, metrics: &Arc<T>, register: impl FnOnce(&'static T, &mut RegisterAction<'_>)) {
        let mut lock = self.async_rw_lock.write().await;
        lock.register_fn(metrics, register);
    }

    pub fn register<T: RegisterableMetric + 'static>(&self, metrics: &Arc<T>) {
        let mut lock = self.rw_lock.write().expect("failed to lock");
        lock.register(metrics);
    }

    pub async fn register_async<T: RegisterableMetric + 'static>(&self, metrics: &Arc<T>) {
        let mut lock = self.async_rw_lock.write().await;
        lock.register(metrics);
    }

    pub async fn write<T: std::io::Write>(&self, f: &mut T) -> std::io::Result<()> {
        write!(f, "{}", self.init)?;
        write!(f, "{}", self.rw_lock.read().expect("failed to read lock"))?;
        write!(f, "{}", self.async_rw_lock.read().await)?;
        Ok(())
    }
}

impl HttpServerHandler for PromMetricsServer {
    type Body = Full<Bytes>;

    async fn handle_request(self: Arc<Self>, _source: std::net::IpAddr, _request: hyper::Request<hyper::body::Incoming>) -> hyper::Response<Self::Body> {
        let mut out = Vec::with_capacity(self.last_write_size.load(Ordering::Relaxed));

        if let Err(error) = self.write(&mut out).await {
            tracing::error!(?error, "failed to write prom metrics string");

            return hyper::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::new(Bytes::from_static("prom metrics serialization error".as_bytes())))
                .unwrap();
        }

        self.last_write_size.store(out.len(), Ordering::Relaxed);
        hyper::Response::new(Full::new(Bytes::from(out)))
    }
}

