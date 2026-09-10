# OAuth authorization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the Claude custom connector and the ChatGPT connector reach `mcpmem` over HTTPS through an OAuth 2.1 flow.

**Architecture:** `mcpmem` becomes an OAuth 2.1 resource server and its own authorization server. An external OpenID Connect provider authenticates the human. `mcpmem` registers clients, shows consent, and issues opaque tokens bound to its own canonical URI. A new crate `mcpmem-oauth` holds the authorization server. The existing axum router gains eight routes.

**Tech Stack:** Rust 2024, axum 0.8, `axum-server` with rustls, `rusqlite` and SQLite WAL, `serde_json`, `sha2`, `base64`, `getrandom`, `reqwest`, `jsonwebtoken`.

**Spec:** `docs/superpowers/specs/2026-09-10-oauth-authorization-design.md`

## Global Constraints

- Base revision is `a0219c9`. Read the spec before the first task.
- Scope strings are the category slugs: `graph-read`, `graph-write`, `vectors`, `code`. They come from `ToolCategory::slug()` at `src/tools.rs:39`. Never write a second mapping.
- The canonical resource URI comes from `--public-url`. Never derive it from the `Host` header.
- Token tables hold a SHA-256 digest. They never hold a token value.
- `redirect_uri` comparison is an exact string match. No prefix match. No wildcard.
- The static bearer path at `src/http.rs:151` must keep working, with OAuth on and with OAuth off. With no auth configured the server stays open.
- Every new dependency needs a reason. Exactly one crate is new to `Cargo.lock`: `jsonwebtoken`, for upstream identity-token verification. `tower`, `http-body-util`, `base64`, `getrandom`, `subtle`, `reqwest` and `url` are already in the tree; the new crate and the development section only name them directly. Verify with `grep -A1 '^name = "<crate>"$' Cargo.lock` before you add one.
- The principals file is JSON, not TOML. The repository already ships JSON manifests, and JSON needs no new dependency.
- Each task is test-first. Each task ends with `cargo fmt --all --check` and a commit.
- Subagents never run git commands. The controller commits.
- Commands below are identical in Bash and fish.

---

## File structure

| File | Responsibility |
| --- | --- |
| `src/authz.rs` | build a `Principal`, and decide whether it may call a tool |
| `src/oauth_routes.rs` | axum handlers that adapt HTTP to `mcpmem-oauth` |
| `src/principals.rs` | read and validate the principals file |
| `crates/mcpmem-oauth/src/lib.rs` | crate root, public types, errors |
| `crates/mcpmem-oauth/src/store.rs` | SQLite persistence for clients, logins, codes, tokens |
| `crates/mcpmem-oauth/src/metadata.rs` | RFC 9728 and RFC 8414 documents |
| `crates/mcpmem-oauth/src/registration.rs` | dynamic client registration and metadata documents |
| `crates/mcpmem-oauth/src/upstream.rs` | OpenID Connect discovery, code exchange, JWKS verification |
| `crates/mcpmem-oauth/src/consent.rs` | consent page and authorization code issue |
| `crates/mcpmem-oauth/src/token.rs` | token grants, rotation, revocation, validation |
| `crates/mcpmem-core/migrations/0004_oauth.sql` | four tables |
| `tests/scope_gating.rs` | per-principal tool gating |
| `tests/oauth_store.rs` | store behaviour and expiry |
| `tests/oauth_flow.rs` | the whole flow against a fake provider |
| `tests/support/fake_idp.rs` | test-only OpenID Connect provider |

Wave 1 holds tasks 1, 2 and 3. They touch disjoint files and may run at the same time. Task 4 onward is sequential.

---

### Task 1: Per-principal tool gating

**Files:**
- Create: `src/authz.rs`
- Create: `tests/scope_gating.rs`
- Modify: `src/lib.rs` (add `pub mod authz;`)
- Modify: `src/tools.rs:249-271` (add `scope_of`)
- Modify: `src/errors.rs:4-25` (add `InsufficientScope`)
- Modify: `src/server.rs:244-261, 569-585, 691-729, 747-751`
- Modify: `src/http.rs:161-221`

**Interfaces:**
- Produces `mcpmem::authz::local_principal() -> Principal`, with every scope.
- Produces `mcpmem::authz::bearer_principal(scopes: &[ToolCategory]) -> Principal`.
- Produces `mcpmem::authz::allows_tool(p: &Principal, tool: &str) -> bool`.
- Produces `mcpmem::tools::scope_of(name: &str) -> Option<&'static str>`.
- Produces `mcpmem::server::HttpOutcome`, the new return type of `dispatch_http_body`.
- Consumes `mcpmem_core::auth::{Principal, PrincipalKind}` from `crates/mcpmem-core/src/auth.rs:8-19`.

- [ ] **Step 1: Write the failing derivation test.**

Add to `src/tools.rs`, inside the existing `mod tests` block at line 273:

```rust
#[test]
fn scope_strings_and_categories_match_in_both_directions() {
    for cat in ToolCategory::ALL {
        let scope = cat.slug();
        let back: ToolCategory = scope.parse().expect("slug parses back");
        assert_eq!(*cat, back, "slug {scope} did not round-trip");
    }
    // Every scope a tool can need must name a real category.
    for meta in ALL_TOOLS {
        let scope = scope_of(meta.name).expect("known tool has a scope");
        assert!(
            ToolCategory::ALL.iter().any(|c| c.slug() == scope),
            "tool {} produced unknown scope {scope}",
            meta.name
        );
    }
    assert_eq!(ToolCategory::ALL.len(), 4, "a new category needs a scope");
}
```

- [ ] **Step 2: Run it and watch it fail.**

```text
cargo test -p mcpmem --lib tools::tests::scope_strings_and_categories_match_in_both_directions
```

Expected: a compile error, because `scope_of` does not exist.

- [ ] **Step 3: Add `scope_of` to `src/tools.rs`, after `category_of` at line 262.**

```rust
/// The OAuth scope a tool needs, or `None` when the name is unknown. The scope
/// string is the category slug; there is no second mapping.
#[inline]
pub fn scope_of(name: &str) -> Option<&'static str> {
    category_of(name).map(ToolCategory::slug)
}
```

- [ ] **Step 4: Run the test and watch it pass.**

```text
cargo test -p mcpmem --lib tools::tests::scope_strings_and_categories_match_in_both_directions
```

- [ ] **Step 5: Write the failing gating test.**

Create `tests/scope_gating.rs`:

```rust
use mcpmem::authz::{allows_tool, bearer_principal, local_principal};
use mcpmem::tools::ToolCategory;

#[test]
fn a_read_only_principal_may_not_call_a_write_tool() {
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    assert!(allows_tool(&p, "read_graph"));
    assert!(!allows_tool(&p, "delete_entities"));
}

#[test]
fn a_local_principal_may_call_every_known_tool() {
    let p = local_principal();
    assert!(allows_tool(&p, "read_graph"));
    assert!(allows_tool(&p, "delete_entities"));
    assert!(allows_tool(&p, "hybrid_search"));
}

#[test]
fn an_unknown_tool_is_never_allowed() {
    assert!(!allows_tool(&local_principal(), "no_such_tool"));
}
```

- [ ] **Step 6: Run it and watch it fail.**

```text
cargo test --test scope_gating
```

Expected: a compile error, because `mcpmem::authz` does not exist.

- [ ] **Step 7: Create `src/authz.rs`.**

```rust
//! Who is calling, and what they may call.
//!
//! Scopes are the tool-category slugs (`graph-read`, `graph-write`, `vectors`,
//! `code`). The effective capability of a request is the intersection of the
//! process-wide `--enable-*` set and the scopes on the presented credential.

use std::collections::BTreeSet;

use mcpmem_core::auth::{Principal, PrincipalKind};

use crate::tools::{self, ToolCategory};

/// The stdio caller. stdio is local and unauthenticated, so it holds every
/// scope; the process-wide category flags still apply.
pub fn local_principal() -> Principal {
    principal_with("local", ToolCategory::ALL)
}

/// The principal behind the static bearer token, with operator-chosen scopes.
pub fn bearer_principal(scopes: &[ToolCategory]) -> Principal {
    principal_with("static", scopes)
}

/// A principal named by the OAuth layer, with the scopes the human approved.
pub fn oauth_principal(id: &str, scopes: BTreeSet<String>) -> Principal {
    Principal {
        id: id.to_owned(),
        kind: PrincipalKind::Human,
        scopes,
        allowed_origins: BTreeSet::new(),
    }
}

fn principal_with(id: &str, scopes: &[ToolCategory]) -> Principal {
    Principal {
        id: id.to_owned(),
        kind: PrincipalKind::Machine,
        scopes: scopes.iter().map(|c| c.slug().to_owned()).collect(),
        allowed_origins: BTreeSet::new(),
    }
}

/// `true` when the principal holds the scope this tool needs. An unknown tool
/// name is never allowed.
#[inline]
pub fn allows_tool(principal: &Principal, tool: &str) -> bool {
    tools::scope_of(tool).is_some_and(|s| principal.scopes.contains(s))
}
```

- [ ] **Step 8: Register the module.** Add `pub mod authz;` to `src/lib.rs` beside the other `pub mod` lines.

- [ ] **Step 9: Run the gating test and watch it pass.**

```text
cargo test --test scope_gating
```

- [ ] **Step 10: Add the error variant.** In `src/errors.rs`, add to `MCSError`:

```rust
    #[error("Insufficient scope: {tool} needs {scope}")]
    InsufficientScope { tool: String, scope: &'static str },
```

and to `error_code`:

```rust
            MCSError::InsufficientScope { .. } => -32002,
```

- [ ] **Step 11: Thread the principal through dispatch.**

In `src/server.rs`, change these four signatures to take `principal: &Principal` as the last parameter: `process_request`, `handle_tools_call`, `process_value_http`, and `dispatch_http_body`. Change `handle_tools_list` to take `principal: &Principal` beside `vectors_enabled`.

In `handle_tools_list`, add the principal filter to the existing category filter at line 693:

```rust
    let mut all: Vec<Value> = base_tools()
        .iter()
        .filter(|t| {
            t.get("name").and_then(Value::as_str).is_some_and(|n| {
                let category_on = if tools::is_write_tool(n) { write } else { read };
                category_on && crate::authz::allows_tool(principal, n)
            })
        })
        .cloned()
        .collect();
```

Apply the same `allows_tool` filter to the vector list at line 722 and to the code list at line 726.

At the top of `handle_tools_call`, after `tool_name` is read at line 756:

```rust
    if !crate::authz::allows_tool(principal, tool_name) {
        let scope = tools::scope_of(tool_name).unwrap_or("graph-read");
        return Err(MCSError::InsufficientScope {
            tool: tool_name.to_owned(),
            scope,
        });
    }
```

- [ ] **Step 12: Return a distinct HTTP outcome.**

Replace the return type of `dispatch_http_body` with this enum, declared beside it in `src/server.rs`:

```rust
/// What the HTTP transport should send back. A scope failure is not a JSON-RPC
/// error body: it must become HTTP 403 with a `WWW-Authenticate` header.
pub enum HttpOutcome {
    /// Only notifications; send 202 with no content.
    Accepted,
    /// Send this JSON body with 200.
    Body(Value),
    /// Send 403 and name every scope the request needed.
    InsufficientScope(Vec<&'static str>),
}
```

`dispatch_http_body` returns `std::result::Result<HttpOutcome, String>`. A batch that holds one denied call returns `InsufficientScope` for the whole batch, with the scopes of every denied call, sorted and deduplicated. Record that rule in the function's doc comment.

- [ ] **Step 13: Adapt the HTTP handlers.**

In `src/http.rs`, `post_handler` builds the principal before dispatch:

```rust
    let principal = crate::authz::bearer_principal(&state.bearer_scopes);
```

