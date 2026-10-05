# mcpmem landing page content
#
# One `key: value` per line under a section heading. Edit the values, run
# `node build.mjs` in this directory, and the new page lands in `dist/`.
# The build fails if a key is missing or unused, so the template and this
# file cannot drift silently. Lines starting with '#' are comments.

## Page metadata (head)

meta.description: mcpmem is an MCP server with a persistent knowledge graph, code intelligence and semantic search. One Rust binary, one SQLite file, no telemetry.
og.title: mcpmem - give your agent a brain that remembers
og.description: Persistent knowledge graph, code intelligence and semantic search for LLM agents. One binary, one file.
og.image: og.png
title: mcpmem - persistent memory, knowledge graph, code intelligence and semantic search for LLM agents

## Hero

hero.tags.1: Rust
hero.tags.2: MCP 2025-11-25
hero.tags.3: Apache-2.0
hero.tags.4: one SQLite file
hero.h1: Give your agent a brain that remembers.
hero.sub: mcpmem is an MCP server with a persistent knowledge graph, code intelligence and semantic search. One binary, one file, no database to run, no telemetry.
hero.cta.primary.href: https://github.com/abankowski/mcpmem
hero.cta.primary: Star on GitHub
hero.cta.secondary: cargo install mcpmem
hero.meta: Works with Claude Desktop, Claude Code and any MCP client.

## What it does

what.eyebrow: What it does
what.title: Three memories, one server
what.intro: Each is a category of MCP tools you switch on with a flag. Compile only what you need.

pillars.1.title: Knowledge graph
pillars.1.body: Entities, directed relations and timestamped observations. Key-value attributes on both nodes and edges. FTS5 search, paths, neighbours, subgraphs, merge and rename.
pillars.1.flag: --enable-graph-read --enable-graph-write
pillars.2.title: Code intelligence
pillars.2.body: Point it at a repo. tree-sitter parses 11 languages into symbols and outlines, so a coding agent navigates by name instead of reading whole files.
pillars.2.flag: --enable-code
pillars.3.title: Semantic search
pillars.3.body: The server embeds entities, observations, relations and the contents of files attached to nodes, then fuses vector similarity with full-text relevance and graph centrality. Semantic, hybrid and MMR retrieval.
pillars.3.flag: --enable-vectors

## How it works

how.eyebrow: How it works
how.title: Install, add two lines, stop forgetting
how.intro: A single binary speaks MCP over stdio for local agents or over HTTP with OAuth 2.1 for remote connectors. Everything lives in one embedded SQLite file per workspace, so backup is a copy and migration is a move.

facts.transport: stdio · HTTP (streamable)
facts.storage: SQLite, WAL mode, one file per workspace
facts.embeddings: Ollama, OpenAI-compatible, Amazon Bedrock
facts.files: attach PDF, markdown, text or images to a node; text is extracted, chunked and embedded
facts.code: register a Git repo in the admin panel; reindex on demand or on a watch
facts.events: signed webhooks on create, update, delete, rename
facts.telemetry: none
facts.models: cheap and open - Ollama, any OpenAI-compatible endpoint, or Amazon Bedrock. No cloud account required.
facts.scale: from one laptop to a shared project or organization brain - many users, per-grant visibility.

## Automation

automation.eyebrow: Automation
automation.title: Not just memory - a workflow engine
automation.intro: mcpmem reacts to events and exposes its graph over MCP, so tools like Node-RED and n8n read, write and remember on your behalf. A set of decoupled workflows automates data processing and keeps the knowledge yours.

## Works with

works.eyebrow: Works with
works.1: Claude Desktop
works.2: Claude Code
works.3: opencode
works.4: codex
works.5: n8n via webhooks
works.6: any MCP client

## The UI

ui.eyebrow: The UI
ui.title: See what your agent knows
ui.intro: A built-in web viewer: force-directed graph, node and relation inspector, direct and semantic search, files on nodes with their contents indexed, and an admin panel for principals, scopes, webhooks and workspaces.

## Security

security.eyebrow: Security
security.title: Fail closed by default

security.card.1.title: OAuth 2.1 with scopes
security.card.1.body: Remote clients authorize through your identity provider and get only the scopes you tick: graph-read, graph-write, vectors, code, admin.
security.card.2.title: Workspaces
security.card.2.body: Each graph has an owner, a visibility and reader or writer grants. A private workspace you cannot access looks exactly like one that does not exist.
security.card.3.title: Nothing leaves the box
security.card.3.body: No telemetry, no cloud dependency. Webhooks are signed and opt-in; the worker runs empty unless you configure it.

## Closing call to action

cta.title: Ready when your agent is.
cta.primary.href: https://github.com/abankowski/mcpmem
cta.primary: Star on GitHub
cta.secondary: crates.io

## Footer

footer.left: Apache-2.0 · by Artur Bankowski