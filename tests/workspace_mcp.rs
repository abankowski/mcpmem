//! MCP workspace selection and the workspace-management tools.
//!
//! Every graph, vector, export and webhook tool accepts an optional
//! `workspaceId` per call; omission selects the caller's saved default and an
//! explicit ID never changes that default. The management tools
//! (`create_workspace`, `list_workspaces`, `get_workspace`,
//! `set_workspace_visibility`, `list_workspace_grants`,
//! `grant_workspace_access`, `revoke_workspace_access`,
//! `set_default_workspace`, `create_machine_account`, `list_machine_accounts`,
//! `revoke_machine_account`) own grants, visibility, defaults and machine
//! credentials.
//!
//! These tests are written against the approved spec (§"MCP contract") before
//! Task 3 lands. The `workspaceId` selector and the management tools do not
//! exist yet, so each test below fails exactly for that reason: an omitted
//! workspace is not resolved, a supplied one is ignored, and the management
//! tools answer `Method not found` (`-32601`). A comment marks every
//! assertion that is red before the selector and tools land. The tests turn
//! green when Task 3 wires selection behind the existing per-call dispatcher;
//! they need no other production change.

use mcpmem::authz::{Principal, bearer_principal, local_principal};
use mcpmem::config::{Config, Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;
use mcpmem::server::{HttpOutcome, MCPServer, dispatch_http_body};
use mcpmem::tools::ToolCategory;
use mcpmem::vector_store::{VectorConfig, VectorStore};
use mcpmem::workspace::{Visibility, WorkspaceRegistry};
use serde_json::{Value, json};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

/// A server with `graph-read`, `graph-write` and `vectors` enabled, one
/// legacy (default) graph owned by `machine:local`, and the static bearer
/// (`machine:static`) registered so a static caller is a real identity.
struct Fixture {
    _dir: tempfile::TempDir,
    kg: Arc<GraphHandle>,
    registry: Arc<WorkspaceRegistry>,
    vs: Option<Arc<VectorStore>>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        memory_file_path: dir
            .path()
            .join("memory.sqlite")
            .to_string_lossy()
            .into_owned(),
        legacy_owner_id: Some("machine:local".into()),
        auth_token: Some("test-bearer".into()),
        enabled_categories: vec![
            ToolCategory::GraphRead,
            ToolCategory::GraphWrite,
            ToolCategory::Vectors,
        ],
        vectors_enabled: true,
        ..Config::default()
    };
    let server = MCPServer::new(config, VectorConfig::new(2)).expect("test server builds");
    let kg = server.graph();
    let registry = server.workspace_registry();
    let vs = server.vector_store();
    Fixture {
        _dir: dir,
        kg,
        registry,
        vs,
    }
}

/// Open and close a graph file so `registry.create` accepts it.
fn graph(path: &Path) -> GraphHandle {
    GraphHandle::new(
        path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .expect("graph opens")
}

/// Create a workspace through the registry (Task 1's approved interface) with
/// `machine:local` as owner. The MCP `create_workspace` tool has its own test.
fn create_graph(fx: &Fixture, name: &str, visibility: Visibility) -> String {
    fx.registry
        .create("machine:local", name, visibility, |path| {
            drop(graph(path));
            Ok(())
        })
        .expect("workspace creation succeeds")
        .workspace_id
}

/// Create a machine account and authenticate it. Returns `(id, principal)`.
fn machine(fx: &Fixture, name: &str, scopes: &[&str]) -> (String, Principal) {
    let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    let (id, token) = fx
        .registry
        .create_machine(name, &scopes)
        .expect("machine account created");
    let principal = fx
        .registry
        .authenticate_machine(&token)
        .expect("machine authentication works")
        .expect("the fresh token is valid");
    (id, principal)
}

fn entity(name: &str, entity_type: &str) -> Value {
    json!({ "name": name, "entityType": entity_type, "observations": [] })
}

fn body_of(outcome: HttpOutcome) -> Value {
    match outcome {
        HttpOutcome::Body(v) => v,
        other => panic!("expected a body, got {other:?}"),
    }
}

fn dispatch(fx: &Fixture, principal: &Principal, body: &str) -> HttpOutcome {
    dispatch_http_body(body, &fx.kg, fx.vs.as_deref(), principal).expect("valid JSON body")
}

/// One `tools/call` as `principal`, returning the JSON-RPC response.
fn call(fx: &Fixture, principal: &Principal, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
    .to_string();
    body_of(dispatch(fx, principal, &body))
}

/// One JSON-RPC batch as `principal`; each entry is `(tool name, arguments)`.
fn batch(fx: &Fixture, principal: &Principal, calls: &[(&str, Value)]) -> Value {
    let items: Vec<Value> = calls
        .iter()
        .map(|(name, arguments)| {
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments },
            })
        })
        .collect();
    let body = serde_json::to_string(&Value::Array(items)).unwrap();
    body_of(dispatch(fx, principal, &body))
}

