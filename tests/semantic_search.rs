//! `semantic_search`: the read side of the embedding worker.
//!
//! The server embeds the query text itself, so the tool needs two things the
//! other vector tools do not: the `indexer` feature at build time, and an
//! embedding provider at run time. These tests cover what the server does when
//! one of them is missing. No test here calls a live embedding service, and no
//! test installs a provider — `indexer_provider` holds a process-wide cell, so
//! one installed provider would leak into every other test in this binary.

use mcpmem::config::Config;
use mcpmem::kg::GraphHandle;
use mcpmem::server::{HttpOutcome, MCPServer, dispatch_http_body};
use mcpmem::tools::{SEMANTIC_SEARCH, ToolCategory, category_of};
use mcpmem::vector_store::{VectorConfig, VectorStore};
use serde_json::Value;
use std::sync::Arc;

const DIMS: u32 = 8;

/// A server with the vector subsystem on. The category flags are process-wide,
/// so this goes through the same constructor `src/main.rs` uses.
fn vector_server(dir: &tempfile::TempDir) -> (Arc<GraphHandle>, Arc<VectorStore>) {
    let config = Config {
        memory_file_path: dir.path().join("memory.db").to_string_lossy().into_owned(),
        enabled_categories: vec![
            ToolCategory::GraphRead,
            ToolCategory::GraphWrite,
            ToolCategory::Vectors,
        ],
        vectors_enabled: true,
        ..Config::default()
    };
    let server = MCPServer::new(config, VectorConfig::new(DIMS)).expect("test server builds");
    let vs = server.vector_store().expect("vectors are enabled");
    (server.graph(), vs)
}

fn body_of(outcome: HttpOutcome) -> Value {
    match outcome {
        HttpOutcome::Body(v) => v,
        other => panic!("expected a body, got {other:?}"),
    }
}

/// The `content[0].text` of a tool result, whether it is an error or not.
fn result_text(value: &Value) -> String {
    value["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected tool text content, got {value}"))
        .to_owned()
}

fn call(kg: &GraphHandle, vs: &VectorStore, arguments: &Value) -> Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": SEMANTIC_SEARCH, "arguments": arguments },
    })
    .to_string();
    body_of(
        dispatch_http_body(&body, kg, Some(vs), &mcpmem::authz::local_principal())
            .expect("the body is valid JSON"),
    )
}

fn listed_tool_names(kg: &GraphHandle, vs: &VectorStore) -> Vec<String> {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let v = body_of(
        dispatch_http_body(body, kg, Some(vs), &mcpmem::authz::local_principal())
            .expect("the body is valid JSON"),
    );
    v["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array")
        .iter()
        .map(|t| {
            t["name"]
                .as_str()
                .expect("every tool has a name")
                .to_owned()
        })
        .collect()
}

/// The manifest the server compiles in.
fn manifest() -> Vec<Value> {
    serde_json::from_str(include_str!("../vector_tools.json")).expect("the manifest is valid JSON")
}

fn manifest_entry(name: &str) -> Value {
    manifest()
        .into_iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("the manifest declares no tool named {name}"))
}

/// Without this, the hiding test below would pass for the wrong reason: a
/// manifest that never declared the tool hides it just as well as the provider
/// gate does. This pins the declaration, so the hiding test proves a gate.
#[test]
fn the_manifest_declares_semantic_search() {
    let tool = manifest_entry(SEMANTIC_SEARCH);
    let schema = &tool["inputSchema"];
    assert_eq!(
        schema["required"],
        serde_json::json!(["queryText"]),
        "queryText is the only required argument: {tool}"
    );
    for key in ["queryText", "topK", "entityType", "textWeight", "vecWeight"] {
        assert!(
            schema["properties"][key].is_object(),
            "the schema must declare {key}: {tool}"
        );
    }
    // The clamp is one constant in `vector_actions`, so the advertised cap must
    // be the cap the sibling vector tools advertise.
    assert_eq!(
        schema["properties"]["topK"]["maximum"],
        manifest_entry("vector_search_entities")["inputSchema"]["properties"]["topK"]["maximum"],
        "topK must advertise the same cap as the other vector tools"
    );
    // The description is the only place a client learns that it sends no vector
    // and that the server picks the model.
    let description = tool["description"].as_str().expect("a description");
    for phrase in ["embeds", "serving index profile", "indexer"] {
        assert!(
            description.contains(phrase),
            "the description must mention {phrase:?}: {description}"
        );
    }
}

