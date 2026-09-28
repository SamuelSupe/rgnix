//! Diagnostic baseline for separating Pingora transport cost from rgnix policy
//! processing. This omits product routing, budgets and telemetry; it is not a
//! supported server mode.
use async_trait::async_trait;
use clap::Parser;
use pingora::{http::RequestHeader, prelude::*, server::configuration::ServerConf};
use std::{
    hash::{Hash, Hasher},
    net::SocketAddr,
    time::Duration,
};

#[derive(Parser)]
#[command(version)]
struct Options {
    #[arg(long)]
    listen: String,
    #[arg(long)]
    upstream: SocketAddr,
    #[arg(long, default_value_t = 2)]
    workers: usize,
}

struct Proxy(SocketAddr);

#[async_trait]
impl ProxyHttp for Proxy {
    type CTX = ();
    fn new_ctx(&self) {}
    async fn upstream_peer(&self, _: &mut Session, _: &mut ()) -> Result<Box<HttpPeer>> {
        let mut peer = HttpPeer::new(self.0, false, "localhost".into());
        peer.options.set_http_version(1, 1);
        peer.options.connection_timeout = Some(Duration::from_secs(5));
        peer.options.read_timeout = Some(Duration::from_secs(30));
        peer.options.write_timeout = Some(Duration::from_secs(30));
        peer.options.idle_timeout = Some(Duration::from_secs(60));
        let mut key = std::collections::hash_map::DefaultHasher::new();
        std::thread::current().id().hash(&mut key);
        peer.group_key = key.finish();
        Ok(Box::new(peer))
    }
    async fn upstream_request_filter(
        &self,
        _: &mut Session,
        request: &mut RequestHeader,
        _: &mut (),
    ) -> Result<()> {
        request.insert_header("Host", "localhost")?;
        request.remove_header("Connection");
        Ok(())
    }
}

fn main() {
    let options = Options::parse();
    assert!(options.workers > 0);
    let conf = ServerConf {
        threads: options.workers,
        work_stealing: false,
        max_retries: 1,
        grace_period_seconds: Some(0),
        graceful_shutdown_timeout_seconds: Some(5),
        max_blocking_threads: Some((16 / options.workers).clamp(1, 2)),
        ..Default::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, conf);
    server.bootstrap();
    let mut service =
        pingora::proxy::http_proxy_service(&server.configuration, Proxy(options.upstream));
    service.add_tcp(&options.listen);
    server.add_service(service);
    server.run_forever();
}
