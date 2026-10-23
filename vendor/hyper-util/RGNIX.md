# rgnix experimental transport patch

Source: crates.io `hyper-util` 0.1.20, upstream https://github.com/hyperium/hyper-util.
The original license and source are retained. `Cargo.toml.orig` and
`.cargo_vcs_info.json` record the upstream package identity.

This is a direct optional path dependency, **not** a crates.io-wide patch.
reqwest and kube continue to use the registry version.

The normal and test Hyper dependencies point to the isolated sibling `../hyper`.
Only the experimental product enables its `rgnix-full-body` feature; see
[the Hyper patch contract](../hyper/RGNIX.md). Registry users remain independent,
including their feature sets. The root lockfile pins both dependency identities.

Changes in `src/client/legacy/client.rs`:

- Keep `Request<B>` boxed while the legacy client carries it through connection
  checkout, the outer send future, and retryable error storage. Unbox once at
  the existing Hyper dispatch boundary. Request retry policy, pool handling,
  cancellation and response-body ownership stay unchanged.
- Consume the unused timer argument in an HTTP/1-only build.
- Share immutable client state through `Arc`, so cloning a request's client
  handle does not clone the connector and protocol builders. The connector
  is still cloned when a connection is actually needed.
- Poll the existing pool checkout first. An idle hit does not construct the
  connection future; a miss retains its registered waiter and runs the original
  checkout/connect race in a boxed future. Expiration, poisoned connections,
  cancellation cleanup and retry policy still use the original pool logic.

The request box adds one allocation; the cold connection path adds one more.
The client state has one shared allocation per client, instead of repeated
connector clones on the request path. Retain these patches only with measured
copy, allocation and behavioral evidence; the product transport disables retries.
See `docs/validation-hyper-prepared-2026-09-28.md` and
`docs/validation-hyper-pool-2026-09-28.md` in the root repository. Fewer counted
operations do not by themselves establish a throughput or tail-latency gain.
