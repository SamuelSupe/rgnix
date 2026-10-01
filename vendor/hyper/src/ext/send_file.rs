use std::{fs::File, sync::Arc};

/// A bounded file region sent after HTTP/1 headers by an IO adapter supporting sendfile.
/// The response must have exactly this Content-Length and an empty Body. This
/// extension must not be used with TLS, compression, HTTP/2 or transfer encoding.
#[derive(Clone, Debug)]
pub struct SendFile {
    /// An already opened file; the transport never resolves a pathname.
    pub file: Arc<File>,
    /// Byte offset in the opened file.
    pub offset: u64,
    /// Remaining bytes. EOF before this boundary is a transport failure.
    pub length: u64,
}
