//! Attachment tools over the MCP request boundary.

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "extractor")]
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use mcpmem::authz::{Principal, local_principal};
use mcpmem::config::Config;
#[cfg(feature = "extractor")]
use mcpmem::runtime::{ExtractorService, ExtractorWake, RoleService};
#[cfg(feature = "extractor")]
use mcpmem_core::attachments::{AttachmentLimits, AttachmentRepository};
#[cfg(feature = "extractor")]
use mcpmem_core::events::now_us;

use mcpmem::server::{HttpOutcome, MAX_REQUEST_BYTES, MCPServer, dispatch_http_body};
use mcpmem::tools::{ATTACHMENT_TOOL_NAMES, ToolCategory};
use mcpmem::workspace::{Visibility, WorkspaceAccess, WorkspaceHandles, WorkspaceRegistry};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct Fixture {
    _dir: tempfile::TempDir,
    registry: Arc<WorkspaceRegistry>,
    handles: Arc<WorkspaceHandles>,
    workspace_id: String,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_limits(None)
    }

    fn with_limits(budget: Option<i64>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            memory_file_path: dir
                .path()
                .join("memory.sqlite")
                .to_string_lossy()
                .into_owned(),
            legacy_owner_id: Some("machine:local".into()),
            enabled_categories: vec![
                ToolCategory::GraphRead,
                ToolCategory::GraphWrite,
                ToolCategory::Attachments,
            ],
            ..Config::default()
        };
        if let Some(budget) = budget {
            config.attachments.workspace_byte_budget = budget;
        }
        let server = MCPServer::new_kg(config).unwrap();
        let registry = server.workspace_registry();
        let handles = server.workspace_handles();
        let selected = registry
            .resolve("machine:local", None, WorkspaceAccess::Owner)
            .unwrap();
        Self {
            _dir: dir,
            registry,
            handles,
            workspace_id: selected.workspace_id,
            path: selected.graph_path,
        }
    }

    fn request(&self, principal: &Principal, body: &Value) -> HttpOutcome {
        dispatch_http_body(&body.to_string(), principal, &self.registry, &self.handles).unwrap()
    }

    fn call(&self, principal: &Principal, name: &str, arguments: Value) -> Value {
        let mut params = json!({"name": name});
        params["arguments"] = arguments;
        body_of(self.request(
            principal,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": params
            }),
        ))
    }

    fn create(&self, name: &str) {
        let response = self.call(
            &local_principal(),
            "create_entities",
            json!({"workspaceId": self.workspace_id, "entities": [
                {"name": name, "entityType": "document", "observations": []}
            ]}),
        );
        assert!(!is_error(&response), "entity setup: {response}");
    }

    fn machine(&self, name: &str, scopes: &[&str], role: &str) -> Principal {
        let requested: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
        let (id, token) = self.registry.create_machine(name, &requested).unwrap();
        self.registry
            .grant("machine:local", &self.workspace_id, &id, role)
            .unwrap();
        self.registry.authenticate_machine(&token).unwrap().unwrap()
    }

    fn count(&self, table: &str) -> i64 {
        Connection::open(&self.path)
            .unwrap()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }
}

#[cfg(feature = "extractor")]
fn fixture_with_extractor_wake(wake: ExtractorWake) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        memory_file_path: dir
            .path()
            .join("memory.sqlite")
            .to_string_lossy()
            .into_owned(),
        legacy_owner_id: Some("machine:local".into()),
        enabled_categories: vec![
            ToolCategory::GraphRead,
            ToolCategory::GraphWrite,
            ToolCategory::Attachments,
        ],
        ..Config::default()
    };
    let server = MCPServer::new_kg_with_extractor_wake(config, Some(wake)).unwrap();
    let registry = server.workspace_registry();
    let handles = server.workspace_handles();
    let selected = registry
        .resolve("machine:local", None, WorkspaceAccess::Owner)
        .unwrap();
    Fixture {
        _dir: dir,
        registry,
        handles,
        workspace_id: selected.workspace_id,
        path: selected.graph_path,
    }
}

#[cfg(feature = "extractor")]
fn extractor_limits() -> AttachmentLimits {
    let attachments = Config::default().attachments;
    AttachmentLimits {
        max_bytes: attachments.max_bytes,
        workspace_byte_budget: attachments.workspace_byte_budget,
        allow_mime: attachments.allow_mime,
    }
}

