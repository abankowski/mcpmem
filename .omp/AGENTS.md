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

The repository's pre-flight mirrors the CI pipeline in
`.github/workflows/ci.yml`. Run it after all workers stop. This command is
identical in Bash and fish; the Bash child runs the shared command string
and writes the marker only after all checks succeed:

```sh
env OMP_PREFLIGHT_CMD='cargo fmt --all --check && scripts/check-release-version.sh && scripts/check-crate-includes.sh && command -v pdfinfo && command -v pdftoppm && pdfinfo -v && pdftoppm -v && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-targets -- --test-threads=1 && cargo test --test indexer_worker --features indexer -- --test-threads=1 && cargo test --test indexer_worker --no-default-features --features indexer -- --test-threads=1 && cargo test --test attachment_extraction --features extractor -- --test-threads=1 && cargo test --test attachment_extraction --features extractor pdf_render_routes_page_to_vision -- --exact --test-threads=1 && cargo test --test attachment_mcp --test attachment_http --features extractor -- --test-threads=1 && cargo test --test vector_e2e --test semantic_search --test ui_http --features extractor -- --test-threads=1 && cargo test --lib --features indexer -- --test-threads=1 && cargo test --test role_composition --no-default-features && cargo test --test role_composition --features indexer && cargo test --test role_composition --features webhooks && cargo test --test role_composition --features extractor && cargo test --test role_composition --no-default-features --features extractor && cargo test --test role_composition --features extractor,webhooks && cargo test --test role_composition --features indexer,webhooks && cargo test --test webhook_tools --test webhook_admin --features webhooks -- --test-threads=1 && cargo package -p mcpmem-core --locked --allow-dirty' bash -c 'eval "$OMP_PREFLIGHT_CMD" && git rev-parse HEAD > "$(git rev-parse --git-dir)/omp-preflight-pass"'
```

The exact chain to run before every push/PR (identical in Bash and fish):

1. `cargo fmt --all --check`
2. `scripts/check-release-version.sh`
3. `scripts/check-crate-includes.sh`
4. `command -v pdfinfo && command -v pdftoppm && pdfinfo -v && pdftoppm -v`
5. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
6. `cargo test --workspace --all-targets -- --test-threads=1`
7. `cargo test --test indexer_worker --features indexer -- --test-threads=1` and `cargo test --test indexer_worker --no-default-features --features indexer -- --test-threads=1`
8. `cargo test --test attachment_extraction --features extractor -- --test-threads=1` and `cargo test --test attachment_extraction --features extractor pdf_render_routes_page_to_vision -- --exact --test-threads=1`
9. `cargo test --test attachment_mcp --test attachment_http --features extractor -- --test-threads=1`
10. `cargo test --test vector_e2e --test semantic_search --test ui_http --features extractor -- --test-threads=1`
11. `cargo test --lib --features indexer -- --test-threads=1`
12. `cargo test --test role_composition` for `--no-default-features`, `--features indexer`, `--features webhooks`, `--features extractor`, `--no-default-features --features extractor`, `--features extractor,webhooks`, and `--features indexer,webhooks`
13. `cargo test --test webhook_tools --test webhook_admin --features webhooks -- --test-threads=1`
14. `cargo package -p mcpmem-core --locked --allow-dirty` — `--allow-dirty` because the local tree carries uncommitted integration work; CI's clean checkout uses plain `--locked`
15. Write the marker only after success: `git rev-parse HEAD > "$(git rev-parse --git-dir)/omp-preflight-pass"`

The `-D warnings` clippy must use `--all-features`; the plain default-feature
run fails on pre-existing findings unrelated to the change.

## Attachment tests need Poppler on PATH

`cargo test --test attachment_extraction --features extractor` includes the
real-PDF case `pdf_render_routes_page_to_vision`, which **fails** — it never
skips — when the external Poppler commands are missing. Install the poppler
package for your OS (`poppler-utils` on Debian/Ubuntu, `poppler` via
Homebrew) and confirm both commands before running the attachment tests. The
check command is identical in Bash and fish:

```sh
command -v pdfinfo && command -v pdftoppm && pdfinfo -v && pdftoppm -v
```

The CI and release workflows install the package and run the same check; a
release target without both commands blocks the release, because a missing
renderer is an observable `render` failure, never a skipped test.

## Second Brain workflow

The Second-Brain MCP (brain-1) is the cross-project knowledge graph for this
work. `rule://second-brain` (global) and `skills/second-brain-memory` define
when to pull context and when to feed it. The assessment and event-to-action
mapping live in `docs/second-brain-workflow.md`; this repository is the
server that backs the brain.

## Hosts

`.omp/ssh.json` maps `brain-1` to `192.168.1.54`, the deployment host that
runs the second-brain memory server (`~/mcpmem/`).