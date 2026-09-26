# Contributing to rgnix

Bug reports and focused pull requests are welcome. Issues may be written in English or Chinese.

## Development checks

Build and test on Linux. The project is validated with OrbStack Ubuntu arm64; the Docker build uses Rust 1.98.0. Install the native dependencies listed in [README](README.md#quick-start), then run:

```sh
bash scripts/check.sh
cargo clippy --locked --all-targets -- -D warnings
helm lint charts/rgnix
helm template rgnix charts/rgnix > /dev/null
```

`scripts/check.sh` runs formatting, Rust tests, a build, HTTP and simulated Kubernetes API recovery, OTLP, log rotation, and product feature suites. They use local temporary servers and require Python 3, curl, OpenSSL, logrotate, python3-grpcio and python3-brotli (Debian/Ubuntu package names). They do not need a live Kubernetes cluster.

NGINX differential checks require **NGINX 1.28.0**:

```sh
NGINX=/path/to/nginx python3 scripts/nginx_parity.py target/debug/rgnix
```

Live Ingress acceptance creates and changes resources in a **dedicated test namespace**, including rolling restarts and endpoint removal. Read the script before running it; it retains resources for inspection and rejects an existing namespace without the `rgnix-qa=true` label.

```sh
RGNIX_IMAGE_TAG=0.4.0 bash scripts/ingress-e2e.sh rgnix-qa-example orbstack
```

The cluster must have the image available. This is an acceptance fixture, not a command for an application namespace.

## Useful changes and reports

- Describe the observable problem, a minimal configuration/script, expected behavior, and actual results.
- Include the rgnix version, OS/architecture, deployment mode, and relevant sanitized logs. Remove private keys, tokens and application data.
- Keep changes within one responsibility and reuse existing helpers. Add regression coverage for a real behavior or boundary, rather than implementation details.
- Document compatibility changes, plugin ABI effects, and operational limits. Distinguish compilation, mock API tests and real cluster validation.
- Follow the existing formatting and keep `Cargo.lock` checked in. Dependency changes should explain their purpose.

The [compatibility matrix](docs/compatibility.md) and [RGL contract](docs/rgl.md) define the current scope. Propose substantial language or protocol changes in an issue before implementing them.
