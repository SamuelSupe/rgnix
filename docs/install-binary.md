# Linux amd64 and arm64 binaries

The v0.3.0 preview provides separate Linux amd64 and arm64 archives, built from the tagged source and locked dependencies on native runners in Debian 12 containers. These are ELF executables for glibc Linux, not macOS or Alpine/musl binaries.

## Runtime requirements

- Linux x86-64 (`amd64`) or AArch64 (`arm64`), matching the selected archive. Debian 12 with glibc 2.36 is the release build/runtime baseline.
- `libgcc_s.so.1` and the normal glibc runtime libraries. Install `libgcc-s1` on Debian/Ubuntu if absent.
- System CA certificates for upstream HTTPS verification. Install `ca-certificates` and keep the trust store current.
- Install `libssl3` on Debian/Ubuntu for builds using dynamic OpenSSL. Each archive includes `ELF-INFO.txt` with that executable's dynamic dependencies and symbol versions; source builds may differ.

The release also includes `rgnix-0.3.0.tgz`, a Helm chart. `SHA256SUMS` lists both binary archives and the chart; the command below checks only the downloaded binary archive.

See the [release workflow and signature verification guide](releases.md). Runtime compatibility on other distributions is not separately certified. The older v0.2.0 artifact identity remains in its [historical record](validation/release-0.2.0-artifact.json).

## Verify and run

Download the appropriate v0.3.0 archive and `SHA256SUMS` from the same release. For amd64 (set `arch=arm64` on AArch64):

```sh
arch=amd64
archive="rgnix-0.3.0-linux-$arch.tar.gz"
grep " $archive\$" SHA256SUMS | sha256sum -c -
tar -xzf "$archive"
cd "rgnix-0.3.0-linux-$arch"

./rgnix --version
./rgnix check -c examples/nginx.conf
./rgnix serve -c examples/nginx.conf
```

In another terminal, `curl http://localhost:8080/health` should return `ok`. The static home page works immediately; `/api/` expects application upstreams on ports 9001–9003.

If desired, install the executable with `sudo install -m 0755 rgnix /usr/local/bin/rgnix`. Keep your configuration, scripts and static files together: relative paths resolve from the main configuration file's directory.

The archive includes the license, examples, and full documentation under `docs/`. The repository's [operations guide](deployment.md) covers service management, TLS, Kubernetes and limits.
