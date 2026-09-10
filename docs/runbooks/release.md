# Release runbook

`mcpmem` publishes to crates.io only for a published GitHub release. A push to
`main` never publishes. The workflow is `.github/workflows/release.yml`.

The commands below are identical in Bash and fish.

## The version rules

- The workspace holds five crates. All five carry the same version.
- A tag is `v` plus the version, for example `v1.0.0`.
- The version is strict semver 2.0.0. Build metadata is rejected: crates.io
  stores it, but no dependency can request it, so the release is unreachable.
- A prerelease version, for example `1.0.0-rc.1`, needs a GitHub release that
  is marked as a prerelease. The workflow fails when the two disagree.
- The released commit must be an ancestor of `origin/main`.

`scripts/check-release-version.sh` enforces every rule above. Ordinary CI runs
it without arguments. The release workflow adds `--registry`, which also
requires the version to be unpublished.

## Prepare a release

1. Set the new version in all five manifests: `Cargo.toml` and
   `crates/mcpmem-*/Cargo.toml`. Set the same version in each `path`
   dependency's `version` field.
2. Run the gate and the tests locally:

   ```sh
   scripts/check-release-version.sh --registry v1.1.0
   cargo test --workspace --all-targets --locked -- --test-threads=1
   ```

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
whole test suite, and then publishes.

## Publish order

`scripts/publish-crates.sh` publishes in dependency order:

1. `mcpmem-core`
2. `mcpmem-runtime`, `mcpmem-indexer`, `mcpmem-webhook`
3. `mcpmem`

`cargo publish` waits for each crate to appear in the index before it returns,
so the next crate resolves it.

The script skips a crate that crates.io already holds at this version. A
re-run after a partial failure therefore completes the release instead of
aborting on the crates that already went out.

## Dry run

`workflow_dispatch` accepts a tag and a `dry_run` flag, which defaults to true.
A dry run packages and verifies instead of publishing.

One limitation, and it is not a defect: `cargo publish --dry-run` verifies a
crate against crates.io, so a dependent cannot be verified before its
dependency is published. Before the first release, only `mcpmem-core` verifies.
The script reports the others and continues.

## Requirements in the repository settings

- A repository secret `CARGO_REGISTRY_TOKEN` with publish rights for all five
  crate names.
- An environment named `crates-io`. Add required reviewers there when a manual
  approval before publishing is wanted.
