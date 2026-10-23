#[cfg(any(feature = "http1", feature = "http2"))]
mod client;
#[cfg(any(feature = "http1", feature = "http2"))]
pub use client::{Builder, Client, Error, ResponseFuture};
pub use pool::IdlePoolBudget;

/// Override HTTP/2 `:authority` after endpoint connection and pool selection.
/// HTTP/1 retains its Host header and its endpoint URI conversion.
#[derive(Clone, Debug)]
pub struct RequestAuthority(pub http::uri::Authority);

pub mod connect;
#[doc(hidden)]
// Publicly available, but just for legacy purposes. A better pool will be
// designed.
pub mod pool;
