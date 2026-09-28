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

This removes the body channel for eligible responses. The request dispatch and
response callback channels remain. Product response permits are still retained
until downstream socket completion or cancellation. The feature can be disabled
for a same-source ablation; dependency feature isolation must be measured
separately from this body change.

Tests extend the existing dispatcher tests to cover response consumption and
connection reuse on both sides of the size boundary, and prompt delivery of the
first fragment while a small response is incomplete. Existing parser, framing,
upgrade and legacy-client tests continue to apply.