Add `bearer_scopes: Arc<[ToolCategory]>` to `HttpState` at line 61, and set it in `run` at line 103 from a new parameter. For this task pass `ToolCategory::ALL`, which keeps today's behaviour exactly. Task 2 makes it configurable.

Match the outcome:

```rust
    match outcome {
        Ok(HttpOutcome::Accepted) => StatusCode::ACCEPTED.into_response(),
        Ok(HttpOutcome::Body(value)) => { /* unchanged JSON or SSE branch */ }
        Ok(HttpOutcome::InsufficientScope(scopes)) => {
            let scope = scopes.join(" ");
            (
                StatusCode::FORBIDDEN,
                [(
                    header::WWW_AUTHENTICATE,
                    format!("Bearer error=\"insufficient_scope\", scope=\"{scope}\""),
                )],
                "insufficient scope",
            )
                .into_response()
        }
        Err(e) => { /* unchanged parse-error branch */ }
    }
```

In `src/server.rs`, the stdio and TCP paths pass `&crate::authz::local_principal()`.

- [ ] **Step 14: Add the end-to-end gating test.** Append to `tests/scope_gating.rs`:

```rust
use mcpmem::server::{HttpOutcome, dispatch_http_body};

#[test]
fn dispatch_denies_a_write_tool_for_a_read_only_principal() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"delete_entities","arguments":{"entityNames":["a"]}}}"#;
    match dispatch_http_body(body, &kg, None, &p).unwrap() {
        HttpOutcome::InsufficientScope(scopes) => assert_eq!(scopes, vec!["graph-write"]),
        _ => panic!("expected a scope refusal"),
    }
}

#[test]
fn tools_list_hides_what_the_principal_may_not_call() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let HttpOutcome::Body(v) = dispatch_http_body(body, &kg, None, &p).unwrap() else {
        panic!("expected a body");
    };
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"read_graph"));
    assert!(!names.contains(&"delete_entities"));
}
```

Add this helper at the top of `tests/scope_gating.rs`. It is the constructor
that `tests/mutation_service.rs:14` uses.

```rust
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;
use std::num::NonZeroUsize;

fn test_graph(dir: &tempfile::TempDir) -> GraphHandle {
    GraphHandle::new(
        &dir.path().join("memory.db"),
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap()
}
```

Both categories must be enabled for these two tests. The category flags are
process-wide atomics at `src/server.rs:123-126`. Set them once at the top of the
file through the public entry point that `src/main.rs` uses, or expose a
`#[doc(hidden)]` setter beside them and call it from the test.

- [ ] **Step 15: Run the whole affected set.**

```text
cargo test --test scope_gating -- --test-threads=1
cargo test --test e2e -- --test-threads=1
cargo test --test ui_http -- --test-threads=1
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Both category flags must be enabled in the e2e run for the list assertions to hold. Read `tests/e2e.rs` for how it starts the server.

- [ ] **Step 16: Commit.**

```bash
git add src/authz.rs src/tools.rs src/errors.rs src/server.rs src/http.rs src/lib.rs tests/scope_gating.rs
git commit -m "feat: gate every tool call on the caller's scopes"
```

---

### Task 2: Configuration surface and startup refusals

**Files:**
- Create: `src/principals.rs`
- Create: `tests/oauth_config.rs`
- Modify: `src/lib.rs:113-277` (new flags)
- Modify: `src/config.rs:10-60, 90-188` (new fields and refusals)

**Interfaces:**
- Produces `mcpmem::config::OAuthConfig`:

```rust
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub public_url: String,
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    pub oidc_client_secret: Option<Arc<str>>,
    pub principals: Vec<crate::principals::PrincipalEntry>,
    pub cimd_allowed_domains: Vec<String>,
    pub trust_forwarded_proto: bool,
}
```

- Produces `Config::oauth: Option<OAuthConfig>` and `Config::bearer_scopes: Vec<ToolCategory>`.
- Produces `mcpmem::principals::PrincipalEntry`:

```rust
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PrincipalEntry {
    pub name: String,
    pub iss: String,
    pub sub: String,
    #[serde(default)]
    pub label: Option<String>,
    pub scopes: Vec<String>,
}
```

- Produces `mcpmem::principals::load(path: &str) -> Result<Vec<PrincipalEntry>>`.

- [ ] **Step 1: Write the failing principals test.**

Create `tests/oauth_config.rs`:

```rust
use std::io::Write;

fn write_tmp(name: &str, body: &str) -> String {
    let dir = std::env::temp_dir().join(format!("mcpmem-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn a_valid_principals_file_loads() {
    let path = write_tmp(
        "ok.json",
        r#"[{"name":"adam","iss":"https://idp.example","sub":"42",
             "label":"adam@example","scopes":["graph-read","graph-write"]}]"#,
    );
    let list = mcpmem::principals::load(&path).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].scopes, vec!["graph-read", "graph-write"]);
}

#[test]
fn an_empty_principals_file_is_refused() {
    let path = write_tmp("empty.json", "[]");
    let err = mcpmem::principals::load(&path).unwrap_err().to_string();
    assert!(err.contains("empty"), "message was: {err}");
}

#[test]
fn an_unknown_scope_is_refused() {
    let path = write_tmp(
        "bad-scope.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-admin"]}]"#,
    );
    let err = mcpmem::principals::load(&path).unwrap_err().to_string();
    assert!(err.contains("graph-admin"), "message was: {err}");
}

#[test]
fn a_duplicate_identity_is_refused() {
    let path = write_tmp(
        "dup.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["code"]},
            {"name":"b","iss":"https://i","sub":"1","scopes":["code"]}]"#,
    );
    let err = mcpmem::principals::load(&path).unwrap_err().to_string();
    assert!(err.contains("duplicate"), "message was: {err}");
}
```

- [ ] **Step 2: Run it and watch it fail.**

```text
cargo test --test oauth_config
```

Expected: a compile error, because `mcpmem::principals` does not exist.

- [ ] **Step 3: Create `src/principals.rs`.**

```rust
//! The allowed humans, read from a JSON file at startup.
//!
//! Identity is `iss` plus `sub`. An email address is display text only, because
//! most providers let a human change it.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::errors::{MCSError, Result};
use crate::tools::ToolCategory;

#[derive(Debug, Clone, Deserialize)]
pub struct PrincipalEntry {
    pub name: String,
    pub iss: String,
    pub sub: String,
    #[serde(default)]
    pub label: Option<String>,
    pub scopes: Vec<String>,
}

impl PrincipalEntry {
    /// The identity key used to match an upstream identity token.
    pub fn key(&self) -> (&str, &str) {
        (self.iss.as_str(), self.sub.as_str())
    }

    pub fn scope_set(&self) -> BTreeSet<String> {
        self.scopes.iter().cloned().collect()
    }
}

/// Read and validate the principals file. Fails closed: an unreadable, empty or
/// invalid file stops the server.
pub fn load(path: &str) -> Result<Vec<PrincipalEntry>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        MCSError::InvalidParams(format!("failed to read --principals-file '{path}': {e}"))
    })?;
    let list: Vec<PrincipalEntry> = serde_json::from_str(&text).map_err(|e| {
        MCSError::InvalidParams(format!("--principals-file '{path}' is not valid JSON: {e}"))
    })?;
    if list.is_empty() {
        return Err(MCSError::InvalidParams(format!(
            "--principals-file '{path}' is empty; refusing to start with nobody allowed"
        )));
    }
    let mut seen = BTreeSet::new();
    for entry in &list {
        if entry.name.trim().is_empty() || entry.iss.trim().is_empty() || entry.sub.trim().is_empty()
        {
            return Err(MCSError::InvalidParams(
                "every principal needs a non-empty name, iss and sub".into(),
            ));
        }
        if entry.scopes.is_empty() {
            return Err(MCSError::InvalidParams(format!(
                "principal '{}' has no scopes",
                entry.name
            )));
        }
        for scope in &entry.scopes {
            if scope.parse::<ToolCategory>().is_err() {
                return Err(MCSError::InvalidParams(format!(
                    "principal '{}' names unknown scope '{scope}'",
                    entry.name
                )));
            }
        }
        if !seen.insert((entry.iss.clone(), entry.sub.clone())) {
            return Err(MCSError::InvalidParams(format!(
                "duplicate principal identity {} {}",
                entry.iss, entry.sub
            )));
        }
    }
    Ok(list)
}
```

Add `pub mod principals;` to `src/lib.rs`.

- [ ] **Step 4: Run the test and watch it pass.**

```text
cargo test --test oauth_config
```

- [ ] **Step 5: Add the flags.** In `src/lib.rs`, inside `Args`, after `--auth-token-file` at line 148:

```rust
    /// Canonical HTTPS URL of this server, for example `https://mem.example.com`.
    /// Required with `--oidc-issuer`. Never derived from the Host header.
    #[arg(long = "public-url")]
    pub public_url: Option<String>,

    /// Upstream OpenID Connect issuer. Turns the OAuth authorization server on.
    #[arg(long = "oidc-issuer")]
    pub oidc_issuer: Option<String>,

    /// Client identifier registered at the upstream provider.
    #[arg(long = "oidc-client-id")]
    pub oidc_client_id: Option<String>,

    /// File holding the upstream client secret. Omit for a public client.
    #[arg(long = "oidc-client-secret-file")]
    pub oidc_client_secret_file: Option<String>,

    /// JSON file listing the humans allowed to authorize, and their scopes.
    #[arg(long = "principals-file")]
    pub principals_file: Option<String>,

    /// Domain allowed to host a client metadata document. Repeatable.
    #[arg(long = "cimd-allowed-domain", value_name = "DOMAIN")]
    pub cimd_allowed_domains: Vec<String>,

    /// Trust X-Forwarded-Proto from a reverse proxy that terminates TLS.
    #[arg(long = "oauth-trust-forwarded-proto")]
    pub oauth_trust_forwarded_proto: bool,

    /// Scopes granted to the static bearer token. Defaults to every category.
    #[arg(long = "static-bearer-scopes", value_delimiter = ',', value_name = "SCOPE")]
    pub static_bearer_scopes: Vec<String>,
```

- [ ] **Step 6: Write the failing refusal tests.** Append to `tests/oauth_config.rs`:

```rust
use clap::Parser;
use mcpmem::{Args, config::Config};

fn args(extra: &[&str]) -> Args {
    let mut argv = vec!["mcpmem"];
    argv.extend_from_slice(extra);
    Args::parse_from(argv)
}

#[test]
fn oauth_without_tls_is_refused() {
    let p = write_tmp(
        "p1.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let err = Config::from_args(&args(&[
        "--transport", "http",
        "--oidc-issuer", "https://idp.example",
        "--oidc-client-id", "abc",
        "--public-url", "https://mem.example.com",
        "--principals-file", &p,
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("TLS"), "message was: {err}");
}

#[test]
fn oauth_without_public_url_is_refused() {
    let p = write_tmp(
        "p2.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let err = Config::from_args(&args(&[
        "--transport", "http",
        "--oauth-trust-forwarded-proto",
        "--oidc-issuer", "https://idp.example",
        "--oidc-client-id", "abc",
        "--principals-file", &p,
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("--public-url"), "message was: {err}");
}

// `webhooks` is not a default feature, and `RoleSet::parse_csv` rejects a role
// that is not compiled. Without this gate the test would fail on the role name
// rather than on the refusal it exists to prove.
#[cfg(feature = "webhooks")]
#[test]
fn oauth_without_the_mcp_role_is_refused() {
    let p = write_tmp(
        "p3.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let err = Config::from_args(&args(&[
        "--transport", "http",
        "--role", "webhooks",
        "--oauth-trust-forwarded-proto",
        "--oidc-issuer", "https://idp.example",
        "--oidc-client-id", "abc",
        "--public-url", "https://mem.example.com",
        "--principals-file", &p,
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("mcp role"), "message was: {err}");
}

