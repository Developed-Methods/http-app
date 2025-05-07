use std::{net::IpAddr, sync::Arc};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{body::Incoming, Request, Response};
use http_app::{prom_metrics_server::PromMetricsServer, HttpServer, HttpServerHandler, HttpServerSettings};

struct Server;
impl HttpServerHandler for Server {
    type Body = Full<Bytes>;

    async fn handle_request(
        self: Arc<Self>,
        source: IpAddr,
        request: Request<Incoming>,
    ) -> Response<Full<Bytes>> {
        let (header, _body) = request.into_parts();
        Response::new(Full::new(Bytes::from(format!("hello world: {:?} - {}", source, header.uri.path()))))
    }
}

#[tokio::main]
async fn main() {
    pkg_details::init!();
    let server = HttpServer::new(Arc::new(Server), HttpServerSettings::default());

    let metrics_server = PromMetricsServer::new(Default::default());
    metrics_server.register(server.get_metrics());

    let server_handle = tokio::spawn(server.start("0.0.0.0:8080".parse().unwrap()));
    let metrics_handle = tokio::spawn(HttpServer::new(metrics_server, HttpServerSettings::default()).start("0.0.0.0:8081".parse().unwrap()));

    let _ = server_handle.await;
    let _ = metrics_handle.await;
}
