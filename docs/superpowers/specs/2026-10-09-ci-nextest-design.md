# CI Parallelism and Nextest — Design

**Status:** Approved on 2026-10-09.

Split the serial `rust` job into `rust-checks`, `tests-default`, `tests-indexer`, `tests-extractor`, `tests-webhooks`, and `tests-roles`. Keep `version`, `frontend`, `site`, `ui-package`, `feature-matrix`, and `browser` contracts. `frontend` remains the owner of `ui-dist`.

Use `cargo nextest run` for every existing Rust test command. Remove `--test-threads=1`. Nextest starts separate test processes. Each process-owned fixture must retain a `TempDir` for its full life. A fixture Git command must set `commit.gpgSign=false` only for that command. Keep the browser job serial because it owns ports `8080`, `8092`, and `/tmp/mcpmem-e2e`.

Add cargo-nextest to contributor setup. Update README and `.omp/AGENTS.md` to name the same complete local pre-flight coverage. Do not change the release workflow or the main ruleset.