/// The `text` of a tool result, parsed back into JSON.
fn tool_json(response: &Value) -> Value {
    let text = tool_text(response);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("tool text was not JSON: {e}: {text}"))
}

fn tool_text(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text content")
        .to_owned()
}

/// A tool-level error (`result.isError`).
fn is_tool_error(response: &Value) -> bool {
    response["result"]["isError"].as_bool().unwrap_or(false)
}

/// Any rejection shape: a tool-level error or a JSON-RPC protocol error.
fn is_denied(response: &Value) -> bool {
    is_tool_error(response) || response["error"].is_object()
}

/// `get_entity` on `workspace_id`, asserting the call itself succeeded.
fn get_entity(fx: &Fixture, principal: &Principal, workspace_id: &str, name: &str) -> Value {
    let response = call(
        fx,
        principal,
        "get_entity",
        json!({ "workspaceId": workspace_id, "name": name }),
    );
    assert!(
        !is_denied(&response),
        "get_entity({name}) in {workspace_id} failed: {response}"
    );
    tool_json(&response)
}

#[cfg(feature = "webhooks")]
const SECRET_REF: &str = "vault://webhook/test";

/// The first workspace a caller creates becomes its default; a later one does
/// not replace it. RED: `create_workspace` does not exist yet, so every call
/// below answers `Method not found`.
#[test]
fn create_workspace_records_ownership_and_the_first_default() {
    let fx = fixture();
    // A caller without a default: a machine account has none until one is set.
    let (_, owner) = machine(&fx, "new-owner", &["graph-write"]);

    let first = call(
        &fx,
        &owner,
        "create_workspace",
        json!({ "name": "One", "visibility": "private" }),
    );
    assert!(
        !is_denied(&first),
        "create_workspace must succeed for a graph-write caller: {first}"
    );
    let ws = first["result"]["workspace"].clone();
    let first_id = ws["workspaceId"].as_str().expect("workspaceId").to_owned();
    assert_eq!(ws["name"], "One");
    assert_eq!(ws["visibility"], "private");
    assert_eq!(ws["role"], "owner");
    assert_eq!(
        ws["isDefault"].as_bool(),
        Some(true),
        "the first workspace becomes the caller's default"
    );

    let second = call(
        &fx,
        &owner,
        "create_workspace",
        json!({ "name": "Two", "visibility": "public" }),
    );
    assert!(
        !is_denied(&second),
        "a second create_workspace must also succeed: {second}"
    );
    let ws2 = second["result"]["workspace"].clone();
    assert_ne!(ws2["workspaceId"].as_str(), Some(first_id.as_str()));
    assert_eq!(
        ws2["isDefault"].as_bool(),
        Some(false),
        "a later workspace never replaces the first default"
    );
}

