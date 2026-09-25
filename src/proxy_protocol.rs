use async_trait::async_trait;
use pingora::{
    listeners::PreTlsProcess,
    protocols::{GetSocketDigest, l4::stream::Stream},
};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::io::AsyncReadExt;

pub struct ProxyProtocol {
    pub trusted: Vec<ipnet::IpNet>,
}
#[async_trait]
impl PreTlsProcess for ProxyProtocol {
    async fn process(&self, stream: &mut Stream) -> pingora::Result<()> {
        let fail =
            |message| pingora::Error::explain(pingora::ErrorType::InvalidHTTPHeader, message);
        let digest = stream
            .get_socket_digest()
            .ok_or_else(|| fail("PROXY protocol requires a TCP socket"))?;
        let peer = digest
            .peer_addr()
            .and_then(|a| a.as_inet())
            .ok_or_else(|| fail("missing PROXY socket peer"))?;
        if !self
            .trusted
            .iter()
            .any(|network| network.contains(&peer.ip()))
        {
            return Err(fail("untrusted PROXY protocol sender"));
        }
        let address = tokio::time::timeout(Duration::from_secs(2), read(stream))
            .await
            .map_err(|_| fail("PROXY header timeout"))?
            .map_err(|_| fail("invalid PROXY protocol header"))?;
        let _ = digest.proxy_protocol_addr.set(address);
        Ok(())
    }
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid PROXY protocol header")
}
async fn read(stream: &mut Stream) -> io::Result<Option<SocketAddr>> {
    let mut first = [0; 12];
    stream.read_exact(&mut first).await?;
    if &first == b"\r\n\r\n\0\r\nQUIT\n" {
        let mut tail = [0; 4];
        stream.read_exact(&mut tail).await?;
        let length = u16::from_be_bytes([tail[2], tail[3]]) as usize;
        if tail[0] >> 4 != 2 || length > 512 {
            return Err(invalid());
        }
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).await?;
        if tail[0] & 15 == 0 {
            return Ok(None);
        }
        if tail[0] & 15 != 1 {
            return Err(invalid());
        }
        return match tail[1] {
            0x11 if length >= 12 => Ok(Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(
                    payload[0], payload[1], payload[2], payload[3],
                )),
                u16::from_be_bytes([payload[8], payload[9]]),
            ))),
            0x21 if length >= 36 => Ok(Some(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(
                    <[u8; 16]>::try_from(&payload[..16]).unwrap(),
                )),
                u16::from_be_bytes([payload[32], payload[33]]),
            ))),
            0x00 => Ok(None),
            _ => Err(invalid()),
        };
    }
    if !first.starts_with(b"PROXY ") {
        return Err(invalid());
    }
    let mut line = first.to_vec();
    while !line.ends_with(b"\r\n") {
        if line.len() >= 108 {
            return Err(invalid());
        }
        line.push(stream.read_u8().await?);
    }
    let line = std::str::from_utf8(&line[..line.len() - 2]).map_err(|_| invalid())?;
    let fields: Vec<_> = line.split(' ').collect();
    if fields.get(1) == Some(&"UNKNOWN") {
        return Ok(None);
    }
    if fields.len() != 6 {
        return Err(invalid());
    }
    let source: IpAddr = fields[2].parse().map_err(|_| invalid())?;
    let destination: IpAddr = fields[3].parse().map_err(|_| invalid())?;
    let port = fields[4].parse().map_err(|_| invalid())?;
    let _: u16 = fields[5].parse().map_err(|_| invalid())?;
    if !((fields[1] == "TCP4" && source.is_ipv4() && destination.is_ipv4())
        || (fields[1] == "TCP6" && source.is_ipv6() && destination.is_ipv6()))
    {
        return Err(invalid());
    }
    Ok(Some(SocketAddr::new(source, port)))
}
