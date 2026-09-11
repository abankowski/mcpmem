# Query-side embedding: what the server does and does not compute

Analysis of `main` at `b2e4044`. Every fact below carries a `path:line`.
Six read-only agents produced the evidence; the decisive negative claims were
re-checked by hand.

Every line number in this document belongs to commit `b2e4044`. The change set
that followed this analysis moved some of those lines. The numbers here stay as
they were, so read them against `b2e4044`.

## Answer in one paragraph

The caller must send the vector on every read path. No MCP tool takes text and
gives back a vector. The write-side "auto-embedding" is real code but it is
**inert in every shipped build**, for two independent reasons: the `indexer`
Cargo feature is not in `default`, and no production code path ever creates an
`IndexProfile`, so the job queue holds every row and the worker claims none.
So the correct statement today is: *the server never computes an embedding,
on read or on write.*

## 1. Read path: the four tools

| Tool | Query input | Text accepted? |
|---|---|---|
| `vector_search_entities` | `embedding` (required) | no |
| `hybrid_search` | `queryText` **and** `queryEmbedding` (both required) | text yes, but FTS5 only |
| `vector_mmr_search` | `embedding` (required) | no |
| `vector_recommend` | `positive` entity names | no — averages **stored** vectors |
| `vector_search_by_entity` | `entityName` | no — reuses the **stored** vector |

- `hybrid_search` takes `queryText` at `src/vector_actions.rs:185`, and that
  string reaches only `kg.search_nodes_filtered` at `src/vector_actions.rs:244`.
  It is never embedded.
- The empty-array error is `"Embedding must not be empty"`,
  `src/vector_actions.rs:35`, raised by the shared `parse_embedding`
  (`src/vector_actions.rs:29-50`).
- Entity dimension default is **384**, set by `--embedding-dims`
  (`src/lib.rs:275-276`). There is no named constant for it.
- The search path does **not** check the query length against `dims`.
  `search_embeddings` (`src/vector_store.rs:824-856`) passes the slice straight
  to the index. An empty store short-circuits to `Ok(vec![])` at
  `src/vector_store.rs:846`, so a wrong-size query returns `count: 0`, not an
  error. Only the write path compares (`src/vector_store.rs:735-739`).

So the description "outside chat" for these four tools is correct and should
stay.

## 2. Write path: real code, unreachable in practice

The embedding worker exists and calls a model over the network:

1. A graph mutation enqueues a job — `crates/mcpmem-core/src/jobs.rs:9`.
2. The `indexer` role polls every 250 ms — `src/runtime.rs:149-183`.
3. The worker claims a leased job — `crates/mcpmem-indexer/src/lib.rs:151`.
4. It builds a canonical document (name, type, observations joined by `\n`) —
   `crates/mcpmem-indexer/src/provider.rs:16-24`.
5. **The only `embed` call site** — `crates/mcpmem-indexer/src/lib.rs:179-181`,
   dispatching on `provider_kind` to Ollama, OpenAI-compatible or Bedrock
   (`crates/mcpmem-indexer/src/lib.rs:104-133`).
6. The vector is committed to `profile_vector` —
   `crates/mcpmem-core/src/jobs.rs:383`.

Two blockers make this dead code in any real deployment:

- **Feature.** `default = ["code", "oauth"]` (`Cargo.toml:82`); `indexer` is
  opt-in (`Cargo.toml:83`) and `mcpmem-indexer` is an optional dependency
  (`Cargo.toml:48`). A stock binary does not link the providers at all.
- **No profile.** `enqueue_change` writes state `pending` only when a serving or
  candidate profile exists; otherwise it writes the nil profile with state
  `held` (`crates/mcpmem-core/src/jobs.rs:23-31`). `claim_due` never returns a
  held row. The only constructor of an `IndexProfile` in the tree is a test at
  `src/vector_store.rs:1520`, and `begin_rebuild`
  (`crates/mcpmem-core/src/jobs.rs:193`) has test callers only —
  `tests/indexer_worker.rs`, `tests/event_outbox.rs`, `src/vector_store.rs:1533`.
  No CLI flag, no MCP tool, no maintenance subcommand creates one
  (`src/bin/maintenance.rs` offers `audit` and `repair` only).