/// An explicit `workspaceId` selects only that graph, and the saved default
/// is untouched by any explicit call. RED: the selector is absent, so every
/// write lands in the legacy graph and reads cross the boundary.
#[test]
fn explicit_workspace_id_selects_only_that_graph_and_never_changes_the_default() {
    let fx = fixture();
    let a = create_graph(&fx, "One", Visibility::Private);
    let b = create_graph(&fx, "Two", Visibility::Private);
    let owner = local_principal();

    // The default (legacy) graph gets its own anchor, then both workspaces
    // get an entity with the same name — the W-02 isolation shape.
    call(
        &fx,
        &owner,
        "create_entities",
        json!({ "entities": [entity("legacy-anchor", "Anchor")] }),
    );
    call(
        &fx,
        &owner,
        "create_entities",
        json!({ "workspaceId": a, "entities": [entity("same-name", "TypeOne")] }),
    );
    call(
        &fx,
        &owner,
        "create_entities",
        json!({ "workspaceId": b, "entities": [entity("same-name", "TypeTwo")] }),
    );

    let in_a = get_entity(&fx, &owner, &a, "same-name");
    assert_eq!(in_a["entityType"], "TypeOne", "{in_a}");
    let in_b = get_entity(&fx, &owner, &b, "same-name");
    assert_eq!(
        in_b["entityType"], "TypeTwo",
        "RED: both writes land in one graph today, so B reads A's row: {in_b}"
    );

    // The explicit calls above must not have moved the caller's default: the
    // no-id call still reads the legacy graph, not A and not B.
    let default_read = tool_text(&call(&fx, &owner, "read_graph", json!({})));
    assert!(default_read.contains("legacy-anchor"), "{default_read}");
    assert!(
        !default_read.contains("same-name"),
        "RED: the default read leaks the explicitly written workspaces: {default_read}"
    );
}

/// A private grant of `reader` denies writes and allows reads; `writer`
/// allows both. RED: no workspace ACL, so the reader's write applies.
#[test]
fn a_reader_grant_denies_writes_and_a_writer_grant_allows_them() {
    let fx = fixture();
    let b = create_graph(&fx, "Two", Visibility::Private);
    let (reader_id, reader) = machine(&fx, "reader", &["graph-read", "graph-write"]);
    let (writer_id, writer) = machine(&fx, "writer", &["graph-read", "graph-write"]);
    fx.registry
        .grant("machine:local", &b, &reader_id, "reader")
        .unwrap();
    fx.registry
        .grant("machine:local", &b, &writer_id, "writer")
        .unwrap();

    let denied = call(
        &fx,
        &reader,
        "create_entities",
        json!({ "workspaceId": b, "entities": [entity("smuggled", "Thing")] }),
    );
    assert!(
        is_denied(&denied),
        "RED: a reader's write must be refused, but it applied: {denied}"
    );

    let allowed = call(
        &fx,
        &writer,
        "create_entities",
        json!({ "workspaceId": b, "entities": [entity("writer-ok", "Thing")] }),
    );
    assert!(
        !is_denied(&allowed),
        "a writer's write must apply: {allowed}"
    );

    let read = get_entity(&fx, &reader, &b, "writer-ok");
    assert_eq!(read["entityType"], "Thing", "reader may read the grant");
}

/// A static caller with `graph-read` scope reads a public graph but gets no
/// write from publicity, and a private graph stays closed to it. RED: no
/// workspace ACL, so the writes and the private read all succeed today.
#[test]
fn a_static_caller_with_graph_read_scope_reads_public_but_cannot_write() {
    let fx = fixture();
    let public = create_graph(&fx, "Public", Visibility::Public);
    let private = create_graph(&fx, "Private", Visibility::Private);
    let owner = local_principal();
    call(
        &fx,
        &owner,
        "create_entities",
        json!({ "workspaceId": public, "entities": [entity("open-secret", "OpenType")] }),
    );
    // The static caller also holds graph-write so the write refusal below can
    // only come from the workspace ACL, never from the scope gate.
    let static_caller = bearer_principal(&[ToolCategory::GraphRead, ToolCategory::GraphWrite]);

    let read = get_entity(&fx, &static_caller, &public, "open-secret");
    assert_eq!(read["entityType"], "OpenType", "public read must work");

    let write = call(
        &fx,
        &static_caller,
        "create_entities",
        json!({ "workspaceId": public, "entities": [entity("static-write", "Thing")] }),
    );
    assert!(
        is_denied(&write),
        "RED: public readability must not imply write access: {write}"
    );

    let closed = call(
        &fx,
        &static_caller,
        "get_entity",
        json!({ "workspaceId": private, "name": "open-secret" }),
    );
    assert!(
        is_denied(&closed),
        "RED: a private graph must refuse an unrelated static caller: {closed}"
    );
}

