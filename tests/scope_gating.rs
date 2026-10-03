//! Per-principal tool gating: a caller may only reach the tools its scopes name.

use mcpmem::authz::{Principal, allows_tool, bearer_principal, local_principal, missing_scope};
use mcpmem::config::Config;
use mcpmem::server::{HttpOutcome, MCPServer, dispatch_http_body};
use mcpmem::tools::ToolCategory;
use mcpmem::workspace::{WorkspaceHandles, WorkspaceRegistry};
use serde_json::Value;
use std::sync::Arc;

/// A graph whose `graph-read` and `graph-write` categories are both enabled,
/// together with the registry and handle cache of the same server (workspace
/// selection resolves through them). Those flags are process-wide, so this
/// goes through the same entry point `src/main.rs` uses rather than setting
/// the atomics directly.
///
/// The fixture also configures the static bearer as a real account: a
/// registered `machine:static` with a writer grant on the legacy workspace
/// and that workspace saved as its default. That models a deployment where
/// the token holder is a first-class identity; the scope-gating tests below
/// exercise scope refusal, not workspace selection, so their write controls
/// must not fail with a selection error.
struct TestServer {
    registry: Arc<WorkspaceRegistry>,
    handles: Arc<WorkspaceHandles>,
}

fn test_graph(dir: &tempfile::TempDir) -> TestServer {
    let config = Config {
        memory_file_path: dir.path().join("memory.db").to_string_lossy().into_owned(),
        legacy_owner_id: Some("machine:local".into()),
        auth_token: Some("scope-test-bearer".into()),
        enabled_categories: vec![ToolCategory::GraphRead, ToolCategory::GraphWrite],
        ..Config::default()
    };
    let server = MCPServer::new_kg(config).expect("test server builds");
    let registry = server.workspace_registry();
    let legacy = registry
        .all_paths()
        .expect("registered paths")
        .into_iter()
        .find(|(_, path)| path.ends_with("memory.db"))
        .expect("the legacy workspace")
        .0;
    registry
        .grant("machine:local", &legacy, "machine:static", "writer")
        .expect("static bearer is a registered identity");
    registry
        .set_default("machine:static", &legacy)
        .expect("static bearer default saves");
    TestServer {
        registry: server.workspace_registry(),
        handles: server.workspace_handles(),
    }
}

/// Dispatch a body against the test server, keeping the workspace context.
fn dispatch(s: &TestServer, principal: &Principal, body: &str) -> Result<HttpOutcome, String> {
    dispatch_http_body(body, principal, &s.registry, &s.handles)
}

fn body_of(outcome: HttpOutcome) -> Value {
    match outcome {
        HttpOutcome::Body(v) => v,
        other => panic!("expected a body, got {other:?}"),
    }
}

/// Workspace-management tool names from the approved spec (§"MCP contract").
/// They land in `src/tools.rs` with Task 3; until then the registry does not
/// know them, which is exactly what the red assertions below check: `scope_of`
/// must answer for each one before `allows_tool` may.
const MANAGEMENT_TOOL_NAMES: &[&str] = &[
    "create_workspace",
    "list_workspaces",
    "get_workspace",
    "set_workspace_visibility",
    "list_workspace_grants",
    "grant_workspace_access",
    "revoke_workspace_access",
    "set_default_workspace",
    "create_machine_account",
    "list_machine_accounts",
    "revoke_machine_account",
];

/// The scope each Task-3 name must carry, pinned from the spec: every read is
/// `graph-read`, every mutation is `graph-write`, and the webhook tools keep
/// the `graph-write` category `src/tools.rs` already gives them.
fn expected_scope_of(name: &str) -> Option<&'static str> {
    match name {
        "webhook_add_subscription" | "webhook_delete_subscription" => Some("graph-write"),
        "list_workspaces" | "get_workspace" | "list_workspace_grants" | "set_default_workspace" => {
            Some("graph-read")
        }
        "create_workspace"
        | "set_workspace_visibility"
        | "grant_workspace_access"
        | "revoke_workspace_access"
        | "create_machine_account"
        | "list_machine_accounts"
        | "revoke_machine_account" => Some("graph-write"),
        _ => None,
    }
}

