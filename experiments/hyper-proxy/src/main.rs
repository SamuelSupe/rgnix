use std::{convert::Infallible, net::SocketAddr, time::Duration};

use bytes::Bytes;
use clap::Parser;
use http_body_util::{Either, Full};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode, Uri,
    body::Incoming,
    header::{CONNECTION, HOST, HeaderName, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo, TokioTimer},
};
use tokio::{net::TcpListener, task::JoinSet};

#[derive(Parser)]
#[command(
    version,
    about = "Experimental HTTP/1 transport; not a production rgnix server"
)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    #[arg(long)]
    upstream: SocketAddr,
    #[arg(long, default_value_t = 1)]
    workers: usize,
}

type ProxyClient = Client<HttpConnector, Incoming>;
type ProxyBody = Either<Incoming, Full<Bytes>>;

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    anyhow::ensure!(args.workers > 0, "workers must be positive");
    let mut runtime = if args.workers == 1 {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(args.workers);
        builder
    };
    runtime.enable_all().build()?.block_on(serve(args))
}

async fn serve(args: Args) -> anyhow::Result<()> {
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    connector.set_connect_timeout(Some(Duration::from_secs(5)));
    let client = Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(512)
        .retry_canceled_requests(false)
        .build(connector);
    let upstream = args
        .upstream
        .to_string()
        .parse::<hyper::http::uri::Authority>()?;
    let listener = TcpListener::bind(args.listen).await?;
    let mut connections = JoinSet::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                stream.set_nodelay(true)?;
                let client = client.clone();
                let upstream = upstream.clone();
                connections.spawn(async move {
                    let service = service_fn(move |request| forward(request, client.clone(), upstream.clone()));
                    // Hyper owns each connection's parser and buffers across keepalive requests.
                    let _ = http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(60))
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        }
    }
    // Bounded drain for experiments; idle keepalive connections are aborted at the deadline.
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    Ok(())
}

async fn forward(
    mut request: Request<Incoming>,
    client: ProxyClient,
    upstream: hyper::http::uri::Authority,
) -> Result<Response<ProxyBody>, Infallible> {
    if request.method() == Method::CONNECT || request.headers().contains_key("upgrade") {
        return Ok(reply(
            StatusCode::NOT_IMPLEMENTED,
            "upgrade is outside this experiment\n",
        ));
    }
    let mut parts = request.uri().clone().into_parts();
    parts.scheme = Some(hyper::http::uri::Scheme::HTTP);
    parts.authority = Some(upstream);
    *request.uri_mut() = match Uri::from_parts(parts) {
        Ok(uri) => uri,
        Err(_) => return Ok(reply(StatusCode::BAD_REQUEST, "invalid target\n")),
    };
    strip_hop_headers(request.headers_mut());
    request
        .headers_mut()
        .insert(HOST, HeaderValue::from_static("localhost"));
    // There is no application retry. Also disable the client's stale-pool retry above.
    match tokio::time::timeout(Duration::from_secs(5), client.request(request)).await {
        Ok(Ok(mut response)) => {
            strip_hop_headers(response.headers_mut());
            Ok(response.map(Either::Left))
        }
        Ok(Err(_)) => Ok(reply(StatusCode::BAD_GATEWAY, "upstream failed\n")),
        Err(_) => Ok(reply(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream header timeout\n",
        )),
    }
}

fn reply(status: StatusCode, message: &'static str) -> Response<ProxyBody> {
    let mut response = Response::new(Either::Right(Full::new(Bytes::from_static(
        message.as_bytes(),
    ))));
    *response.status_mut() = status;
    response
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    // Connection can occur more than once; all nominated hop-local fields must be removed.
    let nominated: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}