#[test]
fn a_public_url_with_a_trailing_slash_is_normalized() {
    let p = write_tmp(
        "p4.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let cfg = Config::from_args(&args(&[
        "--transport", "http",
        "--oauth-trust-forwarded-proto",
        "--oidc-issuer", "https://idp.example",
        "--oidc-client-id", "abc",
        "--public-url", "https://mem.example.com/",
        "--principals-file", &p,
    ]))
    .unwrap();
    assert_eq!(cfg.oauth.unwrap().public_url, "https://mem.example.com");
}
```

The role strings are `mcp`, `indexer` and `webhooks`, at
`crates/mcpmem-runtime/src/lib.rs:23-25`. `RoleSet::parse_csv` rejects a role
whose feature is not compiled, at `crates/mcpmem-runtime/src/lib.rs:78`. Only
`code` is a default feature. Run the role test with
`cargo test --test oauth_config --features webhooks`.

- [ ] **Step 7: Run and watch them fail.**

```text
cargo test --test oauth_config --features webhooks
```

The feature is needed. Without it the role-refusal test is compiled out, and the
run passes while proving nothing. Use this command everywhere in this task.

- [ ] **Step 8: Extend `Config`.** Add to the struct at `src/config.rs:10`:

```rust
    /// OAuth authorization server settings. `None` keeps OAuth off.
    pub oauth: Option<OAuthConfig>,
    /// Scopes granted to the static bearer principal.
    pub bearer_scopes: Vec<ToolCategory>,
```

Add the `OAuthConfig` struct shown in the Interfaces block above `impl Config`.

In `from_args`, after the role parsing at line 156, add:

```rust
        let bearer_scopes = if args.static_bearer_scopes.is_empty() {
            ToolCategory::ALL.to_vec()
        } else {
            args.static_bearer_scopes
                .iter()
                .map(|s| s.parse::<ToolCategory>())
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(MCSError::InvalidParams)?
        };

        let oauth = if let Some(issuer) = args.oidc_issuer.clone() {
            if !roles.roles().contains(&crate::runtime::RuntimeRole::Mcp) {
                return Err(MCSError::InvalidParams(
                    "--oidc-issuer requires the mcp role".into(),
                ));
            }
            if tls_cert.is_none() && !args.oauth_trust_forwarded_proto {
                return Err(MCSError::InvalidParams(
                    "--oidc-issuer needs TLS; pass --tls-cert and --tls-key, or \
                     --oauth-trust-forwarded-proto behind a proxy that terminates TLS"
                        .into(),
                ));
            }
            let public_url = args.public_url.clone().ok_or_else(|| {
                MCSError::InvalidParams("--oidc-issuer requires --public-url".into())
            })?;
            let public_url = public_url.trim_end_matches('/').to_owned();
            if !public_url.starts_with("https://") {
                return Err(MCSError::InvalidParams(
                    "--public-url must use the https scheme".into(),
                ));
            }
            let client_id = args.oidc_client_id.clone().ok_or_else(|| {
                MCSError::InvalidParams("--oidc-issuer requires --oidc-client-id".into())
            })?;
            let principals_path = args.principals_file.clone().ok_or_else(|| {
                MCSError::InvalidParams("--oidc-issuer requires --principals-file".into())
            })?;
            let principals = crate::principals::load(&principals_path)?;
            let oidc_client_secret = match args.oidc_client_secret_file.clone() {
                None => None,
                Some(path) => {
                    let text = std::fs::read_to_string(&path).map_err(|e| {
                        MCSError::InvalidParams(format!(
                            "failed to read --oidc-client-secret-file '{path}': {e}"
                        ))
                    })?;
                    let secret = text.trim();
                    if secret.is_empty() {
                        return Err(MCSError::InvalidParams(format!(
                            "--oidc-client-secret-file '{path}' is empty"
                        )));
                    }
                    Some(Arc::from(secret))
                }
            };
            let cimd_allowed_domains = if args.cimd_allowed_domains.is_empty() {
                vec!["claude.ai".to_string(), "chatgpt.com".to_string()]
            } else {
                args.cimd_allowed_domains.clone()
            };
            Some(OAuthConfig {
                public_url,
                oidc_issuer: issuer.trim_end_matches('/').to_owned(),
                oidc_client_id: client_id,
                oidc_client_secret,
                principals,
                cimd_allowed_domains,
                trust_forwarded_proto: args.oauth_trust_forwarded_proto,
            })
        } else {
            None
        };
```

Add `oauth` and `bearer_scopes` to the returned `Config`, and to `Default for Config` at line 190 as `None` and `ToolCategory::ALL.to_vec()`.

- [ ] **Step 9: Run and watch them pass.**

```text
cargo test --test oauth_config --features webhooks
cargo test -p mcpmem --lib config
```

There is no `tests/config.rs`. The `Config` unit tests live in the library, at
`src/config.rs:218`.

- [ ] **Step 10: Use `bearer_scopes` in the HTTP transport.** The `http::run` call site is `MCPServer::run_http` at `src/server.rs:485`, not `src/main.rs`. Task 1 already added the parameter, so pass `config.bearer_scopes` there and convert it with `Arc::from`.

- [ ] **Step 11: Check and commit.**

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

```bash
git add src/lib.rs src/config.rs src/principals.rs src/main.rs src/http.rs tests/oauth_config.rs
git commit -m "feat: add the OAuth configuration surface and its startup refusals"
```

---

### Task 3: The OAuth store

**Files:**
- Create: `crates/mcpmem-oauth/Cargo.toml`
- Create: `crates/mcpmem-oauth/src/lib.rs`
- Create: `crates/mcpmem-oauth/src/store.rs`
- Create: `crates/mcpmem-core/migrations/0004_oauth.sql`
- Create: `tests/oauth_store.rs`
- Modify: `Cargo.toml:2` (workspace members) and the dependency table
- Modify: `crates/mcpmem-core/src/events.rs:45-57` (migration registry)

**Interfaces:**
- Produces `mcpmem_oauth::store::Store`, opened from a `rusqlite::Connection`.
- Produces these methods:

```rust
/// Everything an issued credential carries, apart from the credential itself.
/// The credential never enters the database: only its digest does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub client_id: String,
    pub principal: String,
    pub scopes: Vec<String>,
    pub resource: String,
    pub family: String,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Access = 0,
    Refresh = 1,
}

/// A code grant adds the two values that bind the code to one client exchange.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeGrant {
    pub grant: Grant,
    pub redirect_uri: String,
    pub code_challenge: String,
}

impl Store {
    pub fn new(conn: rusqlite::Connection) -> Store;

    pub fn put_client(&self, c: &ClientRecord) -> Result<()>;
    pub fn get_client(&self, client_id: &str) -> Result<Option<ClientRecord>>;
    pub fn touch_client(&self, client_id: &str, now_us: i64) -> Result<()>;

    pub fn put_login(&self, l: &LoginRecord) -> Result<()>;
    pub fn take_login(&self, state: &str, now_us: i64) -> Result<Option<LoginRecord>>;
    pub fn set_login_principal(&self, state: &str, principal: &str) -> Result<()>;

    pub fn put_code(&self, code: &str, g: &CodeGrant,
                    created_us: i64, expires_us: i64) -> Result<()>;
    pub fn take_code(&self, code: &str, now_us: i64) -> Result<Option<CodeGrant>>;

    pub fn put_token(&self, token: &str, kind: TokenKind, g: &Grant,
                     created_us: i64, expires_us: i64) -> Result<()>;
    pub fn find_access(&self, token: &str, now_us: i64) -> Result<Option<Grant>>;
    pub fn take_refresh(&self, token: &str, now_us: i64) -> Result<RefreshOutcome>;
    pub fn revoke_family(&self, family: &str) -> Result<()>;
    pub fn family_of(&self, token: &str) -> Result<Option<String>>;