/// Every name the registry knows: knowledge-graph, vector, code and webhook
/// tools, plus the workspace-management names Task 3 adds.
fn every_tool_name() -> Vec<&'static str> {
    mcpmem::tools::ALL_TOOLS
        .iter()
        .map(|t| t.name)
        .chain(mcpmem::tools::VECTOR_TOOL_NAMES.iter().copied())
        .chain(mcpmem::tools::CODE_TOOL_NAMES.iter().copied())
        .chain(mcpmem::tools::WEBHOOK_TOOL_NAMES.iter().copied())
        .chain(MANAGEMENT_TOOL_NAMES.iter().copied())
        .chain(mcpmem::tools::ATTACHMENT_TOOL_NAMES.iter().copied())
        .collect()
}

#[test]
fn attachment_tools_require_their_own_scope_not_graph_write() {
    use mcpmem::tools::{ATTACHMENT_TOOL_NAMES, category_of, scope_of};

    const EXPECTED: [&str; 9] = [
        "begin_attachment_upload",
        "append_attachment_chunk",
        "finish_attachment_upload",
        "cancel_attachment_upload",
        "list_attachments",
        "get_attachment",
        "read_attachment_chunk",
        "get_attachment_page",
        "delete_attachment",
    ];
    assert_eq!(ATTACHMENT_TOOL_NAMES, EXPECTED.as_slice());
    let writer = bearer_principal(&[ToolCategory::GraphWrite]);
    let attached = bearer_principal(&[ToolCategory::Attachments]);
    for name in EXPECTED {
        assert_eq!(category_of(name), Some(ToolCategory::Attachments), "{name}");
        assert_eq!(scope_of(name), Some("attachments"), "{name}");
        assert_eq!(missing_scope(&writer, name), Some("attachments"), "{name}");
        assert!(!allows_tool(&writer, name), "{name}");
        assert!(allows_tool(&attached, name), "{name}");
    }
    assert!(allows_tool(&writer, "create_entities"));
    assert_eq!(
        missing_scope(&attached, "create_entities"),
        Some("graph-write")
    );
}

#[test]
fn a_read_only_principal_may_not_call_a_write_tool() {
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    assert!(allows_tool(&p, "read_graph"));
    assert!(!allows_tool(&p, "delete_entities"));
}

/// One helper owns the scope decision; `allows_tool` only reports it. The two
/// must never disagree for a known tool, under any scope set — not only the
/// empty one and the full one. They keep exactly one deliberate asymmetry: an
/// unknown tool is not a scope failure, yet it is never allowed.
#[test]
fn missing_scope_and_allows_tool_agree_on_every_known_tool() {
    let mut principals = vec![bearer_principal(&[]), local_principal()];
    principals.extend(
        ToolCategory::ALL
            .iter()
            .map(|c| bearer_principal(std::slice::from_ref(c))),
    );

    for p in &principals {
        for name in every_tool_name() {
            let scope = mcpmem::tools::scope_of(name).expect("known tool has a scope");
            // The Task-3 names keep the category the spec pins; before they
            // are registered `scope_of` is None and the expect above fails,
            // which is the red this test exists to show.
            if let Some(expected) = expected_scope_of(name) {
                assert_eq!(scope, expected, "{name} must keep its approved category");
            }
            // Single owner: for a known tool the two are the same decision.
            assert_eq!(
                allows_tool(p, name),
                missing_scope(p, name).is_none(),
                "{name} under {:?}",
                p.scopes
            );
            // And that decision is the obvious one.
            let holds = p.scopes.contains(scope);
            assert_eq!(
                missing_scope(p, name),
                (!holds).then_some(scope),
                "{name} under {:?}",
                p.scopes
            );
        }
        assert_eq!(missing_scope(p, "no_such_tool"), None, "{:?}", p.scopes);
        assert!(!allows_tool(p, "no_such_tool"), "{:?}", p.scopes);
    }
}

/// A `tools/call` with no id is a notification: it never executes, so it can
/// never be a scope failure, and it must not refuse the batch around it. An
/// explicit `"id":null` deserializes to `None` too, so it is one as well.
#[test]
fn a_denied_notification_does_not_refuse_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let s = test_graph(&dir);
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    let body = r#"[
        {"jsonrpc":"2.0","method":"tools/call",
         "params":{"name":"delete_entities","arguments":{"entityNames":["a"]}}},
        {"jsonrpc":"2.0","id":null,"method":"tools/call",
         "params":{"name":"upsert_entities","arguments":{"entities":[]}}},
        {"jsonrpc":"2.0","id":2,"method":"tools/list"}
    ]"#;
    let v = body_of(dispatch(&s, &p, body).unwrap());
    let items = v.as_array().expect("a batch answers with an array");
    assert_eq!(items.len(), 1, "only the request is answered: {v}");
    assert_eq!(items[0]["id"], 2, "{v}");
}

