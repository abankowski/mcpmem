#![cfg(feature = "webhooks")]
//! `webhook_add_subscription` / `webhook_delete_subscription`: the MCP tool
//! surface over the shared subscription table.
//!
//! All tests share one server. The subscription tools record their database
//! path in a process-wide cell the first time a server is built (see
//! `mcpmem::actions::webhooks::init`), so a second server built later in this
//! process would silently keep pointing at the first one's file. One shared
//! fixture keeps that true on purpose instead of by accident.

use mcpmem::authz::{bearer_principal, local_principal};
use mcpmem::config::Config;
use mcpmem::kg::GraphHandle;
use mcpmem::server::{HttpOutcome, MCPServer, dispatch_http_body};
use mcpmem::tools::ToolCategory;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};

/// A server whose `graph-read` and `graph-write` categories are both
/// enabled. A per-test principal narrows what the caller may reach; the
/// category flags stay fixed for every test in this process.
struct Fixture {
    _dir: tempfile::TempDir,
    kg: Arc<GraphHandle>,
}

static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        memory_file_path: dir.path().join("memory.db").to_string_lossy().into_owned(),
        enabled_categories: vec![ToolCategory::GraphRead, ToolCategory::GraphWrite],
        ..Config::default()
    };
    let server = MCPServer::new_kg(config).expect("test server builds");
    let kg = server.graph();
    Fixture { _dir: dir, kg }
});

fn body_of(outcome: HttpOutcome) -> Value {
    match outcome {
        HttpOutcome::Body(v) => v,
        other => panic!("expected a body, got {other:?}"),
    }
}

/// Call `name` as the local principal, which holds every scope. The tests
/// that care about scope build their own principal instead.
fn call(name: &str, arguments: &Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
    .to_string();
    body_of(dispatch_http_body(&body, &FIXTURE.kg, None, &local_principal()).expect("valid JSON"))
}

fn is_error(response: &Value) -> bool {
    response["result"]["isError"].as_bool().unwrap_or(false)
}

/// The `content[0].text` of a tool result, parsed back into JSON when the
/// call succeeded.
fn result_json(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected tool text content, got {response}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("tool text was not JSON: {e}: {text}"))
}

fn listed_tool_names(principal: &mcpmem::authz::Principal) -> Vec<String> {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    body_of(dispatch_http_body(body, &FIXTURE.kg, None, principal).unwrap())["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn adds_a_subscription_for_a_valid_https_endpoint() {
    let response = call(
        "webhook_add_subscription",
        &json!({
            "endpoint": "https://hooks.example.test/receive",
            "consumerOrigin": "test-consumer",
            "secretRef": "vault://webhook/test",
        }),
    );
    assert!(!is_error(&response), "unexpected error: {response}");
    let body = result_json(&response);
    assert!(
        body["subscriptionId"].as_str().is_some(),
        "missing subscriptionId: {body}"
    );
}

#[test]
fn rejects_a_non_https_endpoint() {
    let response = call(
        "webhook_add_subscription",
        &json!({
            "endpoint": "http://hooks.example.test/receive",
            "consumerOrigin": "test-consumer",
            "secretRef": "vault://webhook/test",
        }),
    );
    assert!(is_error(&response), "expected a rejection: {response}");
    let message = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        message.contains("https"),
        "error should mention https: {message}"
    );
}

#[test]
fn deletes_a_subscription_it_just_added() {
    let added = call(
        "webhook_add_subscription",
        &json!({
            "endpoint": "https://hooks.example.test/receive",
            "consumerOrigin": "delete-me",
            "secretRef": "vault://webhook/test",
        }),
    );
    let id = result_json(&added)["subscriptionId"]
        .as_str()
        .expect("subscriptionId")
        .to_owned();

    let deleted = call(
        "webhook_delete_subscription",
        &json!({ "subscriptionId": id }),
    );
    assert!(!is_error(&deleted), "unexpected error: {deleted}");
    assert_eq!(result_json(&deleted)["deleted"].as_bool(), Some(true));

    // The row is gone now, so deleting the same id again is not an error —
    // it just reports that nothing was removed.
    let deleted_again = call(
        "webhook_delete_subscription",
        &json!({ "subscriptionId": id }),
    );
    assert!(
        !is_error(&deleted_again),
        "unexpected error: {deleted_again}"
    );
    assert_eq!(
        result_json(&deleted_again)["deleted"].as_bool(),
        Some(false)
    );
}

/// The tools mutate a stored row, so `src/tools.rs` gives them the
/// `graph-write` scope instead of a scope of their own. A caller without it
/// must not see them.
#[test]
fn tools_are_hidden_from_tools_list_without_graph_write_scope() {
    let names = listed_tool_names(&bearer_principal(&[ToolCategory::GraphRead]));
    assert!(
        !names.iter().any(|n| n == "webhook_add_subscription"),
        "must be hidden without graph-write: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "webhook_delete_subscription"),
        "must be hidden without graph-write: {names:?}"
    );

    // Control: a caller who does hold graph-write sees both names. This
    // proves the assertions above hide the tools for scope, not because the
    // manifest never loaded.
    let names = listed_tool_names(&bearer_principal(&[
        ToolCategory::GraphRead,
        ToolCategory::GraphWrite,
    ]));
    assert!(names.iter().any(|n| n == "webhook_add_subscription"));
    assert!(names.iter().any(|n| n == "webhook_delete_subscription"));
}