    pub fn sweep(&self, now_us: i64) -> Result<u64>;
}
```

- Produces `RefreshOutcome`:

```rust
pub enum RefreshOutcome {
    /// The token was live. Here is its grant, and the token is now spent.
    Valid(Grant),
    /// The token was already spent. The store has revoked the family.
    Replayed,
    /// Unknown, expired or revoked.
    Unknown,
}
```

- Produces `mcpmem_oauth::digest(token: &str) -> String`, the lowercase SHA-256 hex digest.
- Produces `mcpmem_oauth::new_token() -> String`, 32 random bytes in base64url without padding.

- [ ] **Step 1: Write the migration.** Create `crates/mcpmem-core/migrations/0004_oauth.sql`:

```sql
CREATE TABLE IF NOT EXISTS oauth_client(
  client_id      TEXT PRIMARY KEY,
  client_name    TEXT NOT NULL,
  redirect_uris  TEXT NOT NULL,
  source         TEXT NOT NULL,
  created_us     INTEGER NOT NULL,
  last_used_us   INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS oauth_login(
  state            TEXT PRIMARY KEY,
  client_id        TEXT NOT NULL,
  redirect_uri     TEXT NOT NULL,
  client_state     TEXT,
  code_challenge   TEXT NOT NULL,
  resource         TEXT NOT NULL,
  scopes           TEXT NOT NULL,
  upstream_verifier TEXT NOT NULL,
  nonce            TEXT NOT NULL,
  csrf             TEXT NOT NULL,
  principal        TEXT,
  created_us       INTEGER NOT NULL,
  expires_us       INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS oauth_code(
  code_digest    TEXT PRIMARY KEY,
  client_id      TEXT NOT NULL,
  redirect_uri   TEXT NOT NULL,
  code_challenge TEXT NOT NULL,
  resource       TEXT NOT NULL,
  scopes         TEXT NOT NULL,
  principal      TEXT NOT NULL,
  created_us     INTEGER NOT NULL,
  expires_us     INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS oauth_token(
  token_digest TEXT PRIMARY KEY,
  kind         INTEGER NOT NULL,
  family       TEXT NOT NULL,
  client_id    TEXT NOT NULL,
  principal    TEXT NOT NULL,
  scopes       TEXT NOT NULL,
  resource     TEXT NOT NULL,
  spent        INTEGER NOT NULL DEFAULT 0,
  revoked      INTEGER NOT NULL DEFAULT 0,
  created_us   INTEGER NOT NULL,
  expires_us   INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS oauth_token_family ON oauth_token(family);
CREATE INDEX IF NOT EXISTS oauth_token_expiry ON oauth_token(expires_us);
CREATE INDEX IF NOT EXISTS oauth_code_expiry ON oauth_code(expires_us);
CREATE INDEX IF NOT EXISTS oauth_login_expiry ON oauth_login(expires_us);
```

`kind` is `0` for an access token and `1` for a refresh token. `scopes` and `redirect_uris` hold a JSON array. `source` is `dcr` or `cimd`.

- [ ] **Step 2: Register the migration.** In `crates/mcpmem-core/src/events.rs:45`, change the array length to 4 and add the entry:

```rust
pub const MIGRATIONS: [(i64, &str); 4] = [
    (1, include_str!("../migrations/0001_change_events.sql")),
    (2, include_str!("../migrations/0002_webhook_subscriptions.sql")),
    (3, include_str!("../migrations/0003_observation_metadata.sql")),
    (4, include_str!("../migrations/0004_oauth.sql")),
];
```

Do not edit migrations 1, 2 or 3. Their checksums are verified at every startup.

- [ ] **Step 3: Prove the migration applies to an existing database.** Add to `tests/oauth_store.rs`:

```rust
#[test]
fn the_oauth_migration_applies_to_a_database_that_predates_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.mcpmem");
    // First open applies 1..=3 as it did before this change.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        mcpmem_core::schema::initialize_database(&conn).unwrap();
    }
    // Second open must add only migration 4 and must not change earlier rows.
    let conn = rusqlite::Connection::open(&path).unwrap();
    mcpmem_core::schema::initialize_database(&conn).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migration", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 4);
    conn.query_row("SELECT COUNT(*) FROM oauth_token", [], |r| r.get::<_, i64>(0))
        .expect("oauth_token exists");
}
```

`mcpmem_core::schema::initialize_database` is the real initializer, at
`crates/mcpmem-core/src/schema.rs:15`.

- [ ] **Step 4: Run it and watch it fail.**

```text
cargo test --test oauth_store
```

Expected: the table does not exist, or the count is 3.

- [ ] **Step 5: Create the crate.** `crates/mcpmem-oauth/Cargo.toml`:

```toml
[package]
name = "mcpmem-oauth"
version = "1.0.0-rc.4"
edition = "2024"
license = "Apache-2.0"
description = "OAuth 2.1 authorization server for the mcpmem MCP server."
repository = "https://github.com/abankowski/mcpmem"

[dependencies]
mcpmem-core = { path = "../mcpmem-core", version = "1.0.0-rc.4" }
rusqlite = { version = "0.40", features = ["bundled"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
base64 = "0.22"
getrandom = "0.2"
subtle = "2.6"
thiserror = "2"
tracing = "0.1"

[lints]
workspace = true
```

This repository has no `[workspace.dependencies]` table. Each crate states its
own versions. The manifest above copies the spelling of
`crates/mcpmem-webhook/Cargo.toml`. Match the `version` field to the version in
the root `Cargo.toml`, and keep the `[lints] workspace = true` block, which every
crate here carries.

Add `"crates/mcpmem-oauth"` to the workspace members at `Cargo.toml:2`.

- [ ] **Step 6: Write the store, and its unit tests first.** Append to `tests/oauth_store.rs`:

```rust
use mcpmem_oauth::store::{CodeGrant, Grant, RefreshOutcome, Store, TokenKind};
use mcpmem_oauth::{digest, new_token};

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("s.mcpmem")).unwrap();
    mcpmem_core::schema::initialize_database(&conn).unwrap();
    (dir, Store::new(conn))
}

fn grant(family: &str, scopes: &[&str]) -> Grant {
    Grant {
        client_id: "c1".into(),
        principal: "adam".into(),
        scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
        resource: "https://mem.example.com/mcp".into(),
        family: family.into(),
    }
}

#[test]
fn the_store_never_holds_the_token_value() {
    let (_d, s) = store();
    let token = new_token();
    s.put_token(&token, TokenKind::Access, &grant("fam", &["graph-read"]), 1, 1_000)
        .unwrap();
    let stored: String = s
        .connection()
        .query_row("SELECT token_digest FROM oauth_token", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, digest(&token));
    assert_ne!(stored, token);
}

#[test]
fn an_expired_access_token_is_not_found() {
    let (_d, s) = store();
    let token = new_token();
    s.put_token(&token, TokenKind::Access, &grant("fam", &["graph-read"]), 1, 10)
        .unwrap();
    assert!(s.find_access(&token, 5).unwrap().is_some());
    assert!(s.find_access(&token, 11).unwrap().is_none());
}

#[test]
fn find_access_returns_the_grant_it_was_given() {
    let (_d, s) = store();
    let token = new_token();
    let g = grant("fam", &["graph-read", "vectors"]);
    s.put_token(&token, TokenKind::Access, &g, 1, 10_000).unwrap();
    assert_eq!(s.find_access(&token, 2).unwrap().unwrap(), g);
}

#[test]
fn a_refresh_token_is_not_accepted_as_an_access_token() {
    let (_d, s) = store();
    let token = new_token();
    s.put_token(&token, TokenKind::Refresh, &grant("fam", &["graph-read"]), 1, 10_000)
        .unwrap();
    assert!(s.find_access(&token, 2).unwrap().is_none());
}

#[test]
fn a_replayed_refresh_token_kills_the_whole_family() {
    let (_d, s) = store();
    let refresh = new_token();
    let access = new_token();
    let g = grant("fam", &["graph-read"]);
    s.put_token(&refresh, TokenKind::Refresh, &g, 1, 10_000).unwrap();
    s.put_token(&access, TokenKind::Access, &g, 1, 10_000).unwrap();

    assert!(matches!(s.take_refresh(&refresh, 2).unwrap(), RefreshOutcome::Valid(_)));
    assert!(matches!(s.take_refresh(&refresh, 3).unwrap(), RefreshOutcome::Replayed));
    assert!(
        s.find_access(&access, 4).unwrap().is_none(),
        "the sibling access token must die with the family"
    );
}

#[test]
fn one_family_does_not_revoke_another() {
    let (_d, s) = store();
    let r1 = new_token();
    let a2 = new_token();
    s.put_token(&r1, TokenKind::Refresh, &grant("f1", &["code"]), 1, 10_000).unwrap();
    s.put_token(&a2, TokenKind::Access, &grant("f2", &["code"]), 1, 10_000).unwrap();
    s.take_refresh(&r1, 2).unwrap();
    s.take_refresh(&r1, 3).unwrap();
    assert!(s.find_access(&a2, 4).unwrap().is_some());
}

#[test]
fn the_sweep_deletes_only_expired_rows() {
    let (_d, s) = store();
    let live = new_token();
    let dead = new_token();
    s.put_token(&live, TokenKind::Access, &grant("f1", &["code"]), 1, 10_000).unwrap();
    s.put_token(&dead, TokenKind::Access, &grant("f2", &["code"]), 1, 5).unwrap();
    let removed = s.sweep(100).unwrap();
    assert_eq!(removed, 1);
    assert!(s.find_access(&live, 101).unwrap().is_some());
}

#[test]
fn an_authorization_code_is_single_use() {
    let (_d, s) = store();
    let code = new_token();
    let cg = CodeGrant {
        grant: grant("fam", &["graph-read"]),
        redirect_uri: "https://claude.ai/api/mcp/auth_callback".into(),
        code_challenge: "the-challenge".into(),
    };
    s.put_code(&code, &cg, 1, 10_000).unwrap();
    assert_eq!(s.take_code(&code, 2).unwrap().unwrap(), cg);
    assert!(s.take_code(&code, 3).unwrap().is_none());
}

#[test]
fn an_expired_authorization_code_is_not_returned() {
    let (_d, s) = store();
    let code = new_token();
    let cg = CodeGrant {
        grant: grant("fam", &["graph-read"]),
        redirect_uri: "https://claude.ai/api/mcp/auth_callback".into(),
        code_challenge: "the-challenge".into(),
    };
    s.put_code(&code, &cg, 1, 10).unwrap();
    assert!(s.take_code(&code, 11).unwrap().is_none());
}
```

- [ ] **Step 7: Run and watch them fail.**

```text
cargo test --test oauth_store
```

- [ ] **Step 8: Implement `crates/mcpmem-oauth/src/lib.rs`.**

```rust
//! The `mcpmem` OAuth 2.1 authorization server.
//!
//! `mcpmem` issues its own tokens. An upstream OpenID Connect provider only
//! authenticates the human. Tokens are opaque; the database holds their digest.

pub mod store;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of a token value. The store holds this, never the value.
pub fn digest(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    format!("{:x}", h.finalize())
}

/// 32 random bytes, base64url without padding. Panics only when the operating
/// system random source fails, which is not a recoverable condition.
pub fn new_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("operating system random source");
    URL_SAFE_NO_PAD.encode(buf)
}

/// The S256 PKCE challenge for a verifier, base64url without padding.
pub fn s256_challenge(verifier: &str) -> String {
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(h.finalize())
}

/// Constant-time comparison for a digest.
pub fn digest_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}
```

- [ ] **Step 9: Implement `crates/mcpmem-oauth/src/store.rs`.**

Write the record types and the methods named in the Interfaces block. Rules the implementation must follow:

1. Every read filters on `expires_us > now_us`, `revoked = 0` and, for a refresh token, `spent = 0`.
2. `take_code` deletes the row in the same statement that reads it, with `DELETE ... RETURNING`, so a replay finds nothing.
3. `take_refresh` runs inside one immediate transaction. It reads the row by digest. When the row exists and `spent = 1`, it sets `revoked = 1` for every row with the same `family` and returns `Replayed`. When the row is live, it sets `spent = 1` and returns `Valid`.
4. `sweep` deletes expired rows from all four tables and returns the total count.
5. `Store::connection()` exposes the connection for tests only. Mark it `#[doc(hidden)]`.

- [ ] **Step 10: Run the whole store suite and watch it pass.**

```text
cargo test --test oauth_store -- --test-threads=1
```

- [ ] **Step 11: Check and commit.**

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

```bash
git add Cargo.toml Cargo.lock crates/mcpmem-oauth crates/mcpmem-core/migrations/0004_oauth.sql crates/mcpmem-core/src/events.rs tests/oauth_store.rs
git commit -m "feat: add OAuth storage with digest-only tokens"
```

---

### Task 4: Discovery documents and the 401 challenge

> **Superseded code, 2026-09-10.** Task 4 is complete. Five review rounds
> reshaped the test fixture and the routes. Every code block in this task is the
> state before those rounds: it still names `support::oauth_router`,
> `oauth_router_with`, a positional `HttpState::for_test`, and one
> protected-resource route. Do not copy from this task. Read
> `tests/support/mod.rs` and `src/oauth_routes.rs`, and read the frozen-interface
> rules in Task 5. The steps below stay as the record of what was asked.

**Files:**
- Create: `crates/mcpmem-oauth/src/metadata.rs`
- Create: `src/oauth_routes.rs`
- Create: `tests/oauth_discovery.rs`
- Modify: `src/http.rs:60-87, 94-125, 161-221`
- Modify: `crates/mcpmem-oauth/src/lib.rs` (add `pub mod metadata;`)

**Interfaces:**
- Consumes `mcpmem::config::OAuthConfig` from Task 2.
- Produces `mcpmem_oauth::metadata::protected_resource(public_url: &str, scopes: &[&str]) -> serde_json::Value`.
- Produces `mcpmem_oauth::metadata::authorization_server(public_url: &str, scopes: &[&str]) -> serde_json::Value`.
- Produces `mcpmem::oauth_routes::attach(router: Router, state: HttpState) -> Router`.
- Produces `HttpState.oauth: Option<Arc<OAuthState>>`, where `OAuthState` holds the config, the store and the upstream client.

- [ ] **Step 1: Write the failing discovery test.** Create `tests/oauth_discovery.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

mod support;

#[tokio::test]
async fn an_unauthenticated_mcp_post_names_the_resource_metadata() {
    let app = support::oauth_router().await;
    let res = app
        .oneshot(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let header = res
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(header.starts_with("Bearer "), "header was: {header}");
    assert!(
        header.contains(
            r#"resource_metadata="https://mem.example.com/.well-known/oauth-protected-resource""#
        ),
        "header was: {header}"
    );
    assert!(header.contains(r#"scope=""#), "header was: {header}");
}

#[tokio::test]
async fn the_protected_resource_document_names_this_server() {
    let app = support::oauth_router().await;
    let res = app
        .oneshot(
            Request::get("/.well-known/oauth-protected-resource")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(body["resource"], "https://mem.example.com/mcp");
    assert_eq!(body["authorization_servers"][0], "https://mem.example.com");
    assert!(body["scopes_supported"].as_array().unwrap().contains(&"graph-read".into()));
}

#[tokio::test]
async fn the_authorization_server_document_advertises_pkce_and_cimd() {
    let app = support::oauth_router().await;
    let res = app
        .oneshot(
            Request::get("/.well-known/oauth-authorization-server")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(body["issuer"], "https://mem.example.com");
    assert_eq!(body["code_challenge_methods_supported"][0], "S256");
    assert_eq!(body["client_id_metadata_document_supported"], true);
    assert_eq!(body["authorization_response_iss_parameter_supported"], true);
    assert_eq!(body["token_endpoint_auth_methods_supported"][0], "none");
    assert_eq!(body["registration_endpoint"], "https://mem.example.com/oauth/register");
}
```

