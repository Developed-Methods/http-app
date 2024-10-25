use std::{net::IpAddr, sync::Arc};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{body::Incoming, Request, Response};
use http_app::{HttpServer, HttpServerHandler, HttpServerSettings};

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
    let server = HttpServer::new(Arc::new(Server), HttpServerSettings::default());
    server.start("0.0.0.0:8080".parse().unwrap()).await.unwrap();
}