/// `list_workspaces` shows owned and granted graphs and public ones, and
/// never the private graphs of others. `get_workspace` names the owner only
/// to the owner. RED: the tools are absent (both are `Method not found`).
#[test]
fn list_workspaces_keeps_private_graphs_out_of_other_callers_pages() {
    let fx = fixture();
    let a = create_graph(&fx, "Secret-A", Visibility::Private);
    let b = create_graph(&fx, "Secret-B", Visibility::Private);
    let public = create_graph(&fx, "Public", Visibility::Public);
    let owner = local_principal();
    let (_, outsider) = machine(&fx, "outsider", &["graph-read"]);

    let page = call(&fx, &owner, "list_workspaces", json!({ "limit": 100 }));
    assert!(
        !is_denied(&page),
        "list_workspaces must succeed for the owner: {page}"
    );
    let body = tool_json(&page);
    assert_eq!(body["nextCursor"], Value::Null, "small list, no cursor");
    let owned: Vec<&str> = body["workspaces"]
        .as_array()
        .expect("workspaces")
        .iter()
        .map(|w| w["workspaceId"].as_str().expect("workspaceId"))
        .collect();
    assert!(owned.contains(&a.as_str()), "{owned:?}");
    assert!(owned.contains(&b.as_str()), "{owned:?}");

    let page = call(&fx, &outsider, "list_workspaces", json!({ "limit": 100 }));
    assert!(
        !is_denied(&page),
        "list_workspaces must succeed for an authenticated outsider: {page}"
    );
    let body = tool_json(&page);
    let visible: Vec<&str> = body["workspaces"]
        .as_array()
        .expect("workspaces")
        .iter()
        .map(|w| w["workspaceId"].as_str().expect("workspaceId"))
        .collect();
    assert!(
        visible.contains(&public.as_str()),
        "public graphs are visible to authenticated callers: {visible:?}"
    );
    assert!(
        !visible.contains(&a.as_str()) && !visible.contains(&b.as_str()),
        "private metadata must not leak through the list: {visible:?}"
    );

    let owned = call(&fx, &owner, "get_workspace", json!({ "workspaceId": b }));
    assert!(
        !is_denied(&owned),
        "the owner can read its workspace: {owned}"
    );
    let ws = &owned["result"]["workspace"];
    assert_eq!(ws["role"], "owner");
    assert!(
        ws["ownerId"].is_string(),
        "only the owner sees the owner identity"
    );

    let closed = call(&fx, &outsider, "get_workspace", json!({ "workspaceId": b }));
    assert!(
        is_denied(&closed),
        "a private workspace is not readable by an outsider: {closed}"
    );
}