#[cfg(feature = "extractor")]
fn attachment_status(fixture: &Fixture, id: i64) -> String {
    Connection::open(&fixture.path)
        .unwrap()
        .query_row("SELECT status FROM attachment WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .unwrap()
}

#[cfg(feature = "extractor")]
fn entity_id(fixture: &Fixture) -> i64 {
    Connection::open(&fixture.path)
        .unwrap()
        .query_row("SELECT id FROM entity WHERE name='doc'", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[cfg(feature = "extractor")]
fn seed_expired_upload(fixture: &Fixture) {
    let conn = Connection::open(&fixture.path).unwrap();
    let digest: [u8; 32] = Sha256::digest(b"").into();
    AttachmentRepository::new(&conn)
        .begin_upload_at(
            "machine:local",
            entity_id(fixture),
            "expired.txt",
            "text/plain",
            0,
            &digest,
            0,
            0,
            &extractor_limits(),
        )
        .unwrap();
}

#[cfg(feature = "extractor")]
fn queue_attachment_without_wake(fixture: &Fixture) -> i64 {
    let conn = Connection::open(&fixture.path).unwrap();
    let content = b"queued";
    let digest: [u8; 32] = Sha256::digest(content).into();
    AttachmentRepository::new(&conn)
        .store_reader(
            entity_id(fixture),
            "queued.txt",
            "text/plain",
            &mut std::io::Cursor::new(content.as_slice()),
            content.len() as i64,
            &digest,
            &extractor_limits(),
            now_us(),
        )
        .unwrap()
}

#[cfg(feature = "extractor")]
async fn wait_for_idle_extractor(fixture: &Fixture) {
    tokio::time::timeout(Duration::from_millis(500), async {
        while fixture.count("attachment_upload") != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the extractor must finish its expired-upload turn");
}

#[cfg(feature = "extractor")]
async fn wait_for_ready_attachment(fixture: &Fixture, id: i64) {
    tokio::time::timeout(Duration::from_millis(500), async {
        while attachment_status(fixture, id) != "ready" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the extractor must receive the local wake");
}

fn body_of(outcome: HttpOutcome) -> Value {
    match outcome {
        HttpOutcome::Body(value) => value,
        other => panic!("expected an MCP body, got {other:?}"),
    }
}

fn is_error(response: &Value) -> bool {
    response["result"]["isError"].as_bool().unwrap_or(false)
}

fn result(response: &Value) -> Value {
    assert!(!is_error(response), "unexpected tool failure: {response}");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected MCP text content: {response}"));
    serde_json::from_str(text).unwrap()
}

fn assert_error(response: &Value, expected: &str) {
    assert!(is_error(response), "expected a tool failure: {response}");
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(expected),
        "the failure must name {expected}: {response}"
    );
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn begin(fixture: &Fixture, principal: &Principal, filename: &str, content: &[u8]) -> String {
    let response = fixture.call(
        principal,
        "begin_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "entityName": "doc",
            "filename": filename, "mime": "text/plain", "expectedBytes": content.len(),
            "sha256": digest(content)}),
    );
    let payload = result(&response);
    assert_eq!(payload["nextIndex"], 0);
    payload["uploadId"].as_str().unwrap().to_owned()
}

#[cfg(feature = "extractor")]
#[tokio::test]
async fn completed_mcp_upload_wakes_the_injected_extractor() {
    let wake = ExtractorWake::new();
    let fixture = fixture_with_extractor_wake(wake.clone());
    fixture.create("doc");
    seed_expired_upload(&fixture);

    let service = ExtractorService::new_with_wake(fixture.path.clone(), None, wake);
    let role = tokio::spawn(service.run());
    wait_for_idle_extractor(&fixture).await;

    let principal = local_principal();
    let content = b"wake the extractor";
    let upload = begin(&fixture, &principal, "wake.txt", content);
    result(&fixture.call(
        &principal,
        "append_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
            "index": 0, "content": STANDARD.encode(content)}),
    ));
    let attachment = result(&fixture.call(
        &principal,
        "finish_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload}),
    ))["attachmentId"]
        .as_i64()
        .unwrap();

    wait_for_ready_attachment(&fixture, attachment).await;
    role.abort();
}

#[cfg(feature = "extractor")]
#[tokio::test]
async fn failed_cancelled_and_incomplete_mcp_uploads_do_not_wake_the_extractor() {
    let wake = ExtractorWake::new();
    let fixture = fixture_with_extractor_wake(wake.clone());
    fixture.create("doc");
    seed_expired_upload(&fixture);

    let service = ExtractorService::new_with_wake(fixture.path.clone(), None, wake);
    let role = tokio::spawn(service.run());
    wait_for_idle_extractor(&fixture).await;

    let queued = queue_attachment_without_wake(&fixture);
    let principal = local_principal();
    let incomplete = begin(&fixture, &principal, "incomplete.txt", b"missing");
    assert_error(
        &fixture.call(
            &principal,
            "finish_attachment_upload",
            json!({"workspaceId": fixture.workspace_id, "uploadId": incomplete}),
        ),
        "incomplete",
    );
    let cancelled = begin(&fixture, &principal, "cancelled.txt", b"cancelled");
    result(&fixture.call(
        &principal,
        "cancel_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "uploadId": cancelled}),
    ));

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(attachment_status(&fixture, queued), "uploaded");
    role.abort();
}

#[test]
fn fifty_mebibyte_calls_are_byte_exact_and_pending_until_extracted() {
    let fixture = Fixture::new();
    fixture.create("doc");
    let principal = local_principal();
    let bytes: Vec<u8> = (0..52_428_800)
        .map(|offset| (offset as u8).wrapping_mul(41).wrapping_add(13))
        .collect();
    let expected_sha256 = digest(&bytes);
    let upload = begin(&fixture, &principal, "full.txt", &bytes);

    for (index, chunk) in bytes.chunks(1_048_576).enumerate() {
        let response = fixture.call(
            &principal,
            "append_attachment_chunk",
            json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
                "index": index, "content": STANDARD.encode(chunk)}),
        );
        let payload = result(&response);
        assert_eq!(payload["nextIndex"], index + 1);
        assert_eq!(payload["receivedBytes"], (index + 1) * 1_048_576);
    }

    let finish = || {
        result(&fixture.call(
            &principal,
            "finish_attachment_upload",
            json!({"workspaceId": fixture.workspace_id, "uploadId": upload}),
        ))
    };
    let completed = finish();
    let id = completed["attachmentId"].as_i64().unwrap();
    assert_eq!(completed["status"], "uploaded");
    assert_eq!(finish(), completed, "finish is idempotent");
    let metadata = result(&fixture.call(
        &principal,
        "get_attachment",
        json!({"workspaceId": fixture.workspace_id, "attachmentId": id}),
    ));
    assert_eq!(metadata["entityName"], "doc");
    assert_eq!(metadata["filename"], "full.txt");
    assert_eq!(metadata["sizeBytes"], bytes.len());
    assert_eq!(metadata["pageCount"], 0);
    assert_eq!(metadata["status"], "uploaded");
    assert!(
        metadata.get("content").is_none(),
        "metadata must not include raw bytes"
    );
    let list = result(&fixture.call(
        &principal,
        "list_attachments",
        json!({"workspaceId": fixture.workspace_id, "entityName": "doc"}),
    ));
    assert_eq!(list["attachments"][0]["attachmentId"], id);

    let mut downloaded = Vec::with_capacity(bytes.len());
    for offset in (0..bytes.len()).step_by(1_048_576) {
        let chunk = result(&fixture.call(
            &principal,
            "read_attachment_chunk",
            json!({"workspaceId": fixture.workspace_id, "attachmentId": id,
                "offset": offset, "length": 1_048_576}),
        ));
        let decoded = STANDARD.decode(chunk["content"].as_str().unwrap()).unwrap();
        assert_eq!(chunk["offset"], offset);
        assert_eq!(chunk["nextOffset"], offset + decoded.len());
        assert_eq!(chunk["eof"], offset + decoded.len() == bytes.len());
        downloaded.extend_from_slice(&decoded);
    }
    assert_eq!(downloaded, bytes, "every uploaded byte must survive");
    assert_eq!(digest(&downloaded), expected_sha256);
    assert_eq!(fixture.count("attachment_job"), 1);
}

#[test]
fn bad_chunk_size_and_file_declarations_do_not_mutate_sessions() {
    let fixture = Fixture::new();
    fixture.create("doc");
    let principal = local_principal();
    let upload = begin(&fixture, &principal, "bounded.txt", b"ok");
    let oversized = fixture.call(
        &principal,
        "append_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload, "index": 0,
            "content": STANDARD.encode(vec![b'x'; 1_048_577])}),
    );
    assert_error(&oversized, "1,048,576");
    assert_eq!(fixture.count("attachment_upload_chunk"), 0);
    let accepted = result(&fixture.call(
        &principal,
        "append_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
            "index": 0, "content": STANDARD.encode(b"ok")}),
    ));
    assert_eq!(accepted["nextIndex"], 1);
    assert_eq!(accepted["receivedBytes"], 2);
    let replay = result(&fixture.call(
        &principal,
        "append_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
            "index": 0, "content": STANDARD.encode(b"ok")}),
    ));
    assert_eq!(replay, accepted);
    assert_error(
        &fixture.call(
            &principal,
            "append_attachment_chunk",
            json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
                "index": 0, "content": STANDARD.encode(b"no")}),
        ),
        "order",
    );
    assert_eq!(fixture.count("attachment_upload_chunk"), 1);

    let bad_mime = fixture.call(
        &principal,
        "begin_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "entityName": "doc",
            "filename": "bad.bin", "mime": "application/octet-stream",
            "expectedBytes": 2, "sha256": digest(b"ok")}),
    );
    assert_error(&bad_mime, "MIME");
    let large_file = fixture.call(
        &principal,
        "begin_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "entityName": "doc",
            "filename": "large.txt", "mime": "text/plain",
            "expectedBytes": 52_428_801, "sha256": digest(b"ok")}),
    );
    assert_error(&large_file, "per-file");
    assert_eq!(fixture.count("attachment_upload"), 1);
}