Create `tests/support/mod.rs`. It holds the two helpers below.

```rust
// tests/support/mod.rs
use std::sync::Arc;

use axum::Router;
use axum::http::Response;
use http_body_util::BodyExt;
use mcpmem::config::OAuthConfig;
use mcpmem::principals::PrincipalEntry;
use mcpmem::tools::ToolCategory;

pub const PUBLIC_URL: &str = "https://mem.example.com";

/// One allowed human, holding graph-read and graph-write.
pub fn principals(iss: &str) -> Vec<PrincipalEntry> {
    vec![PrincipalEntry {
        name: "adam".into(),
        iss: iss.into(),
        sub: "sub-1".into(),
        label: Some("adam@example.com".into()),
        scopes: vec!["graph-read".into(), "graph-write".into()],
    }]
}

pub fn oauth_config(upstream_issuer: &str) -> OAuthConfig {
    OAuthConfig {
        public_url: PUBLIC_URL.into(),
        oidc_issuer: upstream_issuer.into(),
        oidc_client_id: "mcpmem-test".into(),
        oidc_client_secret: None,
        principals: principals(upstream_issuer),
        cimd_allowed_domains: vec!["claude.ai".into(), "chatgpt.com".into()],
        trust_forwarded_proto: true,
    }
}

/// A router with OAuth on, backed by a fresh temporary database. The directory
/// is returned so the caller keeps it alive for the length of the test.
pub async fn oauth_router_with(upstream_issuer: &str) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = mcpmem::http::HttpState::for_test(
        dir.path().join("t.mcpmem"),
        Some(oauth_config(upstream_issuer)),
        ToolCategory::ALL.to_vec(),
    );
    (dir, mcpmem::http::router(state))
}

/// A router with OAuth on and an upstream that is never reached.
pub async fn oauth_router() -> Router {
    let (dir, router) = oauth_router_with("https://idp.invalid").await;
    // The discovery tests never touch the database after this point.
    std::mem::forget(dir);
    router
}

pub async fn json(res: Response<axum::body::Body>) -> serde_json::Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

pub fn header(res: &Response<axum::body::Body>, name: &str) -> String {
    res.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
        .to_owned()
}
```

`HttpState` fields are private today. Add a `#[doc(hidden)]` constructor
`HttpState::for_test(db_path, oauth, categories)` in `src/http.rs`, beside the
existing struct at line 61. It opens the graph, applies the schema, and builds
the OAuth state. Integration tests cannot build the struct without it.

`std::mem::forget` on the temporary directory is deliberate. The discovery tests
never read the database again, and the operating system removes the directory.
Every other test uses `oauth_router_with` and keeps the handle.

- [ ] **Step 2: Add the development dependencies.** In the root `Cargo.toml`, under `[dev-dependencies]`:

```toml
tower = { version = "0.5", features = ["util"] }
http-body-util = "0.1"
```

- [ ] **Step 3: Run and watch it fail.**

```text
cargo test --test oauth_discovery
```

- [ ] **Step 4: Implement `metadata.rs`.**

```rust
use serde_json::{Value, json};

/// RFC 9728 protected resource metadata. `public_url` carries no trailing slash.
pub fn protected_resource(public_url: &str, scopes: &[&str]) -> Value {
    json!({
        "resource": format!("{public_url}/mcp"),
        "authorization_servers": [public_url],
        "scopes_supported": scopes,
        "bearer_methods_supported": ["header"]
    })
}

/// RFC 8414 authorization server metadata.
pub fn authorization_server(public_url: &str, scopes: &[&str]) -> Value {
    json!({
        "issuer": public_url,
        "authorization_endpoint": format!("{public_url}/oauth/authorize"),
        "token_endpoint": format!("{public_url}/oauth/token"),
        "registration_endpoint": format!("{public_url}/oauth/register"),
        "revocation_endpoint": format!("{public_url}/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
        "scopes_supported": scopes
    })
}
```

- [ ] **Step 5: Create `src/oauth_routes.rs` with the two document handlers.**

```rust
//! HTTP adapters for the OAuth authorization server. Every handler here is thin:
//! it parses the request, calls `mcpmem_oauth`, and shapes the response.

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, http::StatusCode};

use crate::http::HttpState;

pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/{*resource_path}",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
}

fn scopes(state: &HttpState) -> Vec<&'static str> {
    state
        .enabled_categories
        .iter()
        .map(|c| c.slug())
        .collect()
}

async fn protected_resource(State(state): State<HttpState>) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(mcpmem_oauth::metadata::protected_resource(
        &oauth.config.public_url,
        &scopes(&state),
    ))
    .into_response()
}

async fn authorization_server(State(state): State<HttpState>) -> Response {
    let Some(oauth) = state.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(mcpmem_oauth::metadata::authorization_server(
        &oauth.config.public_url,
        &scopes(&state),
    ))
    .into_response()
}
```

Add `enabled_categories: Arc<[ToolCategory]>` and `oauth: Option<Arc<OauthState>>` to `HttpState`, and call `oauth_routes::attach` inside `router()` at `src/http.rs:69`. The `/.well-known` routes answer HTTP 404 when OAuth is off, so a server without OAuth advertises nothing.

Declare `OauthState` in `src/oauth_routes.rs` now, with every field that later
tasks need. Retrofitting it in Task 8 would touch code that is already reviewed.

```rust
/// Everything the OAuth routes share. The clock is a field, not a call to the
/// wall clock: expiry tests cannot sleep for an hour.
pub struct OauthState {
    pub config: crate::config::OAuthConfig,
    pub store: mcpmem_oauth::store::Store,
    pub now_us: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub limits: mcpmem_oauth::limits::RateLimiter,
}
```

`limits` arrives in Task 9. Until then, declare the field with a permissive
limiter, or leave the field out and add it in Task 9. Either is acceptable; say
which you chose in the report. `now_us` defaults to
`Arc::new(mcpmem_core::events::now_us)`.

`HttpState::for_test` takes `Vec<ToolCategory>` and converts with `Arc::from`.

- [ ] **Step 6: Send the challenge.** Replace the two `StatusCode::UNAUTHORIZED` responses in `post_handler` and `get_handler` with a shared helper in `src/http.rs`:

```rust
/// The RFC 6750 challenge. With OAuth on it names the resource metadata, so the
/// client can discover the authorization server. With OAuth off it is bare.
fn unauthorized(state: &HttpState) -> Response {
    let mut value = String::from("Bearer");
    if let Some(oauth) = state.oauth.as_ref() {
        let scope = state
            .enabled_categories
            .iter()
            .map(|c| c.slug())
            .collect::<Vec<_>>()
            .join(" ");
        value.push_str(&format!(
            " resource_metadata=\"{}/.well-known/oauth-protected-resource\", scope=\"{scope}\"",
            oauth.config.public_url
        ));
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, value)],
        "Unauthorized",
    )
        .into_response()
}
```

With OAuth on and no static token configured, a request without a token is unauthorized. Change `authorized` so it returns `false` when `auth_token` is `None` and `oauth` is `Some`. Keep the fully open case: both `None` stays open.

- [ ] **Step 7: Run and watch them pass.**

```text
cargo test --test oauth_discovery
cargo test --test ui_http -- --test-threads=1
```

The `ui_http` run proves the open and static-token paths did not change.

- [ ] **Step 8: Check and commit.**

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

```bash
git add crates/mcpmem-oauth/src/metadata.rs crates/mcpmem-oauth/src/lib.rs src/oauth_routes.rs src/http.rs src/lib.rs Cargo.toml Cargo.lock tests/oauth_discovery.rs tests/support
git commit -m "feat: serve OAuth discovery documents and the 401 challenge"
```

---

### Task 5: Client registration

**Files:**
- Create: `crates/mcpmem-oauth/src/registration.rs`
- Create: `tests/oauth_registration.rs`
- Modify: `src/oauth_routes.rs` (add `POST /oauth/register`)
- Modify: `crates/mcpmem-oauth/src/lib.rs`

**Interfaces:**
- Produces `mcpmem_oauth::registration::register(store, body: &Value, now_us) -> Result<Value, RegistrationError>`.
- Produces `mcpmem_oauth::registration::resolve_metadata_document(url: &str, allowed_domains: &[String], fetch: &dyn Fetch) -> Result<ClientRecord, RegistrationError>`.
- Produces the trait `Fetch`, so tests supply a document without a network:

```rust
pub trait Fetch: Send + Sync {
    fn get(&self, url: &str) -> Result<String, String>;
}
```

- [ ] **Step 1: Write the failing tests.** Create `tests/oauth_registration.rs` with these cases:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use mcpmem_oauth::registration::{Fetch, RegistrationError, resolve_metadata_document};
use tower::ServiceExt;

mod support;

struct StubFetch(String);
impl Fetch for StubFetch {
    fn get(&self, _url: &str) -> Result<String, String> {
        Ok(self.0.clone())
    }
}

struct PanicFetch;
impl Fetch for PanicFetch {
    fn get(&self, url: &str) -> Result<String, String> {
        panic!("must not fetch {url}");
    }
}

