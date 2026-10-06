# Release runbook

`mcpmem` publishes to crates.io only for a published GitHub release. A push to
`main` never publishes. The workflow is `.github/workflows/release.yml`.

The commands below are identical in Bash and fish.

## The version rules

- The workspace holds six crates. All six carry the same version.
- A tag is `v` plus the version, for example `v1.0.0`.
- The version is strict semver 2.0.0. Build metadata is rejected: crates.io
  stores it, but no dependency can request it, so the release is unreachable.
- A prerelease version, for example `1.0.0-rc.1`, needs a GitHub release that
  is marked as a prerelease. The workflow fails when the two disagree.
- The released commit must be an ancestor of `origin/main`.

`scripts/check-release-version.sh` enforces every rule above. Ordinary CI runs
it without arguments. The release workflow adds `--registry`, which also
requires the version to be unpublished.

## The first release binds the names on crates.io

A crate name belongs to nobody until a version of it exists. The first release
is therefore `1.0.0-rc.1`, a prerelease: it takes all six names, and it lets
crates.io accept the Trusted Publisher entries, which it refuses for a crate
that does not exist.

A prerelease is the right tool for that job. `cargo install mcpmem` and a
caret dependency both ignore a prerelease, so nobody receives it by accident,
and `1.0.0` stays free for the first stable release.

Do not use `0.0.1` for this. It is a stable version, it is permanent, and
`0.x` states that the API may break at any time, which is not what this
release means.

Mark the GitHub release as a prerelease. The workflow compares the tag with
that flag and fails when they disagree.

## `main` always names the coming version

A successful release ends by advancing the version on `main`, in a commit made
by the workflow:

- a release candidate advances its counter: `1.0.0-rc.3` releases, `main`
  opens `1.0.0-rc.4`;
- a stable release advances the patch: `1.1.0` releases, `main` opens `1.1.1`.

So the version in `main` is the next release, never the one already published.
The usual release therefore needs no bump at all: tag what `main` already
says.

**Patch, and not minor, because a wrong guess costs different amounts in the
two directions.** From `1.1.1`, a release that turns out to carry a feature
moves forward to `1.2.0`. From `1.2.0`, a release that turns out to be a bugfix
has to move back to `1.1.1`, and every draft note or branch name that already
said `1.2.0` is wrong. The smallest claim keeps every correction a forward one.

`scripts/next-version.sh` holds that arithmetic and has its own tests,
`scripts/test-next-version.sh`. A minor release, a major release, and the
stable release that follows a candidate are human decisions, so set those with
`scripts/set-version.sh`. No default can prevent a wrong claim here. Deriving
the level from the commits would, and that needs a commit-message contract this
repository does not have.

The bump job does nothing when `main` no longer carries the released version,
which keeps a re-run and a manual bump from fighting each other.

## Prepare a release

1. Choose the version. When the number `main` already carries is the one you
   want, skip to the tests. Otherwise set it with one command, which edits all
   six `[package]` blocks and every path dependency requirement, refreshes
   `Cargo.lock`, and runs the gate:

   ```sh
   scripts/set-version.sh 1.0.0
   ```

   Do not edit the manifests by hand. `v1.0.0-rc.2` failed its release gate
   because the tag moved and the workspace did not.
2. Build the frontend bundle, then run the tests and the packaging checks
   locally. The root crate embeds `ui/dist` at compile time and the packaged
   crate carries it, so a fresh bundle must exist before any cargo build:

   ```sh
   cd ui && npm ci && npm run build && npm run check && cd ..
   cargo test --workspace --all-targets --locked -- --test-threads=1
   cargo package -p mcpmem-core --locked
   node scripts/check-ui-package.mjs
   ```

   `scripts/check-ui-package.mjs` runs the Cargo package dry-run and asserts
   the packaged crate ships `ui/dist/ui-manifest.json` and every manifest
   asset with matching bytes — the crate must compile its embedded asset
   table. CI runs the same check on every PR. A prebuilt server binary
   therefore needs **no Node runtime** to serve the UI: the bundle is inside
   the binary. Install that binary from the GitHub release assets (see
   "Deploying the server binary" below), not from crates.io.

3. Merge to `main` through a pull request.

## Cut the release

Tag the merged commit and publish a GitHub release. GitHub creates the tag if
it does not exist yet.

```sh
gh release create v1.1.0 --title v1.1.0 --notes-file CHANGES.md --target main
```

For a prerelease, add `--prerelease`:

```sh
gh release create v1.1.0-rc.1 --title v1.1.0-rc.1 --notes 'release candidate' --prerelease --target main
```

The `release: published` event starts the workflow. It re-runs the version
gate, checks the prerelease flag, checks the ancestry of the commit, runs the
whole test suite, and then publishes. Before the cargo build, the workflow's
frontend job rebuilds the UI bundle from the tagged `ui/src` and uploads it;
the publish and binaries jobs restore it into `ui/dist`. The publish job then
builds the release binary and smoke-tests it serving `/ui` and a real asset —
from the binary's own embedded bytes, with no Node on the host — before
crates.io is touched.

## Publish order

`scripts/publish-crates.sh` publishes in dependency order:

1. `mcpmem-core`
2. `mcpmem-runtime`, `mcpmem-indexer`, `mcpmem-webhook`
3. `mcpmem`