#[test]
fn attachment_tools_require_separate_consent_even_inside_a_batch() {
    let fixture = Fixture::new();
    fixture.create("doc");
    let writer = fixture.machine("writer", &["graph-write"], "writer");
    let before = fixture.count("entity");
    let outcome = fixture.request(
        &writer,
        &json!([
            {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "create_entities", "arguments": {
                    "workspaceId": fixture.workspace_id,
                    "entities": [{"name":"must-not-exist","entityType":"doc","observations":[]}]
                }
            }},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "begin_attachment_upload", "arguments": {"workspaceId": fixture.workspace_id,
                    "entityName":"doc", "filename":"blocked.txt", "mime":"text/plain",
                    "expectedBytes":1, "sha256":digest(b"x")}
            }}
        ]),
    );
    assert!(matches!(outcome, HttpOutcome::InsufficientScope(scopes) if scopes == ["attachments"]));
    assert_eq!(fixture.count("entity"), before);
    assert_eq!(fixture.count("attachment_upload"), 0);
    assert!(matches!(
        fixture.request(&writer, &json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"list_attachments", "arguments":{"workspaceId":fixture.workspace_id,
                "entityName":"doc"}}})),
        HttpOutcome::InsufficientScope(scopes) if scopes == ["attachments"]
    ));

    let listed = body_of(fixture.request(
        &writer,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list"
        }),
    ));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    for tool in ATTACHMENT_TOOL_NAMES {
        assert!(!names.contains(tool), "missing scope exposes {tool}");
    }
    let owner_list = body_of(fixture.request(
        &local_principal(),
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list"
        }),
    ));
    let tools = owner_list["result"]["tools"].as_array().unwrap();
    for tool in ATTACHMENT_TOOL_NAMES {
        assert!(
            tools.iter().any(|entry| entry["name"] == *tool),
            "missing {tool}"
        );
    }
}

