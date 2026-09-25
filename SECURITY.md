# Security policy

rgnix is currently a preview. Security fixes target the current development branch and the next preview release; there is no LTS or enterprise support commitment. Use the compatibility and validation records to decide whether a release is suitable for your deployment.

Report suspected vulnerabilities through GitHub's private vulnerability reporting facility when available. Do not post credentials, private keys, customer traffic or an exploit against a live third-party system in a public issue. If private reporting is unavailable, open a minimal issue asking the maintainer for a private reporting channel without disclosing exploit details.

Include the version/commit, a minimal configuration, the affected trust boundary, reproduction steps in an isolated environment and the observed impact. RGL sandbox escapes, tenant isolation failures, unauthorized backend/certificate access and request framing inconsistencies are security-sensitive.

Deploy least-privilege RBAC, protect management endpoints and use dedicated Gateway deployments where a process boundary is required. Wasm fuel and namespace budgets bound application work; they are not OS-level tenant isolation. Use signed image digests and verify release attestations as described in [release engineering](docs/releases.md).