#[test]
fn a_local_principal_may_call_every_known_tool() {
    let p = local_principal();
    // RED until Task 3: the management names are unknown to the registry, so
    // `allows_tool` answers false for them and this loop fails.
    for name in every_tool_name() {
        assert!(allows_tool(&p, name), "{name}");
    }
}

#[test]
fn an_unknown_tool_is_never_allowed() {
    assert!(!allows_tool(&local_principal(), "no_such_tool"));
}

#[test]
fn dispatch_denies_a_write_tool_for_a_read_only_principal() {
    let dir = tempfile::tempdir().unwrap();
    let s = test_graph(&dir);
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"delete_entities","arguments":{"entityNames":["a"]}}}"#;
    match dispatch(&s, &p, body).unwrap() {
        HttpOutcome::InsufficientScope(scopes) => assert_eq!(scopes, vec!["graph-write"]),
        other => panic!("expected a scope refusal, got {other:?}"),
    }
}

#[test]
fn dispatch_allows_a_write_tool_for_a_full_scope_principal() {
    let dir = tempfile::tempdir().unwrap();
    let s = test_graph(&dir);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"create_entities","arguments":{"entities":[
            {"name":"alpha","entityType":"thing","observations":[]}]}}}"#;
    let v = body_of(dispatch(&s, &local_principal(), body).unwrap());
    assert!(v["error"].is_null(), "full scope must not be refused: {v}");
    assert_ne!(v["result"]["isError"], Value::Bool(true), "{v}");
}

/// A denied call anywhere in a batch refuses the whole batch, and the calls the
/// principal *could* make do not run either. The write here is inside the
/// principal's scopes, so only the pre-dispatch screen can keep it unapplied.
#[test]
fn a_batch_with_one_denied_call_applies_none_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let s = test_graph(&dir);
    let p = bearer_principal(&[ToolCategory::GraphRead, ToolCategory::GraphWrite]);
    let body = r#"[
        {"jsonrpc":"2.0","id":1,"method":"tools/call",
         "params":{"name":"create_entities","arguments":{"entities":[
            {"name":"beta","entityType":"thing","observations":[]}]}}},
        {"jsonrpc":"2.0","id":2,"method":"tools/call",
         "params":{"name":"hybrid_search","arguments":{"queryText":"x"}}}
    ]"#;
    match dispatch(&s, &p, body).unwrap() {
        HttpOutcome::InsufficientScope(scopes) => assert_eq!(scopes, vec!["vectors"]),
        other => panic!("expected a scope refusal, got {other:?}"),
    }

    let check = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call",
        "params":{"name":"read_graph","arguments":{}}}"#;
    let v = body_of(dispatch(&s, &local_principal(), check).unwrap());
    assert!(
        !v.to_string().contains("beta"),
        "the refused batch must not have created 'beta': {v}"
    );

    // Control: the same write on its own does create it, so the assertion
    // above proves the refusal and not a malformed request.
    let allowed = r#"[{"jsonrpc":"2.0","id":1,"method":"tools/call",
         "params":{"name":"create_entities","arguments":{"entities":[
            {"name":"beta","entityType":"thing","observations":[]}]}}}]"#;
    body_of(dispatch(&s, &p, allowed).unwrap());
    let v = body_of(dispatch(&s, &local_principal(), check).unwrap());
    assert!(v.to_string().contains("beta"), "control failed: {v}");
}

#[test]
fn denied_attachment_scope_refuses_a_batch_before_its_graph_write() {
    let dir = tempfile::tempdir().unwrap();
    let s = test_graph(&dir);
    let writer = bearer_principal(&[ToolCategory::GraphRead, ToolCategory::GraphWrite]);
    let body = r#"[
        {"jsonrpc":"2.0","id":1,"method":"tools/call",
         "params":{"name":"create_entities","arguments":{"entities":[
            {"name":"blocked-by-attachment","entityType":"thing","observations":[]}]}}},
        {"jsonrpc":"2.0","id":2,"method":"tools/call",
         "params":{"name":"begin_attachment_upload","arguments":{}}}
    ]"#;
    match dispatch(&s, &writer, body).expect("dispatch") {
        HttpOutcome::InsufficientScope(scopes) => assert_eq!(scopes, vec!["attachments"]),
        other => panic!("a denied attachment call must refuse the whole batch: {other:?}"),
    }
    let check = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"read_graph","arguments":{}}}"#;
    let absent = body_of(dispatch(&s, &local_principal(), check).unwrap());
    assert!(
        !absent.to_string().contains("blocked-by-attachment"),
        "no graph write may run before the scope decision: {absent}"
    );

    let alone = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call",
         "params":{"name":"create_entities","arguments":{"entities":[
            {"name":"blocked-by-attachment","entityType":"thing","observations":[]}]}}}"#;
    let result = body_of(dispatch(&s, &writer, alone).unwrap());
    assert!(
        result["error"].is_null(),
        "the graph write is valid: {result}"
    );
    let present = body_of(dispatch(&s, &local_principal(), check).unwrap());
    assert!(
        present.to_string().contains("blocked-by-attachment"),
        "the allowed graph write must still work: {present}"
    );
}