`IndexProfileRegistry::state` therefore stays `LegacyCompat`
(`crates/mcpmem-core/src/jobs.rs:159`), `serving_profile` returns `None`
(`:177`), and direct vector writes stay allowed (`:184-191`).

## 3. Code path: the `code_embed` description is correct

No correction is needed at the endpoint.

- `handle_code_embed` reads `item["embedding"]` and errors `"missing embedding"`
  when absent — `src/actions/code.rs:684-693`. No fallback embeds text.
- `handle_code_semantic_search` requires `embedding`; there is no `query` or
  `text` parameter — `src/actions/code.rs:715-722`, schema
  `code_tools.json:229-231`.
- Dimension 768 comes from `--code-embedding-dims` (`src/lib.rs:280-281`),
  through `src/config.rs:379` to `code_vec_registry::init`
  (`src/server.rs:436`).
- The indexer never touches the code index. It writes `profile_vector`
  (`crates/mcpmem-core/src/jobs.rs:383`) in the main memory file
  (`src/main.rs:67-70`), while code vectors live in `<project>.code.db`
  (`src/code_vec_registry.rs:96-101`). No reference to `code_vec_registry` or
  `.code.db` exists anywhere under `crates/mcpmem-indexer/`.

## 4. What a `semantic_search(text)` tool would cost

The provider is unreachable from tool dispatch. `MCPServer` holds `config`, `kg`
and `vs` only (`src/server.rs:363-369`), and `handle_tools_call` takes
`(req, kg, vs, principal)` (`src/server.rs:877-881`). The single live
`ProviderRegistry` is a private field of `IndexerService`
(`src/runtime.rs:112`), built only when the `indexer` role is selected
(`src/main.rs:62-70`).

Smallest change set, in dependency order:

1. `crates/mcpmem-indexer/src/provider.rs:32-38` — add
   `embed_text(&self, profile, texts)`. The trait today takes
   `&[CanonicalDocument]`, whose `entity_id` and `revision` fields exist for
   revision fencing; a query has neither, and faking them would put lies in the
   fencing struct. Both providers already collapse documents to strings before
   the request (`ollama.rs:37`, `openai.rs:32`), so `embed` can be re-expressed
   over the new method and no provider gains a second HTTP path.
2. `crates/mcpmem-indexer/src/lib.rs:104-130` — mirror the strict
   `provider_kind` match; an unknown kind must keep failing.
3. `src/vector_store.rs` — add a public accessor for the serving profile next to
   `dims()` (`:1098`). `db` and `managed_snapshot` are private (`:285`, `:290`)
   and no method returns a profile today, so a handler cannot learn which model
   or dimension the target index uses.
4. `src/server.rs` — thread `Option<&ProviderRegistry>` into `MCPServer` and
   `handle_tools_call`, or use the `OnceLock` pattern already present twice in
   the file (`CODE_ENABLED` at `:868`, `code_vec_registry::init` at `:436`).
   Dispatch already runs under `spawn_blocking` (`src/server.rs:637`,
   `src/http.rs:422`), so the providers' blocking client is safe there.
5. `src/vector_actions.rs` — the handler, reusing `opt_usize` (`:53`),
   `MAX_TOP_K` (`:11`) and `with_scratch` so the query vector does not allocate.
6. Registration in three places that must agree: `VECTOR_TOOL_NAMES`
   (`src/tools.rs:197-210`), `vector_tools.json` (compiled in at
   `src/server.rs:783`), and the dispatch arm (`src/server.rs:912-948`).
   Scope gating needs no edit: `category_of` (`src/tools.rs:255-257`) maps any
   name in the list to `ToolCategory::Vectors`.

Risks:

- **The profile gap in section 2 is the real blocker.** Without a profile there
  is no provider kind, no model and no dimension for the tool to use. Profile
  creation needs an entry point first. This is a prerequisite, not a detail.