/// The scope gate reads [`category_of`]. A name it does not know is an unknown
/// tool, not a refused one, so the name must classify on every build — with the
/// `indexer` feature and without it.
#[test]
fn semantic_search_is_always_a_vector_tool() {
    assert_eq!(category_of(SEMANTIC_SEARCH), Some(ToolCategory::Vectors));
}

/// No provider is configured in this process, so the server must not advertise
/// the tool. The other vector tools stay listed, which proves the filter hides
/// this one name and not the whole manifest.
#[test]
fn semantic_search_is_absent_from_tools_list_without_a_provider() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let names = listed_tool_names(&kg, &vs);
    assert!(
        names.iter().any(|n| n == "hybrid_search"),
        "the vector tools must be listed: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == SEMANTIC_SEARCH),
        "semantic_search must be hidden without a provider: {names:?}"
    );
}

/// Hidden from `tools/list` is not enough. The name is still a known vector
/// tool, so a client that calls it anyway must be refused rather than served.
#[test]
fn a_hidden_semantic_search_call_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let v = call(&kg, &vs, &serde_json::json!({ "queryText": "anything" }));
    let text = result_text(&v);
    assert!(v["error"].is_null(), "a scope refusal is not wanted: {v}");
    assert_eq!(
        v["result"]["isError"],
        Value::Bool(true),
        "an unusable tool must refuse the call: {v}"
    );
    // Without the feature the refusal names the build. With the feature and no
    // provider it names the configuration section. Both point at the indexer.
    assert!(
        text.contains("indexer"),
        "the refusal must say what is missing: {text}"
    );
}

/// No store adopts a profile in this test, so the store still serves none. The
/// message must send the operator to the configuration file, not to the client.
#[cfg(feature = "indexer")]
#[test]
fn a_missing_serving_profile_names_the_indexer_section() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let v = call(
        &kg,
        &vs,
        &serde_json::json!({ "queryText": "graph database" }),
    );
    let text = result_text(&v);
    assert_eq!(v["result"]["isError"], Value::Bool(true), "{v}");
    assert!(
        text.contains("[indexer]") && text.contains("no index profile"),
        "the error must name the missing profile and the config section: {text}"
    );
    for word in ["provider", "model", "dimension"] {
        assert!(
            text.contains(word),
            "the error must name what to configure ({word}): {text}"
        );
    }
}

/// Whitespace embeds to a vector that means nothing, and the provider bills for
/// the call. The refusal must come before any of that, so it lands even with no
/// profile and no provider.
#[cfg(feature = "indexer")]
#[test]
fn an_empty_query_text_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    for blank in ["", "   ", "\t\n"] {
        let v = call(&kg, &vs, &serde_json::json!({ "queryText": blank }));
        let text = result_text(&v);
        assert_eq!(v["result"]["isError"], Value::Bool(true), "{blank:?}: {v}");
        assert!(
            text.contains("'queryText'") && text.contains("empty"),
            "the refusal must name the parameter: {text}"
        );
    }
}

/// A missing parameter is a different failure from a blank one, and both must
/// name `queryText`.
#[cfg(feature = "indexer")]
#[test]
fn a_missing_query_text_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (kg, vs) = vector_server(&dir);
    let v = call(&kg, &vs, &serde_json::json!({ "topK": 5 }));
    let text = result_text(&v);
    assert_eq!(v["result"]["isError"], Value::Bool(true), "{v}");
    assert!(text.contains("'queryText'"), "{text}");
}
