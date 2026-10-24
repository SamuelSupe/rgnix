# rgnix experimental HTTP/1 patch

Source: crates.io `hyper` 1.11.1, upstream https://github.com/hyperium/hyper.
The package source, original manifest, VCS identity and MIT license are retained.
This optional path dependency is used by the experimental rgnix transport and its
isolated hyper-util client. It is not a crates.io-wide patch; reqwest/kube keep
the registry versions.

The `rgnix-full-body` feature adds an owned `Bytes` variant to Incoming. Immediately
after parsing a client response head, an exact-length body of 1..=8192 bytes can
use it when all bytes are already in the existing input buffer. The existing
decoder performs framing and connection-state transitions. The optimization
does not wait for more data, prefetch a response, or bypass header validation.
Requests, upgrades, Expect, declared trailers, chunked/EOF-delimited bodies and
incomplete or larger bodies retain the original channel path.

This removes the body channel for eligible responses. Standard `SendRequest`
retains request dispatch and response callback channels. Product response permits
are still retained until downstream socket completion or cancellation. The feature can be disabled
for a same-source ablation; dependency feature isolation must be measured
separately from this body change.

Tests extend the existing dispatcher tests to cover response consumption and
connection reuse on both sides of the size boundary, and prompt delivery of the
first fragment while a small response is incomplete. Existing parser, framing,
upgrade and legacy-client tests continue to apply.

HTTP/1 request-header timers retain their allocation across keep-alive
exchanges. Completion disables timeout checks; the next header read resets
the timer to its own deadline. A retained timer may wake a long-running body
once, but cannot expire that body or renew a partially received header's
deadline. Keep incomplete/trickling header and persistent-connection checks
when upgrading this patch.

With `rgnix-extensions`, a fresh client HTTP/1 connection can be converted with
`into_inline`. Its single owner submits a request with `start_request`, drives
headers with `poll_response`, and continues polling the connection while reading
a streaming body. It reuses the existing dispatcher, parser, decoder, buffering,
I/O timeouts and response-body channels. Only request submission and response
delivery use owner-local slots instead of channels. A cancelled exchange must
drop its connection; requests are never returned for replay. The ordinary rgnix
pool performs its existing idle EOF check before reuse. Upgrades and multiplexed
requests keep the standard client API.

An HTTP/1 `SendInformational` sender can be marked for direct forwarding with a
noncapturing response filter. The client moves owned 102/103 responses through
the existing bounded queue without allocating a generic callback or cloning its
headers. Unmarked senders and the general callback API retain their behavior.
The forwarding path recycles an empty request Extensions map back to its server
connection; maps with any remaining metadata are not cached. The cache never
contains a sender, preventing reference cycles and cross-request metadata leaks.
Status/size bounds, stale-sender isolation and final-response hooks still apply.

The owned connection exposes an idle poll that reuses the existing buffered
IO/TLS reader. It skips dispatcher write/flush work only when no request or body
is active, no bytes remain buffered for writing, and the last transport flush
completed. Busy connections, pending flushes and EOF keep the normal driver.
A controlled pending-flush regression fails when this guard is removed.

With `rgnix-extensions`, each HTTP/1 connection also retains up to 64 parsed
header names of at most 64 bytes each. A positional hit requires an exact match
of the current wire name before cloning its canonical HeaderName. Values,
ordering, framing and all parser validation continue through the existing codec.
Case changes and misses use the original parser. After 64 positional mismatches,
the connection clears the entries and uses the original whole-header parse path;
the bounded vector capacity remains until the connection closes. Wire and
canonical name payloads total at most 8 KiB per connection, plus 64 entry
descriptors and allocator metadata. Nothing is shared between connections or
retained from header values. Parser regressions cover changing values, case,
order, duplicates, invalid framing/names, long/wide fields and sustained churn.
