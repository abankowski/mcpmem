//! Per-principal tool gating: a caller may only reach the tools its scopes name.

use mcpmem::authz::{allows_tool, bearer_principal, local_principal};
use mcpmem::config::Config;
use mcpmem::kg::GraphHandle;
use mcpmem::server::{HttpOutcome, MCPServer, dispatch_http_body};
use mcpmem::tools::ToolCategory;
use serde_json::Value;
use std::sync::Arc;

/// A graph whose `graph-read` and `graph-write` categories are both enabled.
/// Those flags are process-wide, so this goes through the same entry point
/// `src/main.rs` uses rather than setting the atomics directly.
fn test_graph(dir: &tempfile::TempDir) -> Arc<GraphHandle> {
    let config = Config {
        memory_file_path: dir.path().join("memory.db").to_string_lossy().into_owned(),
        enabled_categories: vec![ToolCategory::GraphRead, ToolCategory::GraphWrite],
        ..Config::default()
    };
    MCPServer::new_kg(config)
        .expect("test server builds")
        .graph()
}

fn body_of(outcome: HttpOutcome) -> Value {
    match outcome {
        HttpOutcome::Body(v) => v,
        other => panic!("expected a body, got {other:?}"),
    }
}

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

#[test]
fn dispatch_denies_a_write_tool_for_a_read_only_principal() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"delete_entities","arguments":{"entityNames":["a"]}}}"#;
    match dispatch_http_body(body, &kg, None, &p).unwrap() {
        HttpOutcome::InsufficientScope(scopes) => assert_eq!(scopes, vec!["graph-write"]),
        other => panic!("expected a scope refusal, got {other:?}"),
    }
}

#[test]
fn dispatch_allows_a_write_tool_for_a_full_scope_principal() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"create_entities","arguments":{"entities":[
            {"name":"alpha","entityType":"thing","observations":[]}]}}}"#;
    let v = body_of(dispatch_http_body(body, &kg, None, &local_principal()).unwrap());
    assert!(v["error"].is_null(), "full scope must not be refused: {v}");
    assert_ne!(v["result"]["isError"], Value::Bool(true), "{v}");
}

/// A denied call anywhere in a batch refuses the whole batch, and the calls the
/// principal *could* make do not run either. The write here is inside the
/// principal's scopes, so only the pre-dispatch screen can keep it unapplied.
#[test]
fn a_batch_with_one_denied_call_applies_none_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);
    let p = bearer_principal(&[ToolCategory::GraphRead, ToolCategory::GraphWrite]);
    let body = r#"[
        {"jsonrpc":"2.0","id":1,"method":"tools/call",
         "params":{"name":"create_entities","arguments":{"entities":[
            {"name":"beta","entityType":"thing","observations":[]}]}}},
        {"jsonrpc":"2.0","id":2,"method":"tools/call",
         "params":{"name":"hybrid_search","arguments":{"queryText":"x"}}}
    ]"#;
    match dispatch_http_body(body, &kg, None, &p).unwrap() {
        HttpOutcome::InsufficientScope(scopes) => assert_eq!(scopes, vec!["vectors"]),
        other => panic!("expected a scope refusal, got {other:?}"),
    }

    let check = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call",
        "params":{"name":"read_graph","arguments":{}}}"#;
    let v = body_of(dispatch_http_body(check, &kg, None, &local_principal()).unwrap());
    assert!(
        !v.to_string().contains("beta"),
        "the refused batch must not have created 'beta': {v}"
    );

    // Control: the same write on its own does create it, so the assertion
    // above proves the refusal and not a malformed request.
    let allowed = r#"[{"jsonrpc":"2.0","id":1,"method":"tools/call",
         "params":{"name":"create_entities","arguments":{"entities":[
            {"name":"beta","entityType":"thing","observations":[]}]}}}]"#;
    body_of(dispatch_http_body(allowed, &kg, None, &p).unwrap());
    let v = body_of(dispatch_http_body(check, &kg, None, &local_principal()).unwrap());
    assert!(v.to_string().contains("beta"), "control failed: {v}");
}

/// The refusal names every missing scope once, in order.
#[test]
fn a_denied_batch_names_every_missing_scope_once() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);
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
    match dispatch_http_body(body, &kg, None, &p).unwrap() {
        HttpOutcome::InsufficientScope(scopes) => {
            assert_eq!(scopes, vec!["graph-write", "vectors"]);
        }
        other => panic!("expected a scope refusal, got {other:?}"),
    }
}

#[test]
fn tools_list_hides_what_the_principal_may_not_call() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let v = body_of(dispatch_http_body(body, &kg, None, &p).unwrap());
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"read_graph"));
    assert!(!names.contains(&"delete_entities"));
}

/// Gating must not swallow the unknown-tool case: an unknown name is still a
/// `Method not found` protocol error, exactly as before this change.
#[test]
fn an_unknown_tool_is_still_method_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"no_such_tool","arguments":{}}}"#;
    let v = body_of(dispatch_http_body(body, &kg, None, &local_principal()).unwrap());
    assert_eq!(v["error"]["code"], -32601, "{v}");
}

/// A notification-only body is still `202 Accepted`, gating or not.
#[test]
fn a_notification_only_body_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let kg = test_graph(&dir);
    let body = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let p = bearer_principal(&[ToolCategory::GraphRead]);
    assert!(matches!(
        dispatch_http_body(body, &kg, None, &p).unwrap(),
        HttpOutcome::Accepted
    ));
}