/// Visibility and grant changes are owner-only, and a grant takes effect on
/// the next call. RED: the tools are absent, so the owner's calls fail.
#[test]
fn grant_and_visibility_changes_require_the_owner() {
    let fx = fixture();
    let b = create_graph(&fx, "Mine", Visibility::Private);
    let (reader_id, _reader) = machine(&fx, "reader", &["graph-read"]);
    let (writer_id, writer) = machine(&fx, "writer", &["graph-read", "graph-write"]);
    fx.registry
        .grant("machine:local", &b, &writer_id, "writer")
        .unwrap();
    let owner = local_principal();

    let denied = call(
        &fx,
        &writer,
        "set_workspace_visibility",
        json!({ "workspaceId": b, "visibility": "public" }),
    );
    assert!(
        is_denied(&denied),
        "a writer must not change visibility: {denied}"
    );
    let denied = call(
        &fx,
        &writer,
        "grant_workspace_access",
        json!({ "workspaceId": b, "principalId": reader_id, "role": "reader" }),
    );
    assert!(
        is_denied(&denied),
        "a writer must not grant access: {denied}"
    );

    let changed = call(
        &fx,
        &owner,
        "set_workspace_visibility",
        json!({ "workspaceId": b, "visibility": "public" }),
    );
    assert!(
        !is_denied(&changed),
        "RED: the owner's visibility change must succeed: {changed}"
    );
    assert_eq!(changed["result"]["workspace"]["visibility"], "public");

    let granted = call(
        &fx,
        &owner,
        "grant_workspace_access",
        json!({ "workspaceId": b, "principalId": reader_id, "role": "reader" }),
    );
    assert!(
        !is_denied(&granted),
        "RED: the owner's grant must succeed: {granted}"
    );
    assert_eq!(granted["result"]["grant"]["role"], "reader");

    let grants = call(
        &fx,
        &owner,
        "list_workspace_grants",
        json!({ "workspaceId": b }),
    );
    assert!(
        !is_denied(&grants),
        "RED: list_workspace_grants must succeed for the owner: {grants}"
    );
    let listed = tool_json(&grants);
    assert!(
        listed["grants"]
            .as_array()
            .expect("grants")
            .iter()
            .any(|g| g["principalId"] == reader_id.as_str()),
        "{listed}"
    );

    let revoked = call(
        &fx,
        &owner,
        "revoke_workspace_access",
        json!({ "workspaceId": b, "principalId": reader_id }),
    );
    assert!(
        !is_denied(&revoked),
        "RED: revoke_workspace_access must succeed for the owner: {revoked}"
    );
    assert_eq!(tool_json(&revoked)["revoked"].as_bool(), Some(true));
}

/// A call without `workspaceId` and without a saved default fails with
/// selection required; it never falls back to a public or legacy graph.
/// RED: the selector is absent, so the call reads the legacy graph today.
#[test]
fn a_call_without_workspace_id_and_without_a_default_fails_selection() {
    let fx = fixture();
    let b = create_graph(&fx, "Gated", Visibility::Private);
    let (reader_id, reader) = machine(&fx, "reader", &["graph-read"]);
    fx.registry
        .grant("machine:local", &b, &reader_id, "reader")
        .unwrap();
    let owner = local_principal();
    call(
        &fx,
        &owner,
        "create_entities",
        json!({ "workspaceId": b, "entities": [entity("b-anchor", "Anchor")] }),
    );

    // The granted reader has no default. The call must fail selection, not
    // fall back to the legacy graph where the row also exists today.
    let no_default = call(&fx, &reader, "get_entity", json!({ "name": "b-anchor" }));
    assert!(
        is_denied(&no_default),
        "RED: without a default the call must fail selection, not read: {no_default}"
    );

    // Control: once a default is saved, the identical call resolves.
    fx.registry.set_default(&reader_id, &b).unwrap();
    let with_default = call(&fx, &reader, "read_graph", json!({}));
    assert!(
        !is_denied(&with_default),
        "with a default the no-id call must succeed: {with_default}"
    );
    assert!(
        tool_text(&with_default).contains("b-anchor"),
        "the default must select the granted workspace"
    );
}

