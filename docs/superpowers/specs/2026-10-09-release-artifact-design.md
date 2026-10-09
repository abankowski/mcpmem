# Release Artifact Pipeline — Design

**Status:** Approved on 2026-10-09.

`version` validates the tag, verifies main ancestry, and must find a completed successful `CI` push run on main for the exact tag SHA. `frontend` builds `ui-dist`. One four-target matrix restores that artifact, runs the existing PDF test, builds the release archive, smoke-tests Linux x64, and uploads Actions artifacts.

`publish` waits for the matrix, restores `ui-dist`, and publishes crates. It does not rerun the workspace suite or rebuild the smoke binary. The six library crates use `--no-verify`; root `mcpmem` retains full verification and `--allow-dirty`. `upload-binaries` waits for publish and attaches the four archives to a published GitHub release. `bump` still waits only for publish.

Do not add a token write permission. Caches can restore from main but do not provide the release correctness contract.