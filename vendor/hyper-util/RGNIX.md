# rgnix experimental transport patch

Source: crates.io `hyper-util` 0.1.20, upstream https://github.com/hyperium/hyper-util.
The original license and source are retained. `Cargo.toml.orig` and
`.cargo_vcs_info.json` record the upstream package identity.

This is a direct optional path dependency, **not** a crates.io-wide patch.
reqwest and kube continue to use the registry version.

Changes in `src/client/legacy/client.rs`:

- Keep `Request<B>` boxed while the legacy client carries it through connection
  checkout, the outer send future, and retryable error storage. Unbox once at
  the existing Hyper dispatch boundary. Request retry policy, pool handling,
  cancellation and response-body ownership stay unchanged.
- Consume the unused timer argument in an HTTP/1-only build.

The request box adds one allocation. Retain this patch only with measured copy,
allocation and behavioral evidence; the product transport disables retries.
See `docs/validation-hyper-prepared-2026-09-28.md` in the root repository.