/// One fixture drives export, FTS, relation traversal, taxonomy and the
/// vector cache against two workspaces carrying the same node name; every
/// call passes an explicit `workspaceId` and must serve only that file.
/// RED: the selector is absent, so each call serves the shared legacy graph.
#[test]
fn one_fixture_runs_export_fts_relation_traversal_taxonomy_and_vectors_per_workspace() {
    let fx = fixture();
    let a = create_graph(&fx, "One", Visibility::Private);
    let b = create_graph(&fx, "Two", Visibility::Private);
    let owner = local_principal();

    let seed_legacy = json!({ "entities": [entity("legacy-anchor", "Anchor")] });
    call(&fx, &owner, "create_entities", seed_legacy);

    let seed_a = json!({
        "workspaceId": a,
        "entities": [entity("same-name", "TypeOne"), entity("alpha-neighbor", "Neighbor")],
    });
    call(&fx, &owner, "create_entities", seed_a);
    let rel_a = json!({
        "workspaceId": a,
        "relations": [
            { "from": "same-name", "to": "alpha-neighbor", "relationType": "links" },
        ],
    });
    call(&fx, &owner, "create_relations", rel_a);

    let seed_b = json!({
        "workspaceId": b,
        "entities": [entity("same-name", "TypeTwo"), entity("beta-neighbor", "Neighbor")],
    });
    call(&fx, &owner, "create_entities", seed_b);
    let rel_b = json!({
        "workspaceId": b,
        "relations": [
            { "from": "same-name", "to": "beta-neighbor", "relationType": "links" },
        ],
    });
    call(&fx, &owner, "create_relations", rel_b);

    // Export: B's export carries B's type and neighbor, never A's.
    let exported = call(
        &fx,
        &owner,
        "export_graph",
        json!({ "format": "json", "workspaceId": b }),
    );
    assert!(!is_denied(&exported), "{exported}");
    let text = tool_text(&exported);
    assert!(
        text.contains("TypeTwo") && text.contains("beta-neighbor"),
        "{text}"
    );
    assert!(
        !text.contains("TypeOne") && !text.contains("alpha-neighbor"),
        "RED: the export must not carry the other workspace's rows: {text}"
    );

    // FTS: searching B matches B's rows only. The FTS5 tokenizer splits the
    // hyphenated name, so the query uses its shared "same" token.
    let searched = call(
        &fx,
        &owner,
        "search_nodes",
        json!({ "query": "same", "limit": 100, "workspaceId": b }),
    );
    assert!(!is_denied(&searched), "{searched}");
    let rows = tool_json(&searched);
    let types: Vec<&str> = rows
        .as_array()
        .expect("search rows")
        .iter()
        .map(|r| r["entityType"].as_str().expect("entityType"))
        .collect();
    assert!(types.contains(&"TypeTwo"), "{types:?}");
    assert!(
        !types.contains(&"TypeOne"),
        "RED: the FTS result must not cross graphs: {types:?}"
    );

    // Relation traversal: B reaches its own neighbor and never A's.
    let found = call(
        &fx,
        &owner,
        "find_path",
        json!({ "from": "same-name", "to": "beta-neighbor", "workspaceId": b }),
    );
    assert!(!is_denied(&found), "{found}");
    assert!(
        tool_text(&found).contains("beta-neighbor"),
        "{}",
        tool_text(&found)
    );
    let absent = call(
        &fx,
        &owner,
        "find_path",
        json!({ "from": "same-name", "to": "alpha-neighbor", "workspaceId": b }),
    );
    assert!(
        is_denied(&absent),
        "RED: B must not see A's node through traversal: {absent}"
    );

    // Taxonomy: B's type catalog holds B's types only.
    let types = call(
        &fx,
        &owner,
        "list_entity_types",
        json!({ "workspaceId": b }),
    );
    assert!(!is_denied(&types), "{types}");
    let text = tool_text(&types);
    assert!(text.contains("TypeTwo"), "{text}");
    assert!(
        !text.contains("TypeOne"),
        "RED: the type catalog must not cross graphs: {text}"
    );

    // Vector cache: refresh and stats report B's own file's entity graph.
    // B holds two entities and one relation; the legacy file holds five
    // entities and two relations today, which is the red difference.
    let refreshed = call(
        &fx,
        &owner,
        "vector_refresh_graph_cache",
        json!({ "workspaceId": b }),
    );
    assert!(!is_denied(&refreshed), "{refreshed}");
    let text = tool_text(&refreshed);
    assert!(
        text.contains("\"nodes\":2"),
        "RED: B's cache must hold two nodes: {text}"
    );
    assert!(
        text.contains("\"edges\":1"),
        "B's cache must hold one edge: {text}"
    );
    let stats = call(
        &fx,
        &owner,
        "vector_store_stats",
        json!({ "workspaceId": b }),
    );
    assert!(!is_denied(&stats), "{stats}");
    let text = tool_text(&stats);
    assert!(
        text.contains("\"petgraphNodes\":2"),
        "RED: B's stats must count B's nodes: {text}"
    );

    // Vector search is exercised through the same selector: it serves the
    // selected store, and a never-indexed workspace answers empty, not a
    // cross-graph result.
    let search = call(
        &fx,
        &owner,
        "vector_search_entities",
        json!({ "embedding": [1.0, 0.0], "topK": 10, "workspaceId": b }),
    );
    assert!(!is_denied(&search), "{search}");
    assert_eq!(tool_json(&search)["count"], 0, "{}", tool_json(&search));
}

