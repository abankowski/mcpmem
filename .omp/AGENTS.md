# mcpmem repository rules

Codebase-specific working agreements. The transferable rules live in
`~/.omp/agent/AGENTS.md` and always apply.

## README is the public record of features

Every user-facing feature ships with its README section in the same change —
the README is where a capability is described for the first time. The Second
Brain graph is private memory and never a substitute for repo documentation;
a feature that lands in code and CHANGES but nowhere in README is
undocumented. (Gap this rule closes: webhook delivery logging and the
startup audit shipped without README coverage on 2026-09-15.)

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

The repository's Rust pre-flight mirrors the Rust CI jobs. It covers every
local Rust check and test group. CI runs the frontend, site, package, and
browser jobs separately.

Install cargo-nextest before the first pre-flight. Run this command check after
the install. It fails if cargo cannot find cargo-nextest:

```sh
cargo install cargo-nextest --locked
cargo nextest --version
```

Run the chain with the marker write, exactly as one command:

```sh
env OMP_PREFLIGHT_CMD="cargo fmt --all --check && scripts/check-release-version.sh && scripts/check-crate-includes.sh && cargo package -p mcpmem-core --locked --allow-dirty && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo nextest run --workspace --all-targets && cargo nextest run --test indexer_worker --features indexer && cargo nextest run --test indexer_worker --no-default-features --features indexer && cargo nextest run --test attachment_extraction --features extractor && cargo nextest run --test attachment_extraction --features extractor -- pdf_render_routes_page_to_vision --exact && cargo nextest run --test attachment_mcp --test attachment_http --features extractor && cargo nextest run --test vector_e2e --test semantic_search --test ui_http --features extractor && cargo nextest run --lib --features indexer && cargo nextest run --test role_composition --no-default-features && cargo nextest run --test role_composition --features indexer && cargo nextest run --test role_composition --features webhooks && cargo nextest run --test role_composition --features extractor && cargo nextest run --test role_composition --no-default-features --features extractor && cargo nextest run --test role_composition --features extractor,webhooks && cargo nextest run --test role_composition --features indexer,webhooks && cargo nextest run --test webhook_tools --test webhook_admin --features webhooks && cargo nextest run --test ui_router --test oauth_flow --features ui && cargo nextest run --test ui_router --no-default-features && cargo nextest run --test ui_router --no-default-features --features ui && cargo nextest run --test oauth_flow --no-default-features --features oauth && bash -c 'if cargo tree --no-default-features -e normal | rg -q \"(^| )(reqwest|aws-[a-z0-9-]+|aws_sdk_[a-z0-9_]+) v\"; then echo \"graph-only build unexpectedly includes an HTTP or AWS client\"; exit 1; fi' && git rev-parse HEAD > \"\$(git rev-parse --git-dir)/omp-preflight-pass\"" bash -c '<the same command>'
```

The exact chain to run before every push/PR (identical in Bash and fish):

1. `cargo fmt --all --check`
2. `scripts/check-release-version.sh`
3. `scripts/check-crate-includes.sh`
4. `cargo package -p mcpmem-core --locked`
5. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
6. `cargo nextest run --workspace --all-targets`
7. `cargo nextest run --test indexer_worker --features indexer` and `--no-default-features --features indexer`
8. `cargo nextest run --test attachment_extraction --features extractor`, then the real-PDF leg `-- pdf_render_routes_page_to_vision --exact`
9. `cargo nextest run --test attachment_mcp --test attachment_http --features extractor`
10. `cargo nextest run --test vector_e2e --test semantic_search --test ui_http --features extractor`
11. `cargo nextest run --lib --features indexer` (the taxonomy and workspace-indexer unit tests are gated on `indexer`; the workspace leg never builds them)
12. `cargo nextest run --test role_composition` for `--no-default-features`, `--features indexer`, `--features webhooks`, `--features extractor`, `--no-default-features --features extractor`, `--features extractor,webhooks`, `--features indexer,webhooks`
13. `cargo nextest run --test webhook_tools --test webhook_admin --features webhooks`
14. `cargo nextest run --test ui_router --test oauth_flow --features ui`, `--test ui_router --no-default-features`, `--test ui_router --no-default-features --features ui`, and `--test oauth_flow --no-default-features --features oauth`
15. the graph-only dependency guard: `cargo tree --no-default-features -e normal` must not list `reqwest` or any `aws*` crate
16. write the marker: `git rev-parse HEAD > "$(git rev-parse --git-dir)/omp-preflight-pass"`

Legs 8–10 need real Poppler (`pdfinfo`, `pdftoppm`) on the PATH; CI installs
`poppler-utils` for them. Leg 15 needs `rg` (ripgrep) on the PATH; CI runners
ship it, a macOS machine needs `brew install ripgrep`.

The `-D warnings` clippy must use `--all-features`; the plain default-feature
run fails on pre-existing findings unrelated to the change.

## Second Brain workflow

The Second-Brain MCP (brain-1) is the cross-project knowledge graph for this
work. `rule://second-brain` (global) and `skills/second-brain-memory` define
when to pull context and when to feed it. The assessment and event-to-action
mapping live in `docs/second-brain-workflow.md`; this repository is the
server that backs the brain.

## Hosts

`.omp/ssh.json` maps `brain-1` to `192.168.1.54`, the deployment host that
runs the second-brain memory server (`~/mcpmem/`).