`cargo publish` waits for each crate to appear in the index before it returns,
so the next crate resolves it.

The root `mcpmem` package changes shape here, not order: it carries `ui/dist`
(the embedded bundle), which is why the workflow builds the frontend first and
CI gates the packaged bytes with `scripts/check-ui-package.mjs`.

The script skips a crate that crates.io already holds at this version. A
re-run after a partial failure therefore completes the release instead of
aborting on the crates that already went out.

One failure mode needed a flag. On 2026-10-05 the v3.0.0 release published
the six library crates and failed on the root crate: the package include
list pulls in the freshly built, git-ignored `ui/dist`, and cargo's dirty
check refuses uncommitted files. The root crate now publishes with
`--allow-dirty`; the library crates stay strict. A release that stops
mid-way needs a new version for the re-run.

## Dry run

`workflow_dispatch` accepts a tag and a `dry_run` flag, which defaults to true.
A dry run packages and verifies instead of publishing.

One limitation, and it is not a defect: `cargo publish --dry-run` verifies a
crate against crates.io, so a dependent cannot be verified before its
dependency is published. Before the first release, only `mcpmem-core` verifies.
The script reports the others and continues.

## The crates.io credential

`CARGO_REGISTRY_TOKEN` is an API token from crates.io. Create it under Account
Settings, then API Tokens, then New Token.

- Endpoint scopes: `publish-new` and `publish-update`. `yank` is not needed.
- Crate scope: `mcpmem*`. The pattern matches `mcpmem` and every `mcpmem-`
  crate, including a crate created after the token. The scope is evaluated on
  each call.
- Set an expiry date.

Store it as the repository secret `CARGO_REGISTRY_TOKEN`.

### Replace the token with Trusted Publishing after the first release

crates.io supports Trusted Publishing. GitHub proves the workflow identity
with OIDC, and `rust-lang/crates-io-auth-action` exchanges that proof for a
token that lives 30 minutes. The action revokes the token when the job ends.
No credential is stored in the repository.

crates.io accepts a Trusted Publisher entry only for a crate that exists, so
the first release must use the API token. After that release:

1. Open each of the six crates on crates.io. Add a Trusted Publisher: owner
   `abankowski`, repository `mcpmem`, workflow `release.yml`, environment
   `crates-io`.
2. Delete the `CARGO_REGISTRY_TOKEN` secret in the GitHub repository.
3. Revoke the same token on crates.io, under Account Settings, then API
   Tokens.

Step 3 is not optional, and it is not a duplicate of step 2. The secret in
GitHub is one copy of the token. Deleting that copy removes the workflow's
access, and it removes nothing else: the token stays valid on crates.io, for
anybody who holds it, until crates.io revokes it. Delete the secret and stop,
and the credential still exists with nothing watching it.

Do the two steps together. If the Trusted Publisher entries are wrong, the
next release fails before it publishes anything, and the repair is a new
token. That failure is cheaper than a live token nobody uses.

The workflow needs no edit. It runs the auth action when the secret is absent,
and it fails with a clear message when neither credential is available.

## Requirements in the repository settings

- Either the secret `CARGO_REGISTRY_TOKEN`, or a Trusted Publisher entry for
  all six crates.
- An environment named `crates-io`. Add required reviewers there when a manual
  approval before publishing is wanted.
- The job holds `id-token: write`, which the OIDC exchange needs.

## Deploying the server binary

The server runs from a **GitHub release asset**, not from crates.io.

The crates.io `mcpmem` package builds with **default features**, which omit
`extractor` and `webhooks`. A config whose `roles` list those names then
fails at startup with `unknown runtime role '<name>'`. The GitHub release
asset is built `--all-features` and knows every role.

Install from the release assets:

```sh
cargo binstall mcpmem            # fetches the latest release asset (x86_64
                                 # and aarch64 Linux and macOS are available)
# or download mcpmem-v<tag>-<platform>.tar.gz from the GitHub release page
```

Launch the server with the config and the tool categories it must expose:

```sh
mcpmem --config mcpmem.toml --enable-all
```

- `roles` live in the TOML, under `roles`. Runtime roles are `mcp`,
  `indexer`, `webhooks` and `extractor`.
- The `--enable-*` flags expose tool categories to clients. With no flag, no
  category is exposed, even when a role is active.

### Attachments are a flag, not a role

`attachments` is **not** a runtime role, and it does not belong in `roles`.
Three separate things make attachments work:

1. The `extractor` role processes uploads (`roles = [..., "extractor"]`).
2. The caller holds the OAuth/tool scope `attachments` (in `principals.json`
   or the consent grant).
3. The server starts with `--enable-attachments` (or `--enable-all`).

The `[attachments]` TOML section only tunes limits (`max-bytes`,
`workspace-byte-budget`, `allow-mime`); an absent section keeps the defaults.

### A restart invalidates OAuth token families

A served token is a row in the OAuth store; a family that predates the
latest process start is dead, and a client that only retries a dead refresh
token stays stuck at `401`. After every deploy: re-authorize MCP clients in
the browser flow. Browser logins mint fresh families automatically. The omp
client needs the stale credential cleared before it offers the interactive
flow.