/// A batch may carry different explicit workspace IDs; each call resolves
/// one workspace and no call joins rows from another. RED: the selector is
/// absent, so both reads hit the legacy graph.
#[test]
fn a_batch_with_two_explicit_workspace_ids_reads_both_without_cross_graph_data() {
    let fx = fixture();
    let a = create_graph(&fx, "One", Visibility::Private);
    let b = create_graph(&fx, "Two", Visibility::Private);
    let owner = local_principal();
    call(
        &fx,
        &owner,
        "create_entities",
        json!({ "workspaceId": a, "entities": [entity("same-name", "TypeOne")] }),
    );
    call(
        &fx,
        &owner,
        "create_entities",
        json!({ "workspaceId": b, "entities": [entity("same-name", "TypeTwo")] }),
    );

    let responses = batch(
        &fx,
        &owner,
        &[
            (
                "get_entity",
                json!({ "workspaceId": a, "name": "same-name" }),
            ),
            (
                "get_entity",
                json!({ "workspaceId": b, "name": "same-name" }),
            ),
        ],
    );
    let items = responses.as_array().expect("a batch answers with an array");
    assert_eq!(items.len(), 2, "{responses}");
    assert_eq!(tool_json(&items[0])["entityType"], "TypeOne");
    assert_eq!(
        tool_json(&items[1])["entityType"],
        "TypeTwo",
        "RED: the second read must select B, not return A's row: {responses}"
    );
}

/// A batch containing a denied call executes none of the denied mutation:
/// the refusal happens per call, and what was refused never lands in the
/// selected workspace. RED: no workspace ACL, so the reader's write applies.
#[test]
fn a_batch_with_a_denied_mutation_executes_none_of_it() {
    let fx = fixture();
    let b = create_graph(&fx, "Gated", Visibility::Private);
    let (reader_id, reader) = machine(&fx, "reader", &["graph-read", "graph-write"]);
    fx.registry
        .grant("machine:local", &b, &reader_id, "reader")
        .unwrap();
    let owner = local_principal();

    let smuggled = json!({ "workspaceId": b, "entities": [entity("smuggled", "Thing")] });
    let responses = batch(
        &fx,
        &reader,
        &[
            ("create_entities", smuggled),
            (
                "get_entity",
                json!({ "workspaceId": b, "name": "smuggled" }),
            ),
        ],
    );
    let items = responses.as_array().expect("a batch answers with an array");
    assert!(
        is_denied(&items[0]),
        "RED: the reader's mutation must be refused in the batch: {responses}"
    );

    // The decisive check: the refused entity must not exist in B afterwards.
    let probe = call(
        &fx,
        &owner,
        "get_entity",
        json!({ "workspaceId": b, "name": "smuggled" }),
    );
    assert!(
        is_denied(&probe),
        "RED: the denied mutation must never land in B: {probe}"
    );
}