#[test]
fn upload_session_is_bound_to_principal_and_current_workspace() {
    let fixture = Fixture::new();
    fixture.create("doc");
    let alice = fixture.machine("alice", &["attachments"], "writer");
    let bob = fixture.machine("bob", &["attachments"], "writer");
    let upload = begin(&fixture, &alice, "private.txt", b"secret");
    assert_error(
        &fixture.call(
            &bob,
            "append_attachment_chunk",
            json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
                "index": 0, "content": STANDARD.encode(b"secret")}),
        ),
        "another principal",
    );
    assert_eq!(fixture.count("attachment_upload_chunk"), 0);

    let second = fixture
        .registry
        .create("machine:local", "second", Visibility::Private, |path| {
            fixture.handles.initialize_graph(path)
        })
        .unwrap();
    fixture
        .registry
        .grant("machine:local", &second.workspace_id, &alice.id, "writer")
        .unwrap();
    fixture
        .registry
        .set_default(&alice.id, &second.workspace_id)
        .unwrap();
    assert_error(
        &fixture.call(
            &alice,
            "append_attachment_chunk",
            json!({"uploadId": upload, "index": 0, "content": STANDARD.encode(b"secret")}),
        ),
        "not found",
    );
    assert_eq!(fixture.count("attachment_upload_chunk"), 0);
    let second_path = fixture
        .registry
        .resolve(&alice.id, None, WorkspaceAccess::Read)
        .unwrap()
        .graph_path;
    let other = Connection::open(second_path).unwrap();
    let stray: i64 = other
        .query_row("SELECT count(*) FROM attachment_upload", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stray, 0);
    let resumed = result(&fixture.call(
        &alice,
        "append_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
            "index": 0, "content": STANDARD.encode(b"secret")}),
    ));
    assert_eq!(resumed["receivedBytes"], 6);
}