- Query vectors are never normalised. Stored vectors are L2-validated at commit
  (`crates/mcpmem-core/src/jobs.rs:99-106`); the query side has no equivalent
  (`src/vector_store.rs:826-834`). The new tool must normalise itself.
- Model mismatch is undetectable. `profile_vector` stores `source`, not a model
  (`crates/mcpmem-core/src/jobs.rs:378-380`); the legacy `vector_embedding.model`
  column is free text and is never compared at search time. Same dimension plus
  a different model returns silently wrong rankings.
- With `indexer` off, the tool must be absent from `tools/list`, not fail on
  call. `VECTOR_TOOL_NAMES` (`src/tools.rs:197`) is a plain `const` with no
  `cfg`.
- A `--role mcp` process holds no `ProviderRegistry` and would need its own.
- A provider outage holds a blocking-pool thread for the full 10-second timeout
  (`src/runtime.rs:121`). Nothing caches query embeddings.
- Provider configuration bypasses `Config` entirely: transport and credentials
  come from `MCP_MEMORY_OLLAMA_URL`, `MCP_MEMORY_OPENAI_URL` and
  `MCP_MEMORY_OPENAI_API_KEY` (`crates/mcpmem-indexer/src/lib.rs:75-99`), while
  kind and model come from the database row.

## 5. Defects found on the way

Each one is a document that contradicts the code.

1. **`README.md:464-466`** — **Fixed by the change set that followed this
   analysis.** The sentence was "The server stores and searches vectors; it does
   not call an embedding model." It had no scope qualifier, yet `README.md:741`
   calls `mcpmem-indexer` "the durable embedding worker and its providers". The
   README disagreed with itself. The sentence was accurate for a stock build and
   false for an `indexer` build. The unqualified sentence is gone. The README now
   names the build and the path that the claim covers.
2. **`vector_tools.json:112`** — `indexKind` documented as "'hnsw' or 'ivf'";
   the code also returns `"turboquant"` (`src/vector_actions.rs:361`).
3. **`vector_tools.json:69`** — `hybrid_search` "runs vector search and FTS5
   simultaneously". The two run sequentially on one thread
   (`src/vector_actions.rs:236`, `:239`). The same line calls the centrality
   boost "optional"; it applies whenever `graph_node_count() > 0`
   (`src/vector_actions.rs:268-289`) and no parameter disables it.
4. **`vector_tools.json:197`** — `vector_reindex` "for HNSW it is a no-op".
   TurboQuant is equally a no-op (`src/vector_store.rs:161-165`), yet the
   handler returns `"reindexed": true` unconditionally
   (`src/vector_actions.rs:724`).
5. **`vector_search_entities` under-returns.** It fetches exactly `top_k` then
   drops rows failing the type filter (`src/vector_store.rs:945`, `:961-966`),
   so a filtered call can return fewer than `topK` while matching entities
   exist. The sibling path over-fetches `3 * top_k` for this reason
   (`src/vector_store.rs:1206`). The description does not say so, and the
   emitted `score` is a distance, which only `code_tools.json:197` documents.
6. **`--embedding-dims` is unvalidated against the request cap.** Set it above
   4096 and the server starts, then every upsert fails at
   `src/vector_actions.rs:38` before the store check at `:735` can match.
7. **`docs/analysis/2026-09-07-unified-memory-runtime-contracts.md:34-36`**
   describes a `mcpmem migrate --check` command. No such command exists in
   `src/lib.rs`, `src/main.rs` or `src/bin/`.

## 6. What to tell the skill author

- Keep the four vector tools marked "outside chat". The premise is correct.
- Keep the `code_embed` description as it stands. It is accurate.
- Correct one detail: `hybrid_search` does accept `queryText`, but only as an
  FTS5 term; the vector is still mandatory.
- `vector_search_by_entity` and `vector_recommend` need **no** vector from the
  caller. They build the query from stored vectors
  (`src/vector_actions.rs:511-546`, `:551-624`). They are usable from chat today
  and belong on the "inside chat" list.
