# Docker Image Publication Implementation Plan

> **For the implementation agent:** Use `skill://executing-plans` to execute this plan task by task.

**Goal:** Publish a full-featured mcpmem image for Linux AMD64 and ARM64 to GHCR and Docker Hub.

**Architecture:** Build the UI and Rust binary in a multi-stage Debian Dockerfile. Run the final image as UID 10001. Keep data, settings, certificates, and secrets on mounts. Build native images on the release runners, join them in GHCR, and copy the manifests to Docker Hub.

**Tech Stack:** Docker Buildx, Docker Compose, GitHub Actions, Debian, Node 22, Rust Cargo, Bash.

**Design:** `docs/superpowers/specs/2026-10-11-docker-images-design.md`

**Task graph:** Task 1 → Task 2 → {Task 3, Task 4}; Task 4 → Task 5; {Task 2, Task 5} → Task 6; {Task 3, Task 5, Task 6} → Task 7.

## Task 1: Add the Docker build and runtime boundary

**Files:** `Dockerfile`, `.dockerignore`, `docker/entrypoint.sh`, `docker/mcpmem.toml`.

1. Exclude `.git`, `target`, `ui/node_modules`, local data, certificates, and environment files from the build context.
2. Use Node 22 Debian to run `npm ci`, `npm run build`, `npm run check`, and `node scripts/generate-assets-table.mjs`.
3. Use a Rust Debian builder to run `cargo build --release --all-features --locked -p mcpmem`.
4. Use a Debian slim runtime with only `ca-certificates`, `poppler-utils`, the binary, TOML, and entrypoint.
5. Create user and group `mcpmem` with UID and GID `10001`. Create `/var/lib/mcpmem` with that owner. Set `USER 10001`.
6. Do not add Node, Cargo, source code, `ui/dist`, secrets, certificates, or `VOLUME` to the final image.
7. Use a POSIX entrypoint. Create the data directory only when writable. Do not `chown` a mount. Run `exec mcpmem --config /etc/mcpmem/mcpmem.toml "$@"`.
8. Set the default TOML values below. It must contain paths only and fail when the token or TLS files are absent.

```toml
[server]
memory-file = "/var/lib/mcpmem/memory.mcpmem"
transport = "http"
bind = "0.0.0.0:8080"
roles = ["mcp", "indexer", "extractor", "webhooks"]
ui = true

[workspaces]
legacy-owner-id = "machine:local"

[tools]
all = true

[security]
auth-token-file = "/run/secrets/mcpmem-auth-token"
tls-cert = "/etc/mcpmem/tls/tls.crt"
tls-key = "/etc/mcpmem/tls/tls.key"
```

9. Verify with `docker build --target runtime --tag mcpmem:local .` and inspect the final filesystem.

## Task 2: Add an operator Compose example and smoke test

**Files:** `docker/compose.yaml`, `docker/config/mcpmem.toml`, `docker/config/reverse-proxy.toml`, `scripts/test-container-image.sh`, `docker/test/mcpmem.toml`.

1. Add a direct-TLS Compose example. Use `MCPMEM_IMAGE`, expose `8443:8080`, mount a named data volume, and mount configuration, TLS files, and token read-only.
2. Set `read_only: true`, `tmpfs: /tmp`, `cap_drop: [ALL]`, and `security_opt: [no-new-privileges:true]`.
3. Add a direct-TLS TOML file and a separate OAuth reverse-proxy TOML. The reverse-proxy file sets `oauth.trust-forwarded-proto = true` and states the internal-network and `X-Forwarded-For` requirements.
4. Add a smoke script that accepts `MCPMEM_IMAGE` and supports `--build`. It must never publish an image.
5. Start the image with a temporary data directory, HTTP-only test TOML, bearer-token file, and `legacy-owner-id`. Poll authenticated `GET /ui` for HTTP 200 and `<div id="root">`.
6. Restart against the same data directory with a TOML that omits `legacy-owner-id`. Repeat the UI request. Use a shell trap for all cleanup.
7. Verify with `MCPMEM_IMAGE=mcpmem:local scripts/test-container-image.sh`.

## Task 3: Add pull-request validation

**Files:** `.github/workflows/ci.yml`.

1. Add a `container` job after `version`. Skip it only for a bump-only change. Retain `contents: read`.
2. Build the `runtime` target for AMD64 with Docker Buildx and tag it `mcpmem:ci`. Do not push it.
3. Run `MCPMEM_IMAGE=mcpmem:ci scripts/test-container-image.sh`.
4. Prove the gate fails by changing the expected root element, then restore it and rerun the normal build and smoke commands.

## Task 4: Publish native platform images to GHCR

**Files:** `.github/workflows/release.yml`.

1. Add a `container-platform` matrix after `publish`, only for a `release` event.
2. Use `ubuntu-latest` for `linux/amd64` and `ubuntu-24.04-arm` for `linux/arm64`. Do not use QEMU.
3. Give only these new jobs `contents: read` and `packages: write`. Check out the release tag and authenticate to GHCR with `GITHUB_TOKEN`.
4. Build each full platform image, load a unique local tag, and run the smoke script before any push.
5. Push a digest-addressed temporary GHCR image. Export one immutable digest reference artifact per platform.
6. Add `container-manifest`. Download both artifacts and create GHCR manifests for `X.Y.Z`, plus `X.Y` and `latest` only for stable releases.
7. Inspect every GHCR manifest and fail unless it lists both AMD64 and ARM64.

## Task 5: Mirror manifests to Docker Hub

**Files:** `.github/workflows/release.yml`, the release runbook.

1. In the manifest job, authenticate with `${{ vars.DOCKERHUB_USERNAME }}` and `${{ secrets.DOCKERHUB_TOKEN }}`.
2. Use a fixed, versioned manifest copy tool. Copy completed GHCR manifests and platform images to `docker.io/abankowski/mcpmem`; do not rebuild.
3. Copy the immutable tag first. Copy minor and `latest` only after the stable immutable tag passes.
4. Inspect each Docker Hub tag. Confirm both required platforms by content.
5. Document that a Docker failure after crates.io needs a Docker-only retry or a manual copy from GHCR. Do not republish crates.

## Task 6: Document the public image

**Files:** `README.md`, `CHANGES.md`, the release runbook.

1. Add image names, architecture support, tag rules, and direct-TLS commands to the README.
2. Add external mount access, data-directory scope, UID 10001, first owner, and reverse-proxy trust boundary to the README.
3. Include Bash and fish commands when the forms differ. State when they are identical.
4. State that the image has no bundled proxy, certificate authority client, or secret.
5. List the `DOCKERHUB_USERNAME` repository variable, `DOCKERHUB_TOKEN` secret, GHCR visibility decision, manifest inspection, and Docker-only retry procedure in the release runbook.
6. Add a user-facing Docker distribution entry to `CHANGES.md`.

## Task 7: Run the verification set

1. Run `docker build --target runtime --tag mcpmem:local .`.
2. Run `MCPMEM_IMAGE=mcpmem:local scripts/test-container-image.sh`.
3. Run `docker compose -f docker/compose.yaml config`.
4. Run the repository CI mirror from `.omp/AGENTS.md`.
5. Inspect the workflow diff. Confirm that only container jobs use `packages: write`, prereleases cannot update floating tags, native smoke tests run before push, and both registries contain both platforms.
6. Record exact commands and output summaries in the pull request. Record the Docker Hub settings as deployment dependencies. Do not publish an image from a pull request.