/// The refusal names every missing scope once, in order.
#[test]
fn a_denied_batch_names_every_missing_scope_once() {
    let dir = tempfile::tempdir().unwrap();
    let s = test_graph(&dir);
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    let body = r#"[
        {"jsonrpc":"2.0","id":1,"method":"tools/call",
         "params":{"name":"hybrid_search","arguments":{"queryText":"x"}}},
        {"jsonrpc":"2.0","id":2,"method":"tools/call",
         "params":{"name":"upsert_entities","arguments":{"entities":[]}}},
        {"jsonrpc":"2.0","id":3,"method":"tools/call",
         "params":{"name":"delete_entities","arguments":{"entityNames":["a"]}}},
        {"jsonrpc":"2.0","id":4,"method":"tools/call",
         "params":{"name":"read_graph","arguments":{}}}
    ]"#;
    match dispatch(&s, &p, body).unwrap() {
        HttpOutcome::InsufficientScope(scopes) => {
            assert_eq!(scopes, vec!["graph-write", "vectors"]);
        }
        other => panic!("expected a scope refusal, got {other:?}"),
    }
}

/// The tool names `tools/list` advertises to `principal`.
fn listed_tool_names(s: &TestServer, principal: &Principal) -> Vec<String> {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    body_of(dispatch(s, principal, body).unwrap())["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name").to_owned())
        .collect()
}

#[test]
fn tools_list_hides_what_the_principal_may_not_call() {
    let dir = tempfile::tempdir().unwrap();
    let s = test_graph(&dir);
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let v = body_of(dispatch(&s, &p, body).unwrap());
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"read_graph"));
    assert!(!names.contains(&"delete_entities"));
}

/// The Task-3 management tools appear in `tools/list` by their category: a
/// `graph-read` caller sees the read side, a `graph-write` caller sees the
/// mutations, and the machine-account tools need an admin human or local
/// stdio even with `graph-write`. RED until Task 3 registers the names, so
/// the "must see" assertions fail for exactly that reason.
#[test]
fn management_tools_follow_their_category_scope() {
    let dir = tempfile::tempdir().unwrap();
    let s = test_graph(&dir);

    let reader = bearer_principal(&[ToolCategory::GraphRead]);
    let names = listed_tool_names(&s, &reader);
    assert!(
        names.iter().any(|n| n == "list_workspaces"),
        "a graph-read caller must see list_workspaces: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "create_workspace"),
        "a graph-read caller must not see create_workspace: {names:?}"
    );

    let writer = bearer_principal(&[ToolCategory::GraphRead, ToolCategory::GraphWrite]);
    let names = listed_tool_names(&s, &writer);
    assert!(
        names.iter().any(|n| n == "create_workspace"),
        "a graph-write caller must see create_workspace: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "create_machine_account"),
        "a non-admin machine must not see the machine tools: {names:?}"
    );

    let names = listed_tool_names(&s, &local_principal());
    assert!(
        names.iter().any(|n| n == "create_machine_account"),
        "local stdio must see the machine tools: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "revoke_machine_account"),
        "local stdio must see revoke_machine_account: {names:?}"
    );
}

/// Gating must not swallow the unknown-tool case: an unknown name is still a
/// `Method not found` protocol error, exactly as before this change.
#[test]
fn an_unknown_tool_is_still_method_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let s = test_graph(&dir);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"no_such_tool","arguments":{}}}"#;
    let v = body_of(dispatch(&s, &local_principal(), body).unwrap());
    assert_eq!(v["error"]["code"], -32601, "{v}");
}

/// A notification-only body is still `202 Accepted`, gating or not.
#[test]
fn a_notification_only_body_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let s = test_graph(&dir);
    let body = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    assert!(matches!(
        dispatch(&s, &p, body).unwrap(),
        HttpOutcome::Accepted
    ));
}
