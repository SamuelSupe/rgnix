# Release engineering

Starting with v0.3.0, the `Release` workflow builds native Linux amd64/arm64 archives, a multi-platform image and an OCI Helm chart. The existing v0.2.0 assets remain unchanged. All 0.x releases are marked as GitHub previews.

## Gates and artifacts

1. Native Linux amd64 and arm64 runners build inside Rust 1.98 / Debian bookworm containers, avoiding a newer build-host glibc requirement in the Debian runtime.
2. Both run formatting, Clippy, unit tests and socket/controller/logging/migration regression suites with locked dependencies as an unprivileged user, so filesystem-permission fault checks exercise the non-root runtime contract.
3. The amd64 binary runs the Gateway API Kubernetes/TLS/gRPC behavioral harness in Kind with SHA256-verified v1.6.1 CRDs.
4. Only after these gates pass, assemble the native binaries into an amd64/arm64 image with BuildKit SBOM and provenance, sign the published digest using Sigstore OIDC, package a chart pointing to the versioned GHCR image, and generate archive SHA256 checksums.
5. Publish image, OCI chart and GitHub release assets. GitHub artifact attestations cover the binary archives, chart and checksum file. Versions with major zero or a prerelease suffix are GitHub prereleases.

Image location: `ghcr.io/samuelsupe/rgnix:VERSION`. OCI chart location: `oci://ghcr.io/samuelsupe/rgnix/charts/rgnix`. These are publication destinations, not assertions that a particular version already exists. Image SBOM generation is not a vulnerability scan or proof that all vulnerabilities are absent.

## Preparing a version

Update Cargo.toml, the rgnix package entry in Cargo.lock, Chart.yaml version/appVersion, the default chart tag and README download instructions together. Add an honest changelog and attach validation records for that source revision. Keep Gateway conformance certification separate from the in-repository behavioral harness.

Run a manual `workflow_dispatch` first. It builds candidates and uploads artifacts without pushing registry tags or creating a GitHub release. Once reviewed, push a new `vVERSION` tag on that exact commit. The tag must match both Cargo and chart metadata. Existing releases are not overwritten. The workflow requires the repository's Actions token to have package publication and artifact-attestation permissions.

```sh
gh workflow run release.yml --ref YOUR_BRANCH
gh run list --workflow release.yml
```

After a successful tagged run, verify both image architectures and the registry access policy. A GitHub package may need its initial visibility configured by the repository owner. Keep consumers on explicit versions or digests; the workflow does not move a `latest` tag.

## Verification and upgrade

```sh
sha256sum -c SHA256SUMS
gh attestation verify rgnix-VERSION-linux-amd64.tar.gz --repo SamuelSupe/rgnix
cosign verify ghcr.io/samuelsupe/rgnix@sha256:IMAGE_DIGEST \
  --certificate-identity https://github.com/SamuelSupe/rgnix/.github/workflows/release.yml@refs/tags/vVERSION \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
helm pull oci://ghcr.io/samuelsupe/rgnix/charts/rgnix --version VERSION
```

Verify the SHA256 or attestation of the chart archive as well. The OCI chart is published over an authenticated registry connection; this workflow does not claim a separate Helm provenance signature.

Run `check`, migration assessment and request comparisons before standalone upgrades. Back up durable history, policy files and certificate/plugin dependencies. For Kubernetes, review `helm diff` or rendered manifests, keep two replicas and maxUnavailable=0, and watch readiness, rejection/error metrics and resource status. Gateway restores last-good plugins from controller-namespace ConfigMap checkpoints bound to Gateway, Route and source ConfigMap UIDs. Writes are asynchronous: only persisted updates are crash-durable, and live permissions, Secrets and endpoints are always rechecked. Preserve the controller's checkpoint ConfigMaps during upgrades. Upgrades requiring listener/worker changes require a restart.

Rollback uses the previous image/chart version and matching source configuration. Never apply older Gateway CRDs over a newer cluster installation as an application rollback step. Read the release-specific compatibility notes before restoring a durable history directory produced by a newer binary.
