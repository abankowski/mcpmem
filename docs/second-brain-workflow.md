# Second Brain — workflow wiring

Date: 2026-09-13. This document assesses what the coding workflow should
store in the Second Brain and how it is wired in. The wiring: one global
rule (`rule://second-brain`), one skill
(`skill://second-brain-memory`), one session-end nudge
(`~/.omp/agent/guards/nudge-second-brain.py`).

## Current state

The Second Brain is a knowledge graph served by this repository's software
(`mcp-memory`, unified-memory-runtime) on `brain-1.tandk.pl`, mounted in the
omp harness as the `Second-Brain` MCP server (`~/.omp/agent/mcp.json`).

Live content (2026-09-13): 287 entities, 365 relations, 288 vector embeddings
(HNSW, 384 dims). Content was first mapped on 3.09.2026 from the cross-surface
Claude memory, the legacy Claude memory export, and Claude Code session
transcripts.

The graph already governs itself with three principles that this wiring adopts
as contract:

|Principle (entity name)|Content|
|---|---|
|`Zasada: encja generyczna plus atrybut`|Do not create one type per variant. A `System` carries attributes (whose, integration kind), a `Rola` carries name and engagement, an `Organizacja` carries client/partner/supplier.|
|`Zasada: granica między Artefaktem a Notatką`|An `Artefakt` has a stable address outside the brain. A `Notatka` is private working notes.|
|`Skill second-brain-memory` (Artefakt)|The skill is planned to define read and write, with a closed taxonomy as contract, not a suggestion.|

## What the workflow produces, and where each piece lands

The brain stores concepts, decisions and general lessons. The repository
stores implementation, verification and history. A fact belongs in exactly one
place; the brain references the repository instead of copying it.

|Workflow output|Lands in|Why|
|---|---|---|
|Code, specs, runbooks, analyses, commit history|Repository (a `docs/**` file is an `Artefakt` with a stable address)|Stable address, reviewable, versioned|
|Concepts: systems, projects, technologies, people, organizations, areas|Brain|Cross-project, cross-session|
|Decisions with rationale and rejected alternatives|Brain (`Decyzja`)|Needed in every later session|
|General lessons from incidents, with evidence|Brain (`Wniosek`, `Zasada`)|Must survive the session that learned them|
|Commitments and their close-outs|Brain (`Zobowiązanie`)|Live across projects|
|Constraints, risks, requirements|Brain (`Ograniczenie`, `Ryzyko`, `Wymaganie`)|Bound every design|

## Feed mapping — event to brain action

Feed after each event in the left column. One feed is one session's worth of
records, not one entity per commit.

|Event|Brain action|Type used|
|---|---|---|
|Feature delivered, PR taken to mergeable|Update the `Projekt` or `System` entity: what changed, when, notable constraints discovered|`Projekt`, `System`|
|Architectural decision taken|Create `Decyzja: <summary>` + observations (decision, why, alternative rejected and why); link with `dotyczy` to the project/system, `podjęło` from the person|`Decyzja`|
|Important bug fixed|Update `System`; create `Wniosek` or `Zasada` if the root cause generalizes — carry the evidence, not just the rule|`Wniosek`, `Zasada`, `System`|
|System integration|Update `System`/`Technologia`: integration kind, constraints, docs location; record an integration `Decyzja` when a choice was made; record a `Zobowiązanie` for pending external inputs|`System`, `Technologia`, `Decyzja`, `Zobowiązanie`|
|Commitment made or closed|Create or add a close-out observation (`Domknięte <date>: <outcome>`) to the `Zobowiązanie`|`Zobowiązanie`|
|Significant meeting|`Spotkanie` with conclusions, commitments, decisions; link participants and topics|`Spotkanie`, `Wniosek`, `Zobowiązanie`|
|Investigation with a general cause|Generalize into `Zasada` or `Wniosek`; keep the incident line in the repository|`Zasada`, `Wniosek`|
|Project phase or metric changes|Update `Faza`, `Metryka`, `Cel`|`Faza`, `Metryka`, `Cel`|

When the event does not fit the table, reuse the closest existing type. Do not
invent a type until `suggest_taxonomy` returns no suitable name (the taxonomy
is a contract, not a suggestion).

## Pull triggers — when to read

Pull before significant work on any subject that has a brain entity: a system,
project, technology, person, organization or area. Typical triggers:

- feature work on an existing system;
- integration work;
- a large refactor;
- review of a large PR;
- answering "what do we know about X";
- a new session touches a project that already has brain records.

The pull does not re-establish facts the brain already holds. It reads them
through `batch_get_entities` (exact names; `search_nodes` tokenizes and misses
hyphenated names), `search_nodes` / `semantic_search` for fuzzy matches, then
`describe_entity` / `get_neighbors` for context, and summarizes.

## Naming and quality bar for writes

- Entity names: `Typ: short name` for conceptual types (`Decyzja: …`,
  `Zasada: …`, `Wniosek: …`, `Zobowiązanie: …`); plain name for systems,
  technologies, people and organizations. Match the existing graph.
- One entity per concept. Add observations over time; do not create a second
  entity for the same concept. `upsert_entities` and `create_relations` are
  idempotent — rely on them and verify.
- Observations are complete sentences in the language of the existing content:
  fact, date, owner. Example: `Domknięte 4.09.2026 11:57 CEST: <outcome>`.
- A decision carries its rejected alternative and why, as in the existing
  `Zasada: rozdzielność Umowy, Zamówienia i Oferty` style.
- Reuse the relation set before inventing: `dotyczy`, `należy`, `prowadzi`,
  `pracuje`, `używa`, `pochodzi`, `wynika`, `jest klientem`, `jest partnerem`,
  `zależy od`, `podjęło`, `wymaga aktualizacji`.
- No manual vector writes. The server's indexer embeds changed entities in the
  background; the feed verifies with a re-read, not with vector tool calls.
- After any write, re-read the entity and check the relation list. The
  2026-09-10 audit (docs/analysis/2026-09-10-second-brain-audit.md) found one
  duplicate relation triple; the current `create_relations` skips an existing
  triple, but a re-read is the cheap proof.

## Mechanics

|Piece|Location|Role|
|---|---|---|
|Rule|`~/.omp/agent/rules/second-brain.md`|Always in context: when to pull, when to feed|
|Skill|`~/.omp/agent/skills/second-brain-memory/SKILL.md`|The procedure: taxonomy contract, pull flow, update flow, verification|
|Nudge|`~/.omp/agent/guards/nudge-second-brain.py`|Session-end reminder when significant work happened and the brain got no write|
|Wiring|`~/.omp/agent/extensions/omp-guards/index.ts`|Passes session evidence to the nudge|
|This document|`docs/second-brain-workflow.md`|The assessment and the reference|

The nudge fires only when the session shows evidence of significant work (a
push, a `gh pr create|ready`, or at least 8 non-test file writes) and no
Second-Brain write. The response is one action: feed the brain or reply
`skip`. It never blocks twice for the same message.