#[test]
fn workspace_reader_can_read_but_not_upload_or_delete() {
    let fixture = Fixture::new();
    fixture.create("doc");
    let owner = local_principal();
    let reader = fixture.machine("reader", &["attachments"], "reader");
    let upload = begin(&fixture, &owner, "readable.txt", b"hello");
    result(&fixture.call(
        &owner,
        "append_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
            "index": 0, "content": STANDARD.encode(b"hello")}),
    ));
    let finished = result(&fixture.call(
        &owner,
        "finish_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload}),
    ));
    let id = finished["attachmentId"].as_i64().unwrap();
    let content = result(&fixture.call(
        &reader,
        "read_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "attachmentId": id,
            "offset": 0, "length": 5}),
    ));
    assert_eq!(
        STANDARD
            .decode(content["content"].as_str().unwrap())
            .unwrap(),
        b"hello"
    );
    assert_error(
        &fixture.call(
            &reader,
            "begin_attachment_upload",
            json!({"workspaceId": fixture.workspace_id, "entityName": "doc",
                "filename": "denied.txt", "mime": "text/plain",
                "expectedBytes": 1, "sha256": digest(b"x")}),
        ),
        "access denied",
    );
    assert_error(
        &fixture.call(
            &reader,
            "delete_attachment",
            json!({"workspaceId": fixture.workspace_id, "attachmentId": id}),
        ),
        "access denied",
    );
    assert_eq!(fixture.count("attachment"), 1);
}

#[test]
fn duplicate_filename_quota_and_delete_have_separate_outcomes() {
    let fixture = Fixture::with_limits(Some(52_428_800));
    fixture.create("doc");
    fixture.create("another");
    let owner = local_principal();
    let bytes = b"first";
    let upload = begin(&fixture, &owner, "shared.txt", bytes);
    result(&fixture.call(
        &owner,
        "append_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
            "index": 0, "content": STANDARD.encode(bytes)}),
    ));
    let finished = result(&fixture.call(
        &owner,
        "finish_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload}),
    ));
    let id = finished["attachmentId"].as_i64().unwrap();
    assert_error(
        &fixture.call(
            &owner,
            "begin_attachment_upload",
            json!({"workspaceId": fixture.workspace_id, "entityName": "doc",
                "filename": "shared.txt", "mime": "text/plain", "expectedBytes": 1,
                "sha256": digest(b"y")}),
        ),
        "duplicate",
    );
    let other = fixture.call(
        &owner,
        "begin_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "entityName": "another",
            "filename": "shared.txt", "mime": "text/plain", "expectedBytes": 1,
            "sha256": digest(b"y")}),
    );
    let other_id = result(&other)["uploadId"].as_str().unwrap().to_owned();
    assert_error(
        &fixture.call(
            &owner,
            "begin_attachment_upload",
            json!({"workspaceId": fixture.workspace_id, "entityName": "doc",
                "filename": "reserved.txt", "mime": "text/plain",
                "expectedBytes": 52_428_800, "sha256": digest(b"x")}),
        ),
        "budget",
    );
    result(&fixture.call(
        &owner,
        "cancel_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "uploadId": other_id}),
    ));
    result(&fixture.call(
        &owner,
        "delete_attachment",
        json!({"workspaceId": fixture.workspace_id, "attachmentId": id}),
    ));
    assert_eq!(fixture.count("attachment"), 0);
    assert_eq!(fixture.count("attachment_job"), 0);
    let fresh = begin(&fixture, &owner, "shared.txt", b"new");
    assert!(!fresh.is_empty());
}

