//! Extensions for the HTTP/3 protocol.

use std::{borrow::Cow, str::FromStr};

/// Describes the `:protocol` pseudo-header for extended connect
///
/// See: <https://www.rfc-editor.org/rfc/rfc8441#section-4>
#[derive(PartialEq, Debug, Clone)]
pub struct Protocol(Cow<'static, str>);

impl Protocol {
    /// WebTransport protocol
    pub const WEB_TRANSPORT: Protocol = Protocol(Cow::Borrowed("webtransport"));
    /// RFC 9298 protocol
    pub const CONNECT_UDP: Protocol = Protocol(Cow::Borrowed("connect-udp"));

    /// RFC 9220 WebSocket protocol.
    pub const WEBSOCKET: Protocol = Protocol(Cow::Borrowed("websocket"));

    /// Return a &str representation of the `:protocol` pseudo-header value
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Error when parsing the protocol
pub struct InvalidProtocol;

impl FromStr for Protocol {
    type Err = InvalidProtocol;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) {
            return Err(InvalidProtocol);
        }
        // Unknown valid tokens reach the application so it can return RFC 9220's 501.
        Ok(Self(Cow::Owned(s.to_owned())))
    }
}
