use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use http_app::{
    bytes::Bytes, body::Incoming, read_body_limited, Full, HttpServer, HttpServerHandler, HttpServerSettings,
    ReadBodyError, Request, Response, StatusCode,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

struct TestServer;

impl HttpServerHandler for TestServer {
    type Body = Full<Bytes>;

    async fn handle_request(self: Arc<Self>, _source: IpAddr, request: Request<Incoming>) -> Response<Full<Bytes>> {
        match request.uri().path() {
            "/body" => match read_body_limited(request.into_body(), 8).await {
                Ok(body) => Response::new(Full::new(Bytes::from(format!("len={}", body.len())))),
                Err(ReadBodyError::TooLarge) => Response::builder()
                    .status(StatusCode::PAYLOAD_TOO_LARGE)
                    .body(Full::default())
                    .unwrap(),
                Err(ReadBodyError::Body(error)) => panic!("body error: {}", error),
            },
            "/slow" => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Response::new(Full::new(Bytes::from_static(b"slow")))
            }
            _ => Response::new(Full::new(Bytes::from_static(b"ok"))),
        }
    }
}

async fn spawn_server(settings: HttpServerSettings) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(HttpServer::new(Arc::new(TestServer), settings).serve(listener));
    addr
}

async fn read_to_close(stream: &mut TcpStream) -> String {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await
    .expect("server did not close connection");
    String::from_utf8(out).unwrap()
}

async fn read_response_head(stream: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    while !out.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await.unwrap();
        assert_ne!(n, 0, "connection closed before response");
        out.extend_from_slice(&buf[..n]);
    }
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn body_limit_by_content_length_and_streamed_bytes() {
    let addr = spawn_server(HttpServerSettings::default()).await;

    let cases: [(&[u8], &str); 4] = [
        (b"POST /body HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 8\r\n\r\n12345678", "len=8"),
        (b"POST /body HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 9\r\n\r\n123456789", " 413 "),
        (
            b"POST /body HTTP/1.1\r\nHost: x\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n1234\r\n4\r\n5678\r\n0\r\n\r\n",
            "len=8",
        ),
        (
            b"POST /body HTTP/1.1\r\nHost: x\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n1234\r\n5\r\n56789\r\n0\r\n\r\n",
            " 413 ",
        ),
    ];

    for (request, expected) in cases {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request).await.unwrap();
        let response = read_to_close(&mut stream).await;
        assert!(response.contains(expected), "expected {:?} in {:?}", expected, response);
    }
}

#[tokio::test]
async fn rejects_connections_when_wait_queue_is_full() {
    let addr = spawn_server(HttpServerSettings {
        max_parallel: Some(1),
        max_waiting: 0,
        ..Default::default()
    })
    .await;

    /* hold the only slot with a keep-alive connection */
    let mut first = TcpStream::connect(addr).await.unwrap();
    first.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    assert!(read_response_head(&mut first).await.starts_with("HTTP/1.1 200"));

    let mut second = TcpStream::connect(addr).await.unwrap();
    let _ = second.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert_eq!(read_to_close(&mut second).await, "");
}

#[tokio::test]
async fn closes_connections_with_slow_headers() {
    let addr = spawn_server(HttpServerSettings {
        header_read_timeout: Some(Duration::from_millis(200)),
        ..Default::default()
    })
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();

    let start = Instant::now();
    read_to_close(&mut stream).await;
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn connection_timeout_lets_in_flight_request_finish() {
    let addr = spawn_server(HttpServerSettings {
        connection_timeout: Some(Duration::from_millis(200)),
        connection_grace_period: Duration::from_secs(5),
        ..Default::default()
    })
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();

    let response = read_to_close(&mut stream).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{:?}", response);
    assert!(response.ends_with("slow"), "{:?}", response);
}

#[tokio::test]
async fn connection_timeout_force_closes_after_grace_period() {
    let addr = spawn_server(HttpServerSettings {
        connection_timeout: Some(Duration::from_millis(100)),
        connection_grace_period: Duration::from_millis(100),
        ..Default::default()
    })
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();

    let start = Instant::now();
    assert_eq!(read_to_close(&mut stream).await, "");
    assert!(start.elapsed() < Duration::from_millis(450));
}
