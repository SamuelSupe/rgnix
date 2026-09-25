use anyhow::{Result, ensure};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, clap::Args)]
pub struct Forwarding {
    #[arg(long, value_delimiter = ',')]
    pub trusted_proxy: Vec<IpNet>,
    #[arg(long, default_value = "x-forwarded-for")]
    pub real_ip_header: String,
    #[arg(long)]
    pub real_ip_recursive: bool,
    #[arg(long)]
    pub proxy_protocol: bool,
}
impl Forwarding {
    pub fn policy(&self) -> Result<IdentityPolicy> {
        let policy = IdentityPolicy {
            trusted: self.trusted_proxy.clone(),
            header: self.real_ip_header.to_ascii_lowercase(),
            recursive: self.real_ip_recursive,
            access: vec![],
        };
        policy.validate()?;
        ensure!(
            !self.proxy_protocol || !self.trusted_proxy.is_empty(),
            "PROXY protocol requires at least one --trusted-proxy CIDR"
        );
        Ok(policy)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IdentityPolicy {
    pub trusted: Vec<IpNet>,
    pub header: String,
    pub recursive: bool,
    pub access: Vec<AccessRule>,
}

impl Default for IdentityPolicy {
    fn default() -> Self {
        Self {
            trusted: vec![],
            header: "x-real-ip".into(),
            recursive: false,
            access: vec![],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AccessRule {
    pub network: Option<IpNet>,
    pub allow: bool,
}

impl IdentityPolicy {
    pub fn trusts(&self, ip: IpAddr) -> bool {
        self.trusted.iter().any(|net| net.contains(&ip))
    }
    pub fn allows(&self, ip: IpAddr) -> bool {
        self.access
            .iter()
            .find(|r| r.network.is_none_or(|net| net.contains(&ip)))
            .is_none_or(|r| r.allow)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.trusted.len() <= 256 && self.access.len() <= 256,
            "too many trusted proxies or access rules (maximum 256)"
        );
        http::header::HeaderName::from_bytes(self.header.as_bytes())?;
        Ok(())
    }

    /// Only a trusted socket peer may assert another identity. A malformed chain
    /// is discarded as a whole, so it cannot bypass a trusted-hop boundary.
    pub fn resolve(
        &self,
        peer: IpAddr,
        headers: &http::HeaderMap,
        proxy: Option<IpAddr>,
    ) -> IpAddr {
        if !self.trusts(peer) {
            return peer;
        }
        if self.header == "proxy_protocol" {
            return proxy.unwrap_or(peer);
        }
        let mut chain = Vec::new();
        let mut bytes = 0;
        for value in headers.get_all(&self.header) {
            bytes += value.len();
            if bytes > 8192 {
                return peer;
            }
            let Ok(value) = value.to_str() else {
                return peer;
            };
            for hop in value.split(',') {
                let value = if self.header == "forwarded" {
                    let mut fields = hop
                        .split(';')
                        .filter_map(|p| p.trim().split_once('='))
                        .filter(|(k, _)| k.eq_ignore_ascii_case("for"));
                    let Some((_, value)) = fields.next() else {
                        return peer;
                    };
                    if fields.next().is_some() {
                        return peer;
                    }
                    value.trim()
                } else {
                    hop.trim()
                };
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                let Some(address) = parse_address(value) else {
                    return peer;
                };
                chain.push(address);
                if chain.len() > 64 {
                    return peer;
                }
            }
        }
        if !self.recursive {
            return chain.last().copied().unwrap_or(peer);
        }
        let mut selected = peer;
        for ip in chain.into_iter().rev() {
            if !self.trusts(selected) {
                break;
            }
            selected = ip;
        }
        selected
    }

    pub fn scheme<'a>(&self, peer: IpAddr, headers: &'a http::HeaderMap, tls: bool) -> &'a str {
        if self.trusts(peer) {
            let mut values = headers.get_all("x-forwarded-proto").iter();
            if let Some(value) = values.next().and_then(|v| v.to_str().ok())
                && values.next().is_none()
                && matches!(value, "http" | "https")
            {
                return value;
            }
        }
        if tls { "https" } else { "http" }
    }
}

fn parse_address(value: &str) -> Option<IpAddr> {
    value
        .parse()
        .ok()
        .or_else(|| value.parse::<SocketAddr>().ok().map(|s| s.ip()))
        .or_else(|| value.strip_prefix('[')?.strip_suffix(']')?.parse().ok())
}

pub fn network(value: &str) -> Result<IpNet> {
    value
        .parse()
        .or_else(|_| value.parse::<IpAddr>().map(IpNet::from))
        .map_err(Into::into)
}
