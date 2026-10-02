# File attachment plan preflight

The prior implementation plan was not executable. The owner has now approved the schema, upload, and scope contracts. Use the corrected spec and plan under `docs/superpowers/`. The stop in this analysis is superseded.

## Root-cause check

- **Hypothesis:** The plan treats file attachments as only three new tables and one new owner enum. The merged graph also enforces migration, owner, queue, and transport contracts.
- **Evidence:** The workspace registry requires `max(version) = 14` in `src/workspace.rs:209-219`. The merged migration registry includes version 14 in `crates/mcpmem-core/src/events.rs:45-75`. Migration 0009 restricts owner and chunk kinds in two tables (`crates/mcpmem-core/migrations/0009_chunked_embeddings.sql:1-31`). The plan adds neither a safe version-15 transition nor changes to those CHECK constraints.
- **Rejected alternative:** The workspace code is not missing. PR #55 merged on 2026-10-02, and `origin/main` at `4f05262` has `src/workspace.rs`. The prior investigation used the old design branch and missed the merged code.

## Other failures in the plan

The findings below refer to the former plan. Its line numbers no longer point to the corrected plan.

1. `crates/mcpmem-core/src/jobs.rs:35-60,293-299,473-480,580-623,626-686` owns profile selection, rebuild, revision fences, type IDs, and full-scan checks. The plan's profile query in `docs/superpowers/plans/2026-10-02-file-attachments.md:266-278` does not select profile IDs and does not extend these paths.
2. `crates/mcpmem-core/src/jobs.rs:514-535` gives each chunk a sequential index. A split page uses multiple indexes. The plan's `page = chunk_index + 1` mapping in `docs/superpowers/plans/2026-10-02-file-attachments.md:608,700-725` loses the page and segment text. The code has no existing token splitter (`crates/mcpmem-indexer/src/provider.rs:13-33`). A durable `(attachment_id, chunk_index, page, segment, text)` mapping is needed.
3. The server caps each MCP message at 16 MiB (`src/server.rs:126-129`). The HTTP router applies the same cap (`src/http.rs:280-290`). A 50 MiB file cannot use the promised single-call base64 upload. An upload protocol decision is needed before the tool contract is changed.
4. The HTTP routes live in `src/http.rs:233-291`, not `src/server.rs`. The graph inspector lives in `src/ui/index.html:49-73`, not in the admin shell. An attachment entity panel belongs in the inspector, unless the owner wants a separate admin workflow.
5. The OpenAI embedding URL is an embeddings endpoint (`crates/mcpmem-indexer/src/openai.rs:12-51`), not a vision endpoint. The OCR API key can inherit; the OCR URL needs its own endpoint contract. The spec says an unknown OCR provider fails the job (`docs/superpowers/specs/2026-10-02-file-attachments-design.md:165-171`), while the plan rejects it during config parse (`docs/superpowers/plans/2026-10-02-file-attachments.md:332-335`).
6. The approved spec calls the filename index per entity but defines uniqueness on `(workspace_id, filename)` (`docs/superpowers/specs/2026-10-02-file-attachments-design.md:119-120`). Each workspace already has a separate graph file (`src/workspace.rs:1`). The schema choice must be explicit.

## Proposed correction

Use a new migration after 0014. Keep the old migration checksum fixed. Preserve the version guard: a current binary accepts its current maximum and verifies that marker 14 exists; an older binary still refuses a newer graph. Add owner-kind support to all index and rebuild paths. Persist each chunk's page and exact text. Use an explicit OCR vision URL while reusing the primary OpenAI API key. Place the browser UI in the graph inspector and its HTTP routes in `http.rs`.

The owner approved a streamed HTTP upload, chunked MCP upload, and a graph-local attachment schema. The corrected plan uses migration 15. Do not repeat the approval stop from the former plan.