async fn register(body: &str) -> (StatusCode, serde_json::Value) {
    let server = support::oauth_server().await;
    let res = server
        .request(
            Request::post("/oauth/register")
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await;
    let status = res.status();
    (status, support::json(res).await)
}
```

Task 4 froze the fixture interface. Read `tests/support/mod.rs` before you write
a test, and use these rules.

- The constructors are `server`, `oauth_server`, `oauth_server_with`,
  `oauth_server_with_scopes`, `oauth_server_with_clock`, `oauth_server_at`,
  `oauth_server_without_categories` and `open_server`.
- The types are `Scopes`, `Clock` and `Server`.
- Send every request with `server.request(req).await`. The router is private, and
  no accessor hands out anything that outlives the `Server`.
- The accessors are `oauth`, `clock` and `dir`.
- Keep the `Server` alive for the whole test.
- Hold one `Server` at a time. The fixture serialises the process-wide
  tool-category flags. A second `Server` on the same thread panics at once. Split
  such a test into two tests.

```rust

#[tokio::test]
async fn dynamic_registration_returns_a_client_id() {
    let (status, body) = register(
        r#"{"client_name":"Claude",
            "redirect_uris":["https://claude.ai/api/mcp/auth_callback"],
            "grant_types":["authorization_code","refresh_token"],
            "token_endpoint_auth_method":"none",
            "application_type":"native"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!body["client_id"].as_str().unwrap().is_empty());
    assert_eq!(
        body["redirect_uris"][0],
        "https://claude.ai/api/mcp/auth_callback"
    );
    assert_eq!(body["token_endpoint_auth_method"], "none");
    assert!(body.get("client_secret").is_none(), "clients are public");
}

#[tokio::test]
async fn registration_without_a_redirect_uri_is_refused() {
    let (status, body) = register(r#"{"client_name":"Claude","redirect_uris":[]}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_client_metadata");
}

#[tokio::test]
async fn registration_with_a_plain_http_redirect_uri_is_refused() {
    let (status, body) = register(
        r#"{"client_name":"Evil","redirect_uris":["http://evil.example/cb"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_redirect_uri");
}

#[tokio::test]
async fn a_loopback_redirect_uri_is_accepted() {
    let (status, _) = register(
        r#"{"client_name":"Local","redirect_uris":["http://127.0.0.1:3000/callback"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[test]
fn a_metadata_document_on_an_allowed_domain_is_accepted() {
    let url = "https://claude.ai/oauth/client-metadata.json";
    let doc = format!(
        r#"{{"client_id":"{url}","client_name":"Claude",
             "redirect_uris":["https://claude.ai/api/mcp/auth_callback"]}}"#
    );
    let allowed = vec!["claude.ai".to_string()];
    let record = resolve_metadata_document(url, &allowed, &StubFetch(doc)).unwrap();
    assert_eq!(record.client_id, url);
    assert_eq!(record.source, "cimd");
    assert_eq!(record.redirect_uris, vec!["https://claude.ai/api/mcp/auth_callback"]);
}

#[test]
fn a_metadata_document_whose_client_id_differs_from_its_url_is_refused() {
    let url = "https://claude.ai/oauth/client-metadata.json";
    let doc = r#"{"client_id":"https://claude.ai/other.json","client_name":"Claude",
                  "redirect_uris":["https://claude.ai/cb"]}"#;
    let allowed = vec!["claude.ai".to_string()];
    let err = resolve_metadata_document(url, &allowed, &StubFetch(doc.into())).unwrap_err();
    assert!(matches!(err, RegistrationError::MetadataMismatch));
}

#[test]
fn a_metadata_document_on_a_domain_outside_the_list_is_never_fetched() {
    let allowed = vec!["claude.ai".to_string()];
    let err = resolve_metadata_document(
        "https://evil.example/client.json",
        &allowed,
        &PanicFetch,
    )
    .unwrap_err();
    assert!(matches!(err, RegistrationError::DomainNotAllowed));
}

#[test]
fn a_metadata_document_url_without_https_is_refused() {
    let allowed = vec!["claude.ai".to_string()];
    let err =
        resolve_metadata_document("http://claude.ai/c.json", &allowed, &PanicFetch).unwrap_err();
    assert!(matches!(err, RegistrationError::DomainNotAllowed));
}
```

- [ ] **Step 2: Run and watch them fail.**

```text
cargo test --test oauth_registration
```

- [ ] **Step 3: Implement `registration.rs`.** Rules:

1. A registration request needs at least one `redirect_uris` entry. Every entry must use `https`, or be a loopback `http` URL with host `127.0.0.1` or `localhost`.
2. The generated `client_id` is `new_token()`. No client secret is issued; every client is public.
3. The response carries `client_id`, `client_id_issued_at`, `redirect_uris`, `client_name`, `grant_types`, `response_types` and `token_endpoint_auth_method: "none"`.
4. `resolve_metadata_document` checks the host against the allowed list before it fetches. A host outside the list returns `DomainNotAllowed`. It makes no request.
5. The fetched document must be valid JSON, must carry `client_id`, `client_name` and `redirect_uris`, and its `client_id` must equal the requested URL exactly.
6. The stored `ClientRecord` keeps `source = "cimd"` and the document's own redirect list.

- [ ] **Step 4: Add the route.** In `src/oauth_routes.rs`, add `POST /oauth/register`. Use a real HTTP fetcher built on `reqwest` with these limits: `https` only, no redirects, a 64 KB body cap, and a 5 second timeout. Put the fetcher in `crates/mcpmem-oauth/src/upstream.rs` in Task 6 and re-use it; for this task, place it in `registration.rs` and move it later only if that keeps the code smaller.

- [ ] **Step 5: Run and watch them pass.**

```text
cargo test --test oauth_registration -- --test-threads=1
```

- [ ] **Step 6: Check and commit.**

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

```bash
git add crates/mcpmem-oauth/src/registration.rs crates/mcpmem-oauth/src/lib.rs src/oauth_routes.rs Cargo.toml Cargo.lock tests/oauth_registration.rs
git commit -m "feat: accept dynamic client registration and metadata documents"
```

---

### Task 6: The upstream OpenID Connect leg

**Files:**
- Create: `crates/mcpmem-oauth/src/upstream.rs`
- Create: `tests/support/fake_idp.rs`
- Create: `tests/oauth_upstream.rs`
- Modify: `src/oauth_routes.rs` (add `GET /oauth/authorize` and `GET /oauth/callback`)
- Modify: `crates/mcpmem-oauth/Cargo.toml` (add `reqwest`, `jsonwebtoken` and `url`, all optional, behind a cargo feature)
- Modify: `Cargo.toml` (enable that feature from the `mcpmem` package)

**Dependency guard, added 2026-09-10 after Task 5 found it.** CI fails the build
when a graph-only tree names an HTTP client. The check is at
`.github/workflows/ci.yml:59`, and it runs
`cargo tree --no-default-features -e normal`. The `mcpmem` package depends on
`mcpmem-oauth` with no feature gate today, so a plain `reqwest` dependency in the
new crate enters that tree and fails the build.

Put `reqwest`, `jsonwebtoken` and `url` behind a cargo feature in
`crates/mcpmem-oauth`. Gate the real fetcher and the `Provider` on it. Do not
widen the guard. Run the guard command yourself before you report:

```text
cargo tree --no-default-features -e normal
```

Its output must name no `reqwest`, no `aws-*` and no `aws_sdk_*` package. The
`Fetch` trait and `resolve_metadata_document` from Task 5 stay outside the
feature, because they hold no network code.

**Interfaces:**
- Produces `mcpmem_oauth::upstream::Provider`, built from an issuer URL:

```rust
impl Provider {
    pub async fn discover(issuer: &str) -> Result<Provider, UpstreamError>;
    pub fn authorize_url(&self, client_id: &str, redirect_uri: &str,
                         state: &str, nonce: &str, challenge: &str) -> String;
    pub async fn exchange(&self, code: &str, verifier: &str,
                          client_id: &str, client_secret: Option<&str>,
                          redirect_uri: &str, expected_nonce: &str)
                          -> Result<IdentityClaims, UpstreamError>;
    pub fn authorization_endpoint(&self) -> &str;
    pub fn token_endpoint(&self) -> &str;
    pub fn jwks_uri(&self) -> &str;
}
```

- Produces `IdentityClaims { iss: String, sub: String, email: Option<String>, nonce: Option<String> }`.

- [ ] **Step 1: Build the fake provider first.** Create `tests/support/fake_idp.rs`. It must:

1. Generate an RSA or EC keypair inside the test.
2. Serve `/.well-known/openid-configuration` naming its own `authorization_endpoint`, `token_endpoint` and `jwks_uri`.
3. Serve `/jwks` with the public key and a stable `kid`.
4. Serve `/authorize` by redirecting at once to the `redirect_uri` with a fixed code and the given `state`.
5. Serve `/token` by returning `{"access_token":"…","id_token":"<signed>"}`, where the identity token carries `iss`, `aud`, `sub`, `exp` and the `nonce` from the authorization request.
6. Bind to an ephemeral port and expose its base URL.

Follow the process-spawning pattern in `tests/ui_http.rs:31-37` for the port, but run this provider inside the test process with `axum::serve` on a `tokio` task.

- [ ] **Step 2: Write the failing upstream tests.** Create `tests/oauth_upstream.rs`:

```rust
use mcpmem_oauth::upstream::{Provider, UpstreamError};

mod support;
use support::fake_idp::{FakeIdp, IdpBehaviour};

const CLIENT_ID: &str = "mcpmem-test";
const REDIRECT: &str = "https://mem.example.com/oauth/callback";

#[tokio::test]
async fn discovery_reads_the_three_endpoints() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    assert_eq!(p.authorization_endpoint(), format!("{}/authorize", idp.issuer));
    assert_eq!(p.token_endpoint(), format!("{}/token", idp.issuer));
    assert_eq!(p.jwks_uri(), format!("{}/jwks", idp.issuer));
}

#[tokio::test]
async fn a_valid_identity_token_yields_its_claims() {
    let idp = FakeIdp::start(IdpBehaviour::default()).await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let claims = p
        .exchange("the-code", "the-verifier", CLIENT_ID, None, REDIRECT, "the-nonce")
        .await
        .unwrap();
    assert_eq!(claims.iss, idp.issuer);
    assert_eq!(claims.sub, "sub-1");
    assert_eq!(claims.nonce.as_deref(), Some("the-nonce"));
}

#[tokio::test]
async fn an_identity_token_signed_by_another_key_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour {
        sign_with_foreign_key: true,
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let err = p
        .exchange("c", "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Signature), "was: {err:?}");
}

#[tokio::test]
async fn an_identity_token_with_the_wrong_audience_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour {
        audience: Some("someone-else".into()),
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let err = p
        .exchange("c", "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Audience), "was: {err:?}");
}

#[tokio::test]
async fn an_identity_token_with_a_stale_nonce_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour {
        nonce: Some("an-old-nonce".into()),
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let err = p
        .exchange("c", "v", CLIENT_ID, None, REDIRECT, "the-new-nonce")
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Nonce), "was: {err:?}");
}

#[tokio::test]
async fn an_expired_identity_token_is_refused() {
    let idp = FakeIdp::start(IdpBehaviour {
        expires_in_seconds: -60,
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let err = p
        .exchange("c", "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap_err();
    assert!(matches!(err, UpstreamError::Expired), "was: {err:?}");
}

#[tokio::test]
async fn an_unknown_key_identifier_refetches_the_key_set_once() {
    let idp = FakeIdp::start(IdpBehaviour {
        rotate_kid_after_discovery: true,
        ..IdpBehaviour::default()
    })
    .await;
    let p = Provider::discover(&idp.issuer).await.unwrap();
    let claims = p
        .exchange("c", "v", CLIENT_ID, None, REDIRECT, "n")
        .await
        .unwrap();
    assert_eq!(claims.sub, "sub-1");
    assert_eq!(idp.jwks_requests(), 2, "one initial fetch, one refetch");
}
```

`IdpBehaviour` is the switch board of the fake provider. It carries
`sign_with_foreign_key`, `audience`, `nonce`, `expires_in_seconds` and
`rotate_kid_after_discovery`. Its `Default` produces a well-formed provider with
audience `mcpmem-test`, subject `sub-1`, and the nonce echoed from the request.
`FakeIdp::jwks_requests()` counts requests to `/jwks`.

`Provider::exchange` takes the expected nonce as its last parameter, so nonce
verification lives with the other claim checks rather than at the call site.

- [ ] **Step 3: Run and watch them fail.**

```text
cargo test --test oauth_upstream
```

- [ ] **Step 4: Implement `upstream.rs`.** Rules:

1. Discovery reads `{issuer}/.well-known/openid-configuration` once and caches the result for the process lifetime.
2. The identity token is verified with `jsonwebtoken` against the key whose `kid` matches. An unknown `kid` refetches the JWKS once, then fails.
3. Verify that `iss` equals the configured issuer. Verify that `aud` equals the configured client identifier. Verify that `exp` is in the future. Verify that `nonce` equals the value in the login row.
4. The token request sends `code`, `code_verifier`, `client_id`, `redirect_uri` and `grant_type=authorization_code`. It sends `client_secret` only when one is configured.

- [ ] **Step 5: Add the two routes.** In `src/oauth_routes.rs`:

`GET /oauth/authorize` reads `client_id`, `redirect_uri`, `state`, `code_challenge`, `code_challenge_method`, `scope` and `resource`. It refuses when: the client is unknown; `redirect_uri` is not an exact match for a registered entry; `code_challenge_method` is not `S256`; or `resource` is present and does not equal `{public_url}/mcp`. A refusal is HTTP 400 with a plain body, never a redirect. On success it writes an `oauth_login` row and answers HTTP 302 to the upstream authorization URL. The upstream `redirect_uri` is `{public_url}/oauth/callback`.

`GET /oauth/callback` reads `code` and `state`. It takes the login row, exchanges the code, and verifies the claims. It then looks up `iss` plus `sub` in the principals list. A miss answers HTTP 403 with a plain page and does not redirect. A hit stores the principal name on the login row and shows the consent page from Task 7.

- [ ] **Step 6: Run and watch them pass.**

```text
cargo test --test oauth_upstream -- --test-threads=1
```

- [ ] **Step 7: Check and commit.**

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

```bash
git add crates/mcpmem-oauth/src/upstream.rs crates/mcpmem-oauth/Cargo.toml crates/mcpmem-oauth/src/lib.rs src/oauth_routes.rs Cargo.toml Cargo.lock tests/oauth_upstream.rs tests/support
git commit -m "feat: authenticate the human at an OpenID Connect provider"
```

---

### Task 7: Consent and the authorization code

**Files:**
- Create: `crates/mcpmem-oauth/src/consent.rs`
- Create: `src/ui/consent.html`
- Create: `tests/support/flow.rs`
- Create: `tests/oauth_consent.rs`
- Modify: `src/oauth_routes.rs` (add `POST /oauth/consent`)

**Interfaces:**
- Produces `mcpmem_oauth::consent::page(client_name: &str, principal_label: &str, offered: &[String], csrf: &str, state: &str) -> String`.
- Produces `mcpmem_oauth::consent::approve(store, state: &str, csrf: &str, approved: &[String], now_us) -> Result<Approval, ConsentError>`, where `Approval` holds the code, the downstream `redirect_uri` and the client's own `state`.

- [ ] **Step 1: Write the failing tests.** Create `tests/oauth_consent.rs` with these cases, each with real code:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use mcpmem_oauth::consent::{escape_html, page};
use tower::ServiceExt;

mod support;
use support::flow::{Authorized, authorize_to_consent};

#[tokio::test]
async fn the_consent_page_offers_only_the_intersection() {
    // The principal in `support::principals` holds graph-read and graph-write.
    let a: Authorized = authorize_to_consent("graph-read graph-write code").await;
    assert!(a.body.contains(r#"value="graph-read""#));
    assert!(a.body.contains(r#"value="graph-write""#));
    assert!(!a.body.contains(r#"value="code""#), "code was not granted");
}

#[tokio::test]
async fn approval_issues_a_code_for_only_the_approved_scopes() {
    let a = authorize_to_consent("graph-read graph-write").await;
    let res = a
        .post_consent(&[("csrf", &a.csrf), ("state", &a.state), ("scope", "graph-read")])
        .await;
    assert_eq!(res.status(), StatusCode::FOUND);
    let code = support::flow::code_from(&support::header(&res, "location"));
    let stored = a.store.take_code(&code, a.now + 1).unwrap().unwrap();
    assert_eq!(stored.grant.scopes, vec!["graph-read"]);
}

#[tokio::test]
async fn a_wrong_csrf_token_is_refused() {
    let a = authorize_to_consent("graph-read").await;
    let res = a
        .post_consent(&[("csrf", "not-the-token"), ("state", &a.state), ("scope", "graph-read")])
        .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(a.count_codes(), 0);
}

#[tokio::test]
async fn approving_a_scope_the_principal_lacks_is_refused() {
    let a = authorize_to_consent("graph-read").await;
    let res = a
        .post_consent(&[("csrf", &a.csrf), ("state", &a.state), ("scope", "code")])
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(a.count_codes(), 0);
}

#[tokio::test]
async fn approving_nothing_is_refused() {
    let a = authorize_to_consent("graph-read").await;
    let res = a.post_consent(&[("csrf", &a.csrf), ("state", &a.state)]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(a.count_codes(), 0);
}

#[tokio::test]
async fn a_second_approval_finds_no_login_row() {
    let a = authorize_to_consent("graph-read").await;
    let first = a
        .post_consent(&[("csrf", &a.csrf), ("state", &a.state), ("scope", "graph-read")])
        .await;
    assert_eq!(first.status(), StatusCode::FOUND);
    let second = a
        .post_consent(&[("csrf", &a.csrf), ("state", &a.state), ("scope", "graph-read")])
        .await;
    assert_eq!(second.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_redirect_carries_the_client_state_and_the_iss_parameter() {
    let a = authorize_to_consent("graph-read").await;
    let res = a
        .post_consent(&[("csrf", &a.csrf), ("state", &a.state), ("scope", "graph-read")])
        .await;
    let location = support::header(&res, "location");
    assert!(location.starts_with("https://claude.ai/api/mcp/auth_callback?"));
    assert!(location.contains("code="));
    assert!(location.contains(&format!("state={}", a.client_state)));
    assert!(location.contains("iss=https%3A%2F%2Fmem.example.com"));
}

#[tokio::test]
async fn denial_redirects_with_access_denied() {
    let a = authorize_to_consent("graph-read").await;
    let res = a
        .post_consent(&[("csrf", &a.csrf), ("state", &a.state), ("deny", "1")])
        .await;
    let location = support::header(&res, "location");
    assert!(location.contains("error=access_denied"));
    assert!(location.contains(&format!("state={}", a.client_state)));
    assert_eq!(a.count_codes(), 0);
}

#[test]
fn a_hostile_client_name_is_escaped() {
    let html = page(
        "<script>alert(1)</script>",
        "adam@example.com",
        &["graph-read".to_string()],
        "csrf-value",
        "state-value",
    );
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(html.contains("&lt;script&gt;"));
}

#[test]
fn escape_html_covers_every_dangerous_character() {
    assert_eq!(escape_html(r#"<>&"'"#), "&lt;&gt;&amp;&quot;&#39;");
}
```

`support::flow` holds the helper that drives a client from `/oauth/authorize`
through the fake provider to the consent page. `Authorized` carries the rendered
`body`, the `csrf` value, the login `state`, the client's own `client_state`, the
`store`, and a fixed `now`. `post_consent` posts a form body. `count_codes`
queries `SELECT COUNT(*) FROM oauth_code`. `code_from` reads the `code` query
parameter out of a `Location` header.

- [ ] **Step 2: Run and watch them fail.**

```text
cargo test --test oauth_consent
```

- [ ] **Step 3: Write `src/ui/consent.html`.** One self-contained page. No script, no external asset. It shows the client name and the principal label. It shows one checkbox for each offered scope. It holds a hidden `csrf` field and a hidden `state` field. It has an Approve button and a Deny button. Embed it with `include_str!`, as `src/http.rs:44` does for the viewer.

Escape every value that reaches the page. A client name arrives from an unauthenticated registration request, so treat it as hostile. Write a small `escape_html` in `consent.rs` and test it with `<script>alert(1)</script>` as the client name.

- [ ] **Step 4: Implement `consent.rs` and the route.** Rules:

1. The offered set is the intersection of the requested scopes and the principal's scopes. An empty intersection is HTTP 400.
2. `approve` compares the `csrf` value in constant time.
3. An approved scope outside the offered set is HTTP 400.
4. The login row is consumed by approval, so a second post finds nothing.
5. The redirect carries `code`, the client's own `state` when it sent one, and `iss` set to the public URL.
6. Deny redirects with `error=access_denied` and the same `state` and `iss`.

- [ ] **Step 5: Run and watch them pass.**

```text
cargo test --test oauth_consent -- --test-threads=1
```

- [ ] **Step 6: Check and commit.**

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

```bash
git add crates/mcpmem-oauth/src/consent.rs src/ui/consent.html src/oauth_routes.rs crates/mcpmem-oauth/src/lib.rs tests/oauth_consent.rs
git commit -m "feat: ask the human for least-privilege consent"
```

---

### Task 8: Tokens, and the resource server

**Files:**
- Create: `crates/mcpmem-oauth/src/token.rs`
- Create: `tests/oauth_flow.rs`
- Modify: `tests/support/flow.rs` (add `Flow`, `Reply`, the clock)
- Modify: `src/oauth_routes.rs` (add `POST /oauth/token` and `POST /oauth/revoke`)
- Modify: `src/http.rs:142-233` (resolve a bearer token into a principal)

**Interfaces:**
- Produces `mcpmem_oauth::token::grant_authorization_code(store, params, now_us) -> Result<TokenResponse, TokenError>`.
- Produces `mcpmem_oauth::token::grant_refresh(store, params, now_us) -> Result<TokenResponse, TokenError>`.
- Produces `mcpmem_oauth::token::revoke(store, token: &str) -> Result<(), TokenError>`.
- Produces `mcpmem_oauth::token::validate(store, token: &str, resource: &str, now_us) -> Option<Grant>`.
- Consumes `mcpmem::authz::oauth_principal` from Task 1.

- [ ] **Step 1: Write the failing conformance test.** Create `tests/oauth_flow.rs`. One test walks the whole path, in this order, and asserts at every hop:

1. `POST /mcp` with no token returns HTTP 401 and a `WWW-Authenticate` header.
2. `GET` the `resource_metadata` URL from that header returns the document.
3. `GET /.well-known/oauth-authorization-server` returns the document.
4. `POST /oauth/register` returns a client identifier.
5. `GET /oauth/authorize` with a generated verifier returns HTTP 302 to the fake provider.
6. Following the provider returns HTTP 302 back to `/oauth/callback`.
7. `GET /oauth/callback` returns the consent page and a CSRF value.
8. `POST /oauth/consent` returns HTTP 302 to the client redirect with a code.
9. `POST /oauth/token` returns an access token and a refresh token.
10. `POST /mcp` with the access token returns a `tools/list` result that holds only the approved scopes' tools.

- [ ] **Step 2: Write the failing negative tests** in the same file:

```rust
use axum::http::StatusCode;

use support::flow::{Flow, VERIFIER};

#[tokio::test]
async fn a_wrong_code_verifier_is_refused() {
    let f = Flow::to_code("graph-read").await;
    let res = f.token(&[("code", &f.code), ("code_verifier", "another-verifier")]).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_grant");
}

#[tokio::test]
async fn a_replayed_authorization_code_is_refused() {
    let f = Flow::to_code("graph-read").await;
    let first = f.token(&[("code", &f.code), ("code_verifier", VERIFIER)]).await;
    assert_eq!(first.status, StatusCode::OK);
    let second = f.token(&[("code", &f.code), ("code_verifier", VERIFIER)]).await;
    assert_eq!(second.status, StatusCode::BAD_REQUEST);
    assert_eq!(second.body["error"], "invalid_grant");
}

#[tokio::test]
async fn an_expired_authorization_code_is_refused() {
    let f = Flow::to_code("graph-read").await;
    f.advance_seconds(61);
    let res = f.token(&[("code", &f.code), ("code_verifier", VERIFIER)]).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_grant");
}

#[tokio::test]
async fn a_redirect_uri_that_differs_by_one_character_is_refused() {
    let f = Flow::to_code("graph-read").await;
    let res = f
        .token(&[
            ("code", &f.code),
            ("code_verifier", VERIFIER),
            ("redirect_uri", "https://claude.ai/api/mcp/auth_callbacK"),
        ])
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body["error"], "invalid_grant");
}

#[tokio::test]
async fn an_authorize_request_naming_another_resource_is_refused() {
    let f = Flow::registered().await;
    let res = f.authorize(&[("resource", "https://other.example/mcp")]).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_token_bound_to_another_resource_is_refused() {
    let f = Flow::to_access_token("graph-read").await;
    // Rewrite the stored audience, which is what a token from another instance
    // would carry. The transport must refuse it.
    f.rebind_resource("https://other.example/mcp");
    let res = f.mcp_tools_list(&f.access_token).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_reused_refresh_token_kills_the_family() {
    let f = Flow::to_access_token("graph-read").await;
    let first = f.refresh(&f.refresh_token).await;
    assert_eq!(first.status, StatusCode::OK);
    let new_access = first.body["access_token"].as_str().unwrap().to_owned();

    let replay = f.refresh(&f.refresh_token).await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST);
    assert_eq!(replay.body["error"], "invalid_grant");

    let after = f.mcp_tools_list(&new_access).await;
    assert_eq!(
        after.status,
        StatusCode::UNAUTHORIZED,
        "the whole family must die, including the token issued by the first refresh"
    );
}

#[tokio::test]
async fn a_read_only_token_calling_a_write_tool_gets_403_with_the_scope() {
    let f = Flow::to_access_token("graph-read").await;
    let res = f.mcp_call(&f.access_token, "delete_entities").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert_eq!(
        res.www_authenticate,
        "Bearer error=\"insufficient_scope\", scope=\"graph-write\", \
         resource_metadata=\"https://mem.example.com/.well-known/oauth-protected-resource\""
    );
}

#[tokio::test]
async fn a_read_only_token_does_not_see_write_tools_in_the_list() {
    let f = Flow::to_access_token("graph-read").await;
    let names = f.tool_names(&f.access_token).await;
    assert!(names.contains(&"read_graph".to_string()));
    assert!(!names.contains(&"delete_entities".to_string()));
}

#[tokio::test]
async fn a_revoked_token_is_refused() {
    let f = Flow::to_access_token("graph-read").await;
    assert_eq!(f.mcp_tools_list(&f.access_token).await.status, StatusCode::OK);
    let revoked = f.revoke(&f.access_token).await;
    assert_eq!(revoked.status, StatusCode::OK);
    assert_eq!(
        f.mcp_tools_list(&f.access_token).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn an_unknown_token_is_refused_with_the_challenge() {
    let f = Flow::registered().await;
    let res = f.mcp_tools_list("not-a-token").await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(res.www_authenticate.contains("resource_metadata="));
}
```

`support::flow::Flow` is the driver for this file. It owns the router, the fake
provider, the store, and a mutable clock. Its constructors are `registered`,
`to_code` and `to_access_token`; each one stops at the named stage. Its methods
`authorize`, `token`, `refresh`, `revoke`, `mcp_tools_list`, `mcp_call` and
`tool_names` return a small `Reply` holding `status`, `body` and
`www_authenticate`. `advance_seconds` moves the clock, and `rebind_resource`
writes a different audience straight into `oauth_token`.

The clock is a parameter, not the wall clock. Give `OauthState` a
`now_us: Arc<dyn Fn() -> i64 + Send + Sync>`, defaulting to
`mcpmem_core::events::now_us`. Expiry tests cannot sleep for an hour.

Keep the 403 test as the definition of the header format. Make the helper in
`src/http.rs` produce exactly that order.

- [ ] **Step 3: Run and watch them fail.**

```text
cargo test --test oauth_flow
```

- [ ] **Step 4: Implement `token.rs`.** Rules:

1. The authorization code grant verifies `s256_challenge(code_verifier)` against the stored challenge, in constant time.
2. It verifies that `client_id` and `redirect_uri` equal the values stored with the code.
3. The access token lives one hour. The refresh token lives thirty days. Both carry the same `family`.
4. The refresh grant calls `store.take_refresh`. `Replayed` returns `invalid_grant` and the store has already revoked the family. `Valid` issues a new pair in the same family.
5. `validate` returns `None` when the record is missing, expired, revoked, or its `resource` is not the caller's canonical URI.
6. Revocation accepts an access token or a refresh token. It revokes the whole family. That is simpler than partial revocation, and RFC 7009 allows it.

- [ ] **Step 5: Resolve the token in the transport.** In `src/http.rs`, replace `authorized` with a function that returns a `Principal`:

```rust
/// Resolve the caller. Order: OAuth token, then the static bearer, then the
/// fully open case. `None` means the request is unauthorized.
fn principal_of(state: &HttpState, headers: &HeaderMap) -> Option<Principal> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().strip_prefix("Bearer ").unwrap_or(v).trim());

    if let (Some(oauth), Some(token)) = (state.oauth.as_ref(), presented) {
        let resource = format!("{}/mcp", oauth.config.public_url);
        if let Some(grant) = oauth.validate(token, &resource) {
            let scopes = grant.scopes.into_iter().collect();
            return Some(crate::authz::oauth_principal(&grant.principal, scopes));
        }
    }
    match (state.auth_token.as_ref(), presented) {
        (Some(expected), Some(token)) if server::token_matches(token, expected) => {
            Some(crate::authz::bearer_principal(&state.bearer_scopes))
        }
        (None, _) if state.oauth.is_none() => Some(crate::authz::local_principal()),
        _ => None,
    }
}
```

`post_handler` and `get_handler` call it, answer `unauthorized(state)` on `None`, and pass the principal to `dispatch_http_body`.

- [ ] **Step 6: Run the whole flow suite and watch it pass.**

```text
cargo test --test oauth_flow -- --test-threads=1
```

- [ ] **Step 7: Run every affected suite.**

```text
cargo test --workspace --all-targets --all-features -- --test-threads=1
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

- [ ] **Step 8: Commit.**

```bash
git add crates/mcpmem-oauth/src/token.rs crates/mcpmem-oauth/src/lib.rs src/oauth_routes.rs src/http.rs tests/oauth_flow.rs
git commit -m "feat: issue and validate audience-bound OAuth tokens"
```

---

### Task 9: Limits, sweeping, and documentation

**Files:**
- Create: `crates/mcpmem-oauth/src/limits.rs`
- Create: `docs/runbooks/oauth-deployment.md`
- Modify: `tests/support/flow.rs` (add `fresh`, `register_from`, `insert_expired_family`, `count`, `now`)
- Modify: `crates/mcpmem-oauth/src/store.rs` (call `sweep`)
- Modify: `src/runtime.rs` (schedule the sweep)
- Modify: `README.md`
- Modify: `Cargo.toml` (version)
- Modify: `tests/oauth_flow.rs` (rate-limit cases)

**Interfaces:**
- Produces `mcpmem_oauth::limits::RateLimiter::check(&self, key: &str, now_us: i64) -> bool`.

- [ ] **Step 1: Write the failing limit tests.** Add to `tests/oauth_flow.rs`:

```rust
#[tokio::test]
async fn registration_is_rate_limited_per_peer() {
    let f = Flow::fresh().await;
    for i in 0..20 {
        let res = f.register_from("203.0.113.7").await;
        assert_eq!(res.status, StatusCode::CREATED, "request {i} must pass");
    }
    let blocked = f.register_from("203.0.113.7").await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(!blocked.retry_after.is_empty(), "Retry-After must be set");

    // Another peer is unaffected by the first peer's window.
    let other = f.register_from("203.0.113.8").await;
    assert_eq!(other.status, StatusCode::CREATED);

    // The window rolls over.
    f.advance_seconds(61);
    let later = f.register_from("203.0.113.7").await;
    assert_eq!(later.status, StatusCode::CREATED);
}

#[tokio::test]
async fn the_sweep_removes_expired_rows_and_keeps_live_ones() {
    let f = Flow::to_access_token("graph-read").await;
    let live = f.access_token.clone();

    // A second, already-expired family.
    f.insert_expired_family("dead");
    assert_eq!(f.count("oauth_token"), 3, "one live pair plus one dead access");

    let removed = f.store.sweep(f.now()).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(f.mcp_tools_list(&live).await.status, StatusCode::OK);

    // After an hour every access token is gone, and the refresh token remains.
    f.advance_seconds(3601);
    let removed = f.store.sweep(f.now()).unwrap();
    assert!(removed >= 1);
    assert_eq!(f.mcp_tools_list(&live).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(f.count("oauth_token"), 1, "the refresh token outlives the access token");
}
```

`Flow::fresh` builds the router with no client registered. `register_from` sends
a registration request with a given peer address. `Reply` gains a `retry_after`
field. `insert_expired_family` writes one already-expired access token.
`count(table)` runs `SELECT COUNT(*)`. `now()` reads the test clock.

- [ ] **Step 2: Run and watch them fail.**

```text
cargo test --test oauth_flow
```

- [ ] **Step 3: Implement `limits.rs`.** Use a fixed-window counter in memory, keyed by peer address. Bound the number of keys, so the map cannot grow without limit. Default: 20 requests each minute for `/oauth/register`. Default: 60 requests each minute for `/oauth/token` and `/oauth/authorize`. Over the limit, answer HTTP 429 with `Retry-After`.

The peer address comes from `axum::extract::ConnectInfo`. With `--oauth-trust-forwarded-proto` set, read the leftmost `X-Forwarded-For` entry instead, and say so in the runbook.

- [ ] **Step 4: Schedule the sweep.** Call `store.sweep(now_us())` from the existing periodic tick. Read `src/runtime.rs:13-61` and attach it where the other periodic work runs. Do not start a new thread. Also evict clients with `last_used_us` older than thirty days that never reached a token.

- [ ] **Step 5: Run and watch them pass.**

```text
cargo test --test oauth_flow -- --test-threads=1
```

- [ ] **Step 6: Write the runbook.** `docs/runbooks/oauth-deployment.md` covers:

1. Registering `mcpmem` at the upstream provider, with redirect URI `{public_url}/oauth/callback`.
2. Finding a human's `sub` value at that provider, and writing the principals file.
3. The full command line, in Bash and in fish. State when the two are identical.
4. Adding the connector in Claude, and in ChatGPT.
5. Revoking one person's access, and revoking every token.
6. What each refusal message means.

- [ ] **Step 7: Update `README.md`.** Add an OAuth section with the shortest working example. Say that the static bearer path still exists.

- [ ] **Step 8: Set the version.** This release adds behaviour and breaks nothing. Set `version = "1.1.0-rc.1"` in `Cargo.toml`, and update `Cargo.lock` with a build. Confirm the number with the operator before the release tag.

- [ ] **Step 9: Run the full matrix.**

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets -- --test-threads=1
cargo test --test indexer_worker --features indexer -- --test-threads=1
cargo test --test indexer_worker --no-default-features --features indexer -- --test-threads=1
cargo test --test role_composition --no-default-features
cargo test --test role_composition --features indexer
cargo test --test role_composition --features webhooks
cargo test --test role_composition --features indexer,webhooks
```

- [ ] **Step 10: Commit.**

```bash
git add crates/mcpmem-oauth/src/limits.rs crates/mcpmem-oauth/src/store.rs src/runtime.rs README.md docs/runbooks/oauth-deployment.md Cargo.toml Cargo.lock tests/oauth_flow.rs
git commit -m "feat: bound OAuth abuse and document the deployment"
```

---

## Requirement coverage

| Requirement | Task |
| --- | --- |
| R1 protected resource metadata | 4 |
| R2 authorization server metadata | 4 |
| R3 401 challenge with a metadata link | 4 |
| R4 dynamic client registration with limits | 5, 9 |
| R5 metadata documents from allowed domains | 5 |
| R6 upstream discovery | 6 |
| R7 identity token verification | 6 |
| R8 principal lookup | 2, 6 |
| R9 least-privilege consent | 7 |
| R10 audience-bound access tokens | 8 |
| R11 rotating refresh tokens | 3, 8 |
| R12 digest-only storage | 3 |
| R13 per-tool scope checks and filtered `tools/list` | 1 |
| R14 403 with `insufficient_scope` | 1, 8 |
| R15 static bearer keeps working | 1, 2, 8 |
| R16 the four startup refusals | 2 |
| R17 migration and sweep | 3, 9 |
| R18 scope derivation test | 1 |
| R19 revocation | 8 |
| R20 manual acceptance with Claude and ChatGPT | final gate |

## Final verification and release gate

- [ ] Run the full matrix from Task 9, step 9, and record the output.
- [ ] Deploy behind TLS and connect the real Claude custom connector. Record the
      client identifier that Claude registered, and the scopes granted.
- [ ] Connect the real ChatGPT connector against the same deployment. Record
      whether it used a metadata document or dynamic registration.
- [ ] Confirm that a revoked token stops working within one request.
- [ ] Update this plan with the commit identifiers and the executed commands.