/// Machine-account tools accept only an admin human or trusted local stdio.
/// Neither the static bearer nor an issued machine may create, list or revoke
/// accounts, even with `graph-write` in its scopes. RED: the tools are
/// absent, so the local (`machine:local`) calls fail while the denials below
/// are trivially green; the local controls are what turn green with Task 3.
#[test]
fn machine_admin_tools_reject_static_and_issued_machines() {
    let fx = fixture();
    let local = local_principal();
    let static_caller = bearer_principal(&[ToolCategory::GraphRead, ToolCategory::GraphWrite]);
    let (issued_id, issued) = machine(&fx, "issued", &["graph-read", "graph-write"]);

    let created = call(
        &fx,
        &local,
        "create_machine_account",
        json!({ "name": "ops", "scopes": ["graph-read", "graph-write"] }),
    );
    assert!(
        !is_denied(&created),
        "RED: local stdio must be able to create machine accounts: {created}"
    );
    let body = tool_json(&created);
    let ops_id = body["principalId"]
        .as_str()
        .expect("principalId")
        .to_owned();
    assert!(
        body["token"].as_str().is_some(),
        "the credential appears exactly once"
    );

    for (label, caller) in [("static", &static_caller), ("issued", &issued)] {
        let denied = call(
            &fx,
            caller,
            "create_machine_account",
            json!({ "name": "sneaky", "scopes": ["graph-read"] }),
        );
        assert!(
            is_denied(&denied),
            "{label} machine must not create accounts: {denied}"
        );
        let denied = call(&fx, caller, "list_machine_accounts", json!({}));
        assert!(
            is_denied(&denied),
            "{label} machine must not list accounts: {denied}"
        );
        let denied = call(
            &fx,
            caller,
            "revoke_machine_account",
            json!({ "principalId": ops_id }),
        );
        assert!(
            is_denied(&denied),
            "{label} machine must not revoke accounts: {denied}"
        );
    }

    let listed = call(&fx, &local, "list_machine_accounts", json!({}));
    assert!(
        !is_denied(&listed),
        "RED: local stdio must be able to list machine accounts: {listed}"
    );
    let listed_json = tool_json(&listed);
    let accounts = listed_json["accounts"].as_array().expect("accounts");
    assert!(
        accounts.iter().any(|a| a["principalId"] == ops_id.as_str()),
        "{:?}",
        accounts
    );

    let revoked = call(
        &fx,
        &local,
        "revoke_machine_account",
        json!({ "principalId": issued_id }),
    );
    assert!(
        !is_denied(&revoked),
        "RED: local stdio must be able to revoke machine accounts: {revoked}"
    );
    assert_eq!(tool_json(&revoked)["revoked"].as_bool(), Some(true));
}

/// Webhook subscription creation needs workspace ownership in addition to
/// the `graph-write` scope. RED: the selector is absent, so the handler
/// stores the row without any ownership check and the writer's call applies.
#[cfg(feature = "webhooks")]
#[test]
fn webhook_subscription_creation_requires_workspace_ownership() {
    let fx = fixture();
    let b = create_graph(&fx, "Hooked", Visibility::Private);
    let (writer_id, writer) = machine(&fx, "writer", &["graph-read", "graph-write"]);
    fx.registry
        .grant("machine:local", &b, &writer_id, "writer")
        .unwrap();
    let owner = local_principal();

    let sub_args = json!({
        "workspaceId": b,
        "endpoint": "https://hooks.example.test/receive",
        "consumerOrigin": "ws-test",
        "secretRef": SECRET_REF,
    });
    let denied = call(&fx, &writer, "webhook_add_subscription", sub_args);
    assert!(
        is_denied(&denied),
        "RED: a writer must not register a subscription, but it did: {denied}"
    );

    let added = call(
        &fx,
        &owner,
        "webhook_add_subscription",
        json!({
            "workspaceId": b,
            "endpoint": "https://hooks.example.test/receive",
            "consumerOrigin": "ws-test",
            "secretRef": SECRET_REF,
        }),
    );
    assert!(
        !is_denied(&added),
        "the owner must be able to register a subscription: {added}"
    );
    assert!(
        tool_json(&added)["subscriptionId"].as_str().is_some(),
        "{}",
        tool_json(&added)
    );
}
