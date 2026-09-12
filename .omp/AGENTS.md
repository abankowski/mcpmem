# mcpmem repository rules

Codebase-specific working agreements. The transferable rules live in
`~/.omp/agent/AGENTS.md` and always apply.

## Pre-flight before publishing a PR

The push/publish guard (`require-preflight.py`) looks for the repository's
pre-flight and blocks `gh pr create` / `gh pr edit --body` / the first branch
push until it passes on the exact commit. This repository has **no** script
the guard can auto-discover, so its Cargo default would be used. That default
(`cargo clippy --all-targets -- -D warnings` without `--all-features`) fails
on this tree, because the default-feature build trips pre-existing lint
findings (`src/config_file.rs` unused `BTreeSet`,
`src/server.rs:919` `semantic_search_available` is `const fn`). The finding is
real only under default features; CI itself lints `--all-features` and is
green. Do not try to make the default run pass.

The repository's pre-flight is the CI pipeline
(`.github/workflows/ci.yml`), mirrored locally. Run it with the marker write,
exactly as one command:

```sh
env OMP_PREFLIGHT_CMD="cargo fmt --all --check && scripts/check-release-version.sh && scripts/check-crate-includes.sh && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-targets -- --test-threads=1 && cargo test --test indexer_worker --features indexer -- --test-threads=1 && cargo test --test indexer_worker --no-default-features --features indexer -- --test-threads=1 && cargo test --test role_composition --no-default-features && cargo test --test role_composition --features indexer && cargo test --test role_composition --features webhooks && cargo test --test role_composition --features indexer,webhooks && cargo test --test webhook_tools --test webhook_admin --features webhooks -- --test-threads=1 && cargo package -p mcpmem-core --locked --allow-dirty && git rev-parse HEAD > \"\$(git rev-parse --git-dir)/omp-preflight-pass\"" bash -c '<the same command>'
```

Simpler form using the override only (the guard then suggests and accepts it):

```sh
env OMP_PREFLIGHT_CMD="cargo fmt --all --check && scripts/check-release-version.sh && scripts/check-crate-includes.sh && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-targets -- --test-threads=1 && cargo test --test indexer_worker --no-default-features --features indexer -- --test-threads=1 && cargo test --test role_composition --features indexer,webhooks && cargo test --test webhook_tools --test webhook_admin --features webhooks -- --test-threads=1 && cargo package -p mcpmem-core --locked --allow-dirty && git rev-parse HEAD > \"\$(git rev-parse --git-dir)/omp-preflight-pass\"" [the command]
```

The exact chain to run before every push/PR (identical in Bash and fish):

1. `cargo fmt --all --check`
2. `scripts/check-release-version.sh`
3. `scripts/check-crate-includes.sh`
4. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
5. `cargo test --workspace --all-targets -- --test-threads=1`
6. `cargo test --test indexer_worker --features indexer -- --test-threads=1` and `--no-default-features --features indexer`
7. `cargo test --test role_composition` for `--no-default-features`, `--features indexer`, `--features webhooks`, `--features indexer,webhooks`
8. `cargo test --test webhook_tools --test webhook_admin --features webhooks -- --test-threads=1`
9. `cargo package -p mcpmem-core --locked`
10. write the marker: `git rev-parse HEAD > "$(git rev-parse --git-dir)/omp-preflight-pass"`

The `-D warnings` clippy must use `--all-features`; the plain default-feature
run fails on pre-existing findings unrelated to the change.

## Hosts

`.omp/ssh.json` maps `brain-1` to `192.168.1.54`, the deployment host that
runs the second-brain memory server (`~/mcpmem/`).