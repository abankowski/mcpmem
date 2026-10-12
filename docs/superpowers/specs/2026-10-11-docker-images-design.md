# Docker Image Publication Design

## Goal

Publish a complete `mcpmem` image for Linux AMD64 and ARM64 with each release.

The image must let an operator keep data, configuration, certificates, and secrets outside the image.

## Scope

The release publishes these image names:

- `ghcr.io/abankowski/mcpmem`
- `docker.io/abankowski/mcpmem`

The image includes all Cargo features, the embedded browser UI, Poppler, and CA certificates.

The image does not include a reverse proxy, a certificate authority client, sample secrets, or a health endpoint.

## Image Contract

Use a multi-stage Debian build. The build stage must use Node 22 to build `ui/dist` and generate `src/ui/assets.rs`.

The Rust build must run `cargo build --release --all-features --locked -p mcpmem`.

The runtime stage must use a Debian GNU/Linux base. It must include `ca-certificates` and `poppler-utils`.

Run the process as the fixed non-root UID `10001`. The image creates `/var/lib/mcpmem` with that owner.

The runtime stage contains only the `mcpmem` binary, the default TOML file, the entrypoint, and runtime packages. It must not contain Node, Cargo, source code, or build output.

The entrypoint must create `/var/lib/mcpmem` only when its parent is writable. It must not change ownership of a bind mount.

```text
mcpmem --config /etc/mcpmem/mcpmem.toml
```

The entrypoint must not generate credentials, certificates, or a database owner.

Operators who use a bind mount must make the data directory writable by UID `10001`. Docker named volumes inherit the image directory owner.

## Configuration and Filesystem Contract

The image provides this default configuration path:

```text
/etc/mcpmem/mcpmem.toml
```

The default file sets the durable data root to:

```text
/var/lib/mcpmem/memory.mcpmem
```

It enables every product tool and runtime role. It declares the direct-TLS paths below. It does not contain a secret or certificate.

| Purpose | Container path | Access | Operator action |
| --- | --- | --- | --- |
| Durable data | `/var/lib/mcpmem` | Read-write | Mount one named volume or host directory. |
| Configuration | `/etc/mcpmem/mcpmem.toml` | Read-only | Replace the default file when settings differ. |
| TLS certificate | `/etc/mcpmem/tls/tls.crt` | Read-only | Mount only for direct TLS. |
| TLS key | `/etc/mcpmem/tls/tls.key` | Read-only | Mount only for direct TLS. |
| Bearer token | `/run/secrets/mcpmem-auth-token` | Read-only | Mount a Docker secret or host secret file. |
| OAuth principals | `/etc/mcpmem/principals.json` | Read-only | Mount only for OAuth. |
| OAuth, indexer, OCR, and webhook keys | `/run/secrets/*` | Read-only | Mount only when the selected configuration uses them. |

The operator must mount the whole data directory. They must not mount only `memory.mcpmem`, because mcpmem also creates the workspace registry and workspace graph files beside it.

The supplied direct-TLS configuration uses `auth-token-file`. It must fail on startup if the token or certificate mount is absent. The image must never start an unauthenticated HTTP server.

A fresh data volume needs `legacy-owner-id`. The default configuration uses `machine:local`. Operators who need a different owner must mount a replacement TOML file before first startup.

A reverse-proxy deployment uses a replacement TOML file with `oauth-trust-forwarded-proto`. The documentation must state that the application network is internal-only and the proxy overwrites `X-Forwarded-For`.

The image must not declare a Docker `VOLUME`. The Compose example declares the named data volume explicitly. The direct `docker run` example must include an explicit data mount.

## Operator Interface

Provide one direct-TLS Compose example. It must:

- expose port `8443`;
- mount the named data volume at `/var/lib/mcpmem`;
- mount configuration, certificate material, and secrets read-only;
- set `read_only: true` for the root filesystem;
- mount a writable `tmpfs` at `/tmp` for PDF rendering;
- drop all Linux capabilities;
- set `no-new-privileges:true`.

The documentation must provide matching `docker run` and Compose commands. It must show one direct-TLS configuration and one reverse-proxy override.

The reverse-proxy example must not ship another proxy image. Existing operators already choose their proxy and certificate lifecycle.

## Release Contract

Keep the current release trigger and crate publication gate. Add image work only after `publish` succeeds.

Build `linux/amd64` on the existing Ubuntu AMD64 runner. Build `linux/arm64` on the existing Ubuntu ARM64 runner. Do not emulate either architecture with QEMU.

Each platform job builds the full image and pushes a digest-addressed platform image to GHCR. A final release job creates the GHCR multi-platform manifests.

Use GHCR as the source registry. Copy each completed GHCR manifest to Docker Hub. This avoids a second platform build and preserves the published image content.

The release tags are:

- every release: `<major>.<minor>.<patch>`;
- stable releases only: `<major>.<minor>` and `latest`.

A prerelease must not move `latest` or a floating minor tag.

Give only the new container jobs `packages: write`. Do not widen permissions for existing release jobs. Store the Docker Hub token in `DOCKERHUB_TOKEN`. Store the Docker Hub username in the `DOCKERHUB_USERNAME` repository variable.

The release must fail when either registry does not contain each required platform manifest and tag. Crates.io publication remains the release gate. A Docker publication failure after crate publication needs an operator retry; it must not republish crates.

## CI and Verification

Add a pull-request container job for Linux AMD64. The job builds the final Docker target but does not push an image.

Add `scripts/test-container-image.sh`. The script must prove the user-visible container contract:

1. Build the final image.
2. Start it with a temporary data directory, an explicit test configuration, and a bearer-token file.
3. Use a fresh data directory with `legacy-owner-id`.
4. Wait for `GET /ui` with the bearer credential to return HTTP 200.
5. Confirm that the response contains `<div id="root">`.
6. Stop the container.
7. Start a second container against the same data directory without `legacy-owner-id`.
8. Repeat the authenticated `/ui` request.

This test proves both first-start initialization and durable storage reuse. It does not need a new product health endpoint.

The release platform jobs must run the same smoke script for their own image before they push a platform digest.

The manifest job must inspect both registry tags after publication. It must verify `linux/amd64` and `linux/arm64` in each manifest list.

## Documentation and Change Record

Add a Docker section to `README.md`. It must describe image names, tags, architecture support, required mounts, direct TLS, reverse-proxy requirements, and first-start ownership.

Update `CHANGES.md` with the new distribution channel. Update the release runbook with Docker Hub setup, GHCR package visibility, required repository secret and variable, and image retry steps.

## Acceptance Criteria

1. A tagged stable release publishes identical version coverage to both registries for Linux AMD64 and ARM64.
2. A container starts only with an explicit authentication configuration.
3. A user can replace the TOML file, certificates, principals, and every secret with read-only mounts.
4. A user can preserve all mcpmem data by mounting one writable data directory.
5. The browser UI works from the final image without Node or `ui/dist` at runtime.
6. A fresh data mount initializes with the configured legacy owner, and a second start reuses it.
7. Pull-request CI proves the final image build and smoke path without registry publication.
