# Linux arm64 binary

The v0.2.0 preview archive contains the binary extracted from the release Linux arm64 image, built from the tagged source and locked dependencies. It is an AArch64 ELF executable built on Debian 12, not a macOS or Alpine/musl binary.

## Runtime requirements

- Linux arm64 with glibc. The binary imports symbols up to **GLIBC_2.34**; use glibc 2.34 or newer.
- `libgcc_s.so.1` and the normal glibc runtime libraries. Install `libgcc-s1` on Debian/Ubuntu if absent.
- System CA certificates for upstream HTTPS verification. Install `ca-certificates` and keep the trust store current.
- This release's ELF dependency table has no dynamic `libssl` or `libcrypto` dependency; OpenSSL is linked into the executable. Other source builds may differ.

The release also includes `rgnix-0.2.0.tgz`, a Helm chart. `SHA256SUMS` lists both assets; the command below checks only the binary archive.

The exact binary SHA-256 and validation environment are in [the artifact record](validation/release-0.2.0-artifact.json). Runtime compatibility on other distributions is not separately certified.

## Verify and run

Download `rgnix-0.2.0-linux-arm64.tar.gz` and `SHA256SUMS` from the same release, then run:

```sh
grep ' rgnix-0.2.0-linux-arm64.tar.gz$' SHA256SUMS | sha256sum -c -
tar -xzf rgnix-0.2.0-linux-arm64.tar.gz
cd rgnix-0.2.0-linux-arm64

./rgnix --version
./rgnix check -c examples/nginx.conf
./rgnix serve -c examples/nginx.conf
```

In another terminal, `curl http://localhost:8080/health` should return `ok`. The static home page works immediately; `/api/` expects application upstreams on ports 9001–9003.

If desired, install the executable with `sudo install -m 0755 rgnix /usr/local/bin/rgnix`. Keep your configuration, scripts and static files together: relative paths resolve from the main configuration file's directory.

The archive includes the license, examples, and full documentation under `docs/`. The repository's [operations guide](deployment.md) covers service management, TLS, Kubernetes and limits.