#[test]
fn page_reads_count_unicode_scalars_and_bound_responses() {
    let fixture = Fixture::new();
    fixture.create("doc");
    let owner = local_principal();
    let upload = begin(&fixture, &owner, "unicode.txt", "aé日b".as_bytes());
    result(&fixture.call(
        &owner,
        "append_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
            "index": 0, "content": STANDARD.encode("aé日b")}),
    ));
    let id = result(&fixture.call(
        &owner,
        "finish_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload}),
    ))["attachmentId"]
        .as_i64()
        .unwrap();
    let conn = Connection::open(&fixture.path).unwrap();
    conn.execute(
        "INSERT INTO attachment_text(attachment_id,page,text,chars) VALUES(?1,1,?2,4)",
        rusqlite::params![id, "aé日b"],
    )
    .unwrap();
    let page = result(&fixture.call(
        &owner,
        "get_attachment_page",
        json!({"workspaceId": fixture.workspace_id, "attachmentId": id,
            "page": 1, "offset": 1, "maxChars": 2}),
    ));
    assert_eq!(
        page,
        json!({"page": 1, "text": "é日", "offset": 1, "nextOffset": 3, "eof": false})
    );
    assert_error(
        &fixture.call(
            &owner,
            "get_attachment_page",
            json!({"workspaceId": fixture.workspace_id, "attachmentId": id,
                "page": 1, "maxChars": 4_097}),
        ),
        "4,096",
    );
    assert_error(
        &fixture.call(
            &owner,
            "read_attachment_chunk",
            json!({"workspaceId": fixture.workspace_id, "attachmentId": id,
                "offset": 0, "length": 1_048_577}),
        ),
        "1,048,576",
    );
}

#[test]
fn max_chars_zero_returns_an_empty_slice_not_the_whole_page() {
    let fixture = Fixture::new();
    fixture.create("doc");
    let owner = local_principal();
    let upload = begin(&fixture, &owner, "page.txt", b"four");
    result(&fixture.call(
        &owner,
        "append_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
            "index": 0, "content": STANDARD.encode(b"four")}),
    ));
    let id = result(&fixture.call(
        &owner,
        "finish_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload}),
    ))["attachmentId"]
        .as_i64()
        .unwrap();
    let conn = Connection::open(&fixture.path).unwrap();
    conn.execute(
        "INSERT INTO attachment_text(attachment_id,page,text,chars) VALUES(?1,1,?2,4)",
        rusqlite::params![id, "four"],
    )
    .unwrap();
    let page = result(&fixture.call(
        &owner,
        "get_attachment_page",
        json!({"workspaceId": fixture.workspace_id, "attachmentId": id,
            "page": 1, "offset": 0, "maxChars": 0}),
    ));
    assert_eq!(
        page,
        json!({"page": 1, "text": "", "offset": 0, "nextOffset": 0, "eof": false})
    );
}

#[test]
fn empty_chunk_is_refused_while_a_zero_byte_file_finishes_without_chunks() {
    let fixture = Fixture::new();
    fixture.create("doc");
    let owner = local_principal();
    let upload = begin(&fixture, &owner, "zero.txt", b"");
    let appended = fixture.call(
        &owner,
        "append_attachment_chunk",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload,
            "index": 0, "content": ""}),
    );
    assert_error(&appended, "empty");
    assert_eq!(fixture.count("attachment_upload_chunk"), 0);
    let finished = result(&fixture.call(
        &owner,
        "finish_attachment_upload",
        json!({"workspaceId": fixture.workspace_id, "uploadId": upload}),
    ));
    assert_eq!(finished["status"], "uploaded");
    assert_eq!(fixture.count("attachment_chunk"), 0);
}

#[tokio::test]
async fn oversized_single_mcp_body_still_fails_at_sixteen_mebibytes() {
    let dir = tempfile::tempdir().unwrap();
    let state = mcpmem::http::HttpState::for_test(mcpmem::http::TestSetup {
        db_path: dir.path().join("mcp.sqlite"),
        oauth: None,
        auth_token: Some(Arc::from("secret")),
        metadata_fetch: None,
        bearer_scopes: vec![ToolCategory::Attachments],
        enabled_categories: vec![ToolCategory::Attachments],
        now_us: None,
        ui_enabled: true,
    });
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "append_attachment_chunk",
            "arguments": {"content": "a".repeat(MAX_REQUEST_BYTES)}
        }
    });
    let message = call.to_string();
    let response = mcpmem::http::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(message))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
