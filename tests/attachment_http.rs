#![cfg(feature = "oauth")]

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Request, Response, StatusCode, header};
use futures::stream;
use http_body_util::BodyExt;
use mcpmem::principals::PrincipalEntry;
use mcpmem::tools::ToolCategory;
use mcpmem_oauth::store::{Grant, Store, TokenKind};
use rusqlite::Connection;
use serde_json::{Value, json};

mod support;

const CHUNK: usize = 1_048_576;
const FILE_BYTES: usize = 52_428_800;

fn plant(store: &Store, principal: &str, scopes: &[&str]) -> String {
    let token = mcpmem_oauth::new_token();
    let now = mcpmem_core::events::now_us();
    store
        .put_token(
            &token,
            TokenKind::Access,
            &Grant {
                client_id: mcpmem_oauth::ADMIN_CLIENT_ID.to_owned(),
                principal: principal.to_owned(),
                scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
                resource: format!("{}/mcp", support::PUBLIC_URL),
                family: mcpmem_oauth::new_token(),
            },
            now,
            now + 3_600_000_000,
        )
        .unwrap();
    token
}

struct Fixture {
    server: support::Server,
    owner: String,
    reader: String,
    writer_without_scope: String,
    workspace: String,
}

async fn fixture(enabled_attachments: bool) -> Fixture {
    let mut oauth = support::oauth_config("https://idp.invalid");
    for (name, sub) in [("reader", "sub-2"), ("unscoped", "sub-3")] {
        oauth.principals.push(PrincipalEntry {
            name: name.into(),
            iss: "https://idp.invalid".into(),
            sub: sub.into(),
            label: None,
            scopes: vec![
                "graph-read".into(),
                "graph-write".into(),
                "attachments".into(),
            ],
        });
    }
    oauth.principals[0].scopes.push("attachments".into());
    let mut enabled = ToolCategory::ALL.to_vec();
    if !enabled_attachments {
        enabled.retain(|category| *category != ToolCategory::Attachments);
    }
    let server = support::server(
        Some(oauth.clone()),
        support::Scopes {
            bearer: Vec::new(),
            enabled,
        },
        None,
    )
    .await;
    let (owner, reader, writer_without_scope) = server.oauth().with_store(|store| {
        let id = |index: usize| {
            mcpmem::principals::human_id(&oauth.principals[index].iss, &oauth.principals[index].sub)
        };
        (
            plant(store, &id(0), &["graph-read", "graph-write", "attachments"]),
            plant(store, &id(1), &["graph-read", "attachments"]),
            plant(store, &id(2), &["graph-write"]),
        )
    });
    let created = mcp(
        &server,
        &owner,
        "create_workspace",
        json!({"name": "private", "visibility": "private"}),
    )
    .await;
    let workspace = created["result"]["workspace"]["workspaceId"]
        .as_str()
        .expect("a workspace id")
        .to_owned();
    let seeded = mcp(
        &server,
        &owner,
        "create_entities",
        json!({
            "workspaceId": workspace,
            "entities": [
                { "name": "Alice", "entityType": "person", "observations": [] },
                { "name": "Bob", "entityType": "person", "observations": [] },
            ],
        }),
    )
    .await;
    assert!(
        seeded.get("error").is_none() && seeded["result"]["isError"].as_bool() != Some(true),
        "seeding failed: {seeded}"
    );
    let reader_id =
        mcpmem::principals::human_id(&oauth.principals[1].iss, &oauth.principals[1].sub);
    let grant = mcp(
        &server,
        &owner,
        "grant_workspace_access",
        json!({"workspaceId":workspace,"principalId":reader_id,"role":"reader"}),
    )
    .await;
    assert_eq!(grant["result"]["grant"]["role"], "reader", "{grant}");
    Fixture {
        server,
        owner,
        reader,
        writer_without_scope,
        workspace,
    }
}

async fn mcp(server: &support::Server, token: &str, tool: &str, arguments: Value) -> Value {
    let response = server
        .request(
            Request::post("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc":"2.0","id":1,"method":"tools/call",
                        "params":{"name":tool,"arguments":arguments}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK, "{tool}");
    support::json(response).await
}

fn upload_path(ws: &str, entity: &str, filename: &str) -> String {
    format!("/ui/attachments?workspaceId={ws}&entityName={entity}&filename={filename}")
}

async fn send(
    server: &support::Server,
    token: &str,
    method: &str,
    path: &str,
    body: Body,
    mime: Option<&str>,
) -> Response<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if !token.is_empty() {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(mime) = mime {
        builder = builder.header(header::CONTENT_TYPE, mime);
    }
    server.request(builder.body(body).unwrap()).await
}

async fn data(response: Response<Body>) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

fn graph(fixture: &Fixture) -> Connection {
    let registry_path = format!(
        "{}.workspaces.sqlite",
        fixture.server.memory_db_path().display()
    );
    let registry = Connection::open(registry_path).unwrap();
    let path: String = registry
        .query_row(
            "SELECT graph_path FROM workspace WHERE workspace_id=?1",
            [&fixture.workspace],
            |row| row.get(0),
        )
        .unwrap();
    Connection::open(path).unwrap()
}

fn attachment_count(fixture: &Fixture) -> i64 {
    graph(fixture)
        .query_row("SELECT count(*) FROM attachment", [], |row| row.get(0))
        .unwrap()
}

/// The real-transport smoke: a spawned `mcpmem` binary served over HTTP/1.1
/// and driven by a raw TCP client, so the 50 MiB upload and the streamed
/// download cross an actual socket with no Content-Length on the upload.
struct ChildServer {
    child: Child,
    port: u16,
    db_path: String,
    log_path: String,
}

impl Drop for ChildServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for ext in [
            "",
            "-wal",
            "-shm",
            ".workspaces.sqlite",
            ".workspaces.sqlite-wal",
            ".workspaces.sqlite-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{ext}", self.db_path));
        }
        let _ = std::fs::remove_dir_all(format!("{}.workspaces", self.db_path));
        let _ = std::fs::remove_file(&self.log_path);
    }
}

fn spawn_child_server() -> ChildServer {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let pid = std::process::id();
    let db_path = format!("/tmp/attachment_http_{pid}_{port}.db");
    let log_path = format!("/tmp/attachment_http_{pid}_{port}.log");
    let bin =
        std::env::var("CARGO_BIN_EXE_mcpmem").unwrap_or_else(|_| "target/debug/mcpmem".into());
    let mut child = Command::new(bin)
        .arg("-f")
        .arg(&db_path)
        .arg("--legacy-owner-id")
        .arg("machine:local")
        .arg("--transport")
        .arg("http")
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--auth-token")
        .arg("attachment-http-test-bearer")
        .arg("--enable-all")
        .arg("--log-level")
        .arg("info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the mcpmem binary");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return ChildServer {
                child,
                port,
                db_path,
                log_path,
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("mcpmem child did not start serving on 127.0.0.1:{port}");
}

/// One raw HTTP request with a known-length body; returns (status, headers, body).
fn raw_request(
    port: u16,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: Option<&[u8]>,
) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer attachment-http-test-bearer\r\n"
    );
    if let Some(mime) = content_type {
        head.push_str(&format!("Content-Type: {mime}\r\n"));
    }
    if let Some(bytes) = body {
        head.push_str(&format!("Content-Length: {}\r\n", bytes.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut wire = head.into_bytes();
    if let Some(bytes) = body {
        wire.extend_from_slice(bytes);
    }
    stream.write_all(&wire).unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a response head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .expect("a status code");
    (status, head, raw[split + 4..].to_vec())
}

/// Streaming POST with `Transfer-Encoding: chunked` and no Content-Length.
fn raw_chunked_upload(
    port: u16,
    path: &str,
    mime: &str,
    frames: &[Vec<u8>],
) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer attachment-http-test-bearer\r\nContent-Type: {mime}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).unwrap();
    for frame in frames {
        write!(&mut stream, "{:x}\r\n", frame.len()).unwrap();
        stream.write_all(frame).unwrap();
        stream.write_all(b"\r\n").unwrap();
    }
    stream.write_all(b"0\r\n\r\n").unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a response head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .expect("a status code");
    (status, head, raw[split + 4..].to_vec())
}

fn mcp_call(port: u16, request: &str) -> Value {
    let (status, _, body) = raw_request(
        port,
        "POST",
        "/mcp",
        Some("application/json"),
        Some(request.as_bytes()),
    );
    assert_eq!(status, 200, "{request}: {body:?}");
    serde_json::from_slice(&body).expect("a JSON-RPC body")
}

#[test]
fn real_server_streams_a_fifty_mib_upload_and_download() {
    let server = spawn_child_server();
    let created = mcp_call(
        server.port,
        r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"create_workspace","arguments":{"name":"fixture","visibility":"private"}},"id":1}"#,
    );
    let workspace = created["result"]["workspace"]["workspaceId"]
        .as_str()
        .expect("the workspace id")
        .to_owned();
    let seeded = mcp_call(
        server.port,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_entities","arguments":{{"workspaceId":"{workspace}","entities":[{{"name":"Alice","entityType":"person","observations":[]}}]}}}},"id":2}}"#
        ),
    );
    assert!(
        seeded.get("error").is_none() && seeded["result"]["isError"].as_bool() != Some(true),
        "seeding failed: {seeded}"
    );

    // 800 frames of 64 KiB: one uniform byte value per 1 MiB block, so the
    // downloaded bytes prove both order and completeness.
    let frames: Vec<Vec<u8>> = (0..800)
        .map(|frame| vec![(frame / 16) as u8; 64 * 1024])
        .collect();
    let upload_path =
        format!("/ui/attachments?workspaceId={workspace}&entityName=Alice&filename=real.bin");
    let (status, _, body) =
        raw_chunked_upload(server.port, &upload_path, "application/pdf", &frames);
    assert_eq!(
        status,
        201,
        "the real upload must be accepted: {}",
        String::from_utf8_lossy(&body)
    );
    let upload: Value = serde_json::from_slice(&body).expect("an upload reply");
    assert_eq!(upload["status"], "uploaded");
    let id = upload["attachmentId"].as_i64().expect("an attachment id");

    let (status, headers, bytes) = raw_request(
        server.port,
        "GET",
        &format!("/ui/attachments/{id}/download?workspaceId={workspace}"),
        None,
        None,
    );
    assert_eq!(status, 200);
    assert!(
        headers
            .to_lowercase()
            .contains("content-type: application/pdf"),
        "{headers}"
    );
    assert_eq!(bytes.len(), FILE_BYTES);
    for (within, byte) in bytes.iter().enumerate() {
        assert_eq!(*byte, (within / CHUNK) as u8);
    }
}

fn chunked_body(chunks: usize, bytes_per_chunk: usize) -> Body {
    Body::from_stream(stream::unfold(0, move |index| async move {
        (index < chunks).then(|| {
            (
                Ok::<Bytes, std::io::Error>(Bytes::from(vec![index as u8; bytes_per_chunk])),
                index + 1,
            )
        })
    }))
}

#[tokio::test]
async fn fifty_mib_upload_and_download_have_identical_bytes_without_a_full_buffer() {
    let fx = fixture(true).await;
    let post = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "large.txt"),
        chunked_body(50, CHUNK),
        Some("text/plain"),
    )
    .await;
    assert_eq!(post.status(), StatusCode::CREATED);
    let body = data(post).await;
    assert_eq!(body["status"], "uploaded");
    let id = body["attachmentId"].as_i64().unwrap();

    let download = send(
        &fx.server,
        &fx.owner,
        "GET",
        &format!("/ui/attachments/{id}/download?workspaceId={}", fx.workspace),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(download.headers()[header::CONTENT_TYPE], "text/plain");
    assert_eq!(
        download.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"large.txt\""
    );
    let mut body = download.into_body();
    let mut offset = 0;
    while let Some(frame) = body.frame().await {
        let data = frame.unwrap().into_data().unwrap();
        assert!(
            data.len() <= 64 * 1024,
            "a download frame must stay bounded"
        );
        for (within, byte) in data.iter().enumerate() {
            assert_eq!(*byte, ((offset + within) / CHUNK) as u8);
        }
        offset += data.len();
    }
    assert_eq!(offset, FILE_BYTES);
}

#[tokio::test]
async fn byte_after_limit_is_rejected_even_with_a_false_short_length() {
    let fx = fixture(true).await;
    let request = Request::post(upload_path(&fx.workspace, "Alice", "too-large.txt"))
        .header(header::AUTHORIZATION, format!("Bearer {}", fx.owner))
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::CONTENT_LENGTH, "3")
        .body(chunked_body(50, CHUNK + 1))
        .unwrap();
    let response = fx.server.request(request).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        data(response).await["error"]
            .as_str()
            .unwrap()
            .contains("per-file")
    );
    assert_eq!(attachment_count(&fx), 0);
}

#[tokio::test]
async fn missing_length_is_allowed_and_advertised_oversize_reads_no_bytes() {
    let fx = fixture(true).await;
    let response = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "short.txt"),
        chunked_body(1, 3),
        Some("text/plain"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let polled = Arc::new(AtomicUsize::new(0));
    let read = Arc::clone(&polled);
    let body = Body::from_stream(stream::once(async move {
        read.fetch_add(1, Ordering::SeqCst);
        Ok::<Bytes, std::io::Error>(Bytes::from_static(b"ignored"))
    }));
    let request = Request::post(upload_path(&fx.workspace, "Alice", "oversize.txt"))
        .header(header::AUTHORIZATION, format!("Bearer {}", fx.owner))
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::CONTENT_LENGTH, (FILE_BYTES + 1).to_string())
        .body(body)
        .unwrap();
    assert_eq!(
        fx.server.request(request).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(polled.load(Ordering::SeqCst), 0);
    assert_eq!(attachment_count(&fx), 1);
}

#[tokio::test]
async fn disconnected_upload_does_not_create_an_attachment_or_a_job() {
    let fx = fixture(true).await;
    let body = Body::from_stream(stream::iter([
        Ok(Bytes::from_static(b"partial")),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "disconnect",
        )),
    ]));
    let response = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "partial.txt"),
        body,
        Some("text/plain"),
    )
    .await;
    assert!(!response.status().is_success());
    assert_eq!(attachment_count(&fx), 0);
    let jobs: i64 = graph(&fx)
        .query_row("SELECT count(*) FROM attachment_job", [], |row| row.get(0))
        .unwrap();
    assert_eq!(jobs, 0);
}

#[tokio::test]
async fn mime_duplicate_and_entity_rules_leave_the_original_intact() {
    let fx = fixture(true).await;
    let invalid = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "x.bin"),
        Body::from("bad"),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(
        data(invalid).await["error"]
            .as_str()
            .unwrap()
            .contains("MIME")
    );
    let first = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "shared.txt"),
        Body::from("first"),
        Some("text/plain"),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let id = data(first).await["attachmentId"].as_i64().unwrap();
    let duplicate = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "shared.txt"),
        Body::from("second"),
        Some("text/plain"),
    )
    .await;
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    assert!(
        data(duplicate).await["error"]
            .as_str()
            .unwrap()
            .contains("duplicate")
    );
    let second = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Bob", "shared.txt"),
        Body::from("second"),
        Some("text/plain"),
    )
    .await;
    assert_eq!(second.status(), StatusCode::CREATED);
    let conn = graph(&fx);
    let original: (i64, i64) = conn
        .query_row(
            "SELECT size_bytes,revision FROM attachment WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(original, (5, 1));
    assert_eq!(attachment_count(&fx), 2);
}

#[tokio::test]
async fn every_route_refuses_a_disabled_category_before_it_reads_a_body() {
    let fx = fixture(false).await;
    let count = Arc::new(AtomicUsize::new(0));
    let read = Arc::clone(&count);
    let body = Body::from_stream(stream::once(async move {
        read.fetch_add(1, Ordering::SeqCst);
        Ok::<Bytes, std::io::Error>(Bytes::from_static(b"not consumed"))
    }));
    let routes = [
        (
            "POST",
            upload_path(&fx.workspace, "Alice", "disabled.txt"),
            body,
            Some("text/plain"),
        ),
        (
            "GET",
            format!(
                "/ui/attachments?workspaceId={}&entityName=Alice",
                fx.workspace
            ),
            Body::empty(),
            None,
        ),
        (
            "GET",
            format!("/ui/attachments/1?workspaceId={}", fx.workspace),
            Body::empty(),
            None,
        ),
        (
            "GET",
            format!(
                "/ui/attachments/1/pages?workspaceId={}&page=1",
                fx.workspace
            ),
            Body::empty(),
            None,
        ),
        (
            "GET",
            format!("/ui/attachments/1/download?workspaceId={}", fx.workspace),
            Body::empty(),
            None,
        ),
        (
            "DELETE",
            format!("/ui/attachments/1?workspaceId={}", fx.workspace),
            Body::empty(),
            None,
        ),
    ];
    for (method, path, body, mime) in routes {
        let response = send(&fx.server, &fx.owner, method, &path, body, mime).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(attachment_count(&fx), 0);
}

#[tokio::test]
async fn scope_and_workspace_gates_deny_before_any_upload_read() {
    let fx = fixture(true).await;
    for (token, expected_status) in [
        (&fx.reader, StatusCode::NOT_FOUND),
        (&fx.writer_without_scope, StatusCode::FORBIDDEN),
        (&"".to_owned(), StatusCode::UNAUTHORIZED),
    ] {
        let count = Arc::new(AtomicUsize::new(0));
        let polled = Arc::clone(&count);
        let body = Body::from_stream(stream::once(async move {
            polled.fetch_add(1, Ordering::SeqCst);
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"never read"))
        }));
        let response = send(
            &fx.server,
            token,
            "POST",
            &upload_path(&fx.workspace, "Alice", "denied.txt"),
            body,
            Some("text/plain"),
        )
        .await;
        assert_eq!(response.status(), expected_status);
        if expected_status == StatusCode::FORBIDDEN {
            assert!(
                response.headers()[header::WWW_AUTHENTICATE]
                    .to_str()
                    .unwrap()
                    .contains("attachments")
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
    assert_eq!(attachment_count(&fx), 0);
}

#[tokio::test]
async fn a_scoped_reader_reads_metadata_and_page_but_cannot_delete() {
    let fx = fixture(true).await;
    let uploaded = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "safe.txt"),
        Body::from("hello"),
        Some("text/plain"),
    )
    .await;
    let id = data(uploaded).await["attachmentId"].as_i64().unwrap();
    graph(&fx)
        .execute(
            "INSERT INTO attachment_text(attachment_id,page,text,chars) VALUES(?1,1,'Aé🙂Z',4)",
            [id],
        )
        .unwrap();
    let list = send(
        &fx.server,
        &fx.reader,
        "GET",
        &format!(
            "/ui/attachments?workspaceId={}&entityName=Alice",
            fx.workspace
        ),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(list.status(), StatusCode::OK);
    assert_eq!(data(list).await["attachments"][0]["attachmentId"], id);
    let meta = send(
        &fx.server,
        &fx.reader,
        "GET",
        &format!("/ui/attachments/{id}?workspaceId={}", fx.workspace),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(meta.status(), StatusCode::OK);
    let metadata = data(meta).await;
    assert_eq!(metadata["entityName"], "Alice");
    assert_eq!(metadata["pageCount"], 1);
    assert!(metadata.get("content").is_none());
    let page = send(
        &fx.server,
        &fx.reader,
        "GET",
        &format!(
            "/ui/attachments/{id}/pages?workspaceId={}&page=1&offset=1&maxChars=2",
            fx.workspace
        ),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    let expect = json!({
        "page": 1,
        "text": "é🙂",
        "offset": 1,
        "nextOffset": 3,
        "eof": false,
    });
    assert_eq!(data(page).await, expect);
    let delete = send(
        &fx.server,
        &fx.reader,
        "DELETE",
        &format!("/ui/attachments/{id}?workspaceId={}", fx.workspace),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(delete.status(), StatusCode::NOT_FOUND);
    let removed = send(
        &fx.server,
        &fx.owner,
        "DELETE",
        &format!("/ui/attachments/{id}?workspaceId={}", fx.workspace),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert_eq!(attachment_count(&fx), 0);
}

#[tokio::test]
async fn the_global_mcp_request_limit_remains_sixteen_mib() {
    let fx = fixture(true).await;
    let huge = Request::post("/mcp")
        .header(header::AUTHORIZATION, format!("Bearer {}", fx.owner))
        .header(header::CONTENT_TYPE, "application/json")
        .body(chunked_body(17, CHUNK))
        .unwrap();
    assert_eq!(
        fx.server.request(huge).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

/// The attachment HTTP routes open the resolved graph directly, bypassing
/// the lazy `WorkspaceHandles` initialization that migrates every other
/// graph. A workspace whose file predates the attachments migration (no
/// `attachment` tables, no migration-15 ledger row) must be upgraded before
/// the first attachment SQL runs, or the request dies with
/// `no such table: attachment`.
#[tokio::test]
async fn attachment_routes_upgrade_a_pre_attachment_workspace() {
    let fx = fixture(true).await;
    // Roll the workspace's graph back to before migration 15 (attachments):
    // drop the attachment schema and the migration's ledger row.
    let conn = graph(&fx);
    conn.execute_batch(
        "DELETE FROM schema_migration WHERE version=15;
         DROP TABLE IF EXISTS attachment_upload_chunk;
         DROP TABLE IF EXISTS attachment_upload;
         DROP TABLE IF EXISTS attachment_job;
         DROP TABLE IF EXISTS attachment_chunk;
         DROP TABLE IF EXISTS attachment_text;
         DROP TABLE IF EXISTS attachment;",
    )
    .expect("roll the graph back before the attachments migration");
    drop(conn);

    let list = send(
        &fx.server,
        &fx.owner,
        "GET",
        &format!(
            "/ui/attachments?workspaceId={}&entityName=Alice",
            fx.workspace
        ),
        Body::empty(),
        None,
    )
    .await;
    let (status, body) = {
        let (parts, body) = list.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (
            parts.status,
            serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
        )
    };
    assert_eq!(
        status,
        StatusCode::OK,
        "the route must bootstrap the graph before reading attachment tables: {body}"
    );
    assert_eq!(body["attachments"], json!([]));

    // The bootstrap re-applied migration 15, so the table exists again.
    let migrated: i64 = graph(&fx)
        .query_row("SELECT count(*) FROM attachment", [], |row| row.get(0))
        .expect("migration 15 must be re-applied");
    assert_eq!(migrated, 0);

    // Roll the graph back a second time. The download route must bootstrap
    // the graph on its own, like list, get, pages, post, and delete.
    let conn = graph(&fx);
    conn.execute_batch(
        "DELETE FROM schema_migration WHERE version=15;
         DROP TABLE IF EXISTS attachment_upload_chunk;
         DROP TABLE IF EXISTS attachment_upload;
         DROP TABLE IF EXISTS attachment_job;
         DROP TABLE IF EXISTS attachment_chunk;
         DROP TABLE IF EXISTS attachment_text;
         DROP TABLE IF EXISTS attachment;",
    )
    .expect("roll the graph back a second time");
    drop(conn);

    let download = send(
        &fx.server,
        &fx.owner,
        "GET",
        &format!("/ui/attachments/1/download?workspaceId={}", fx.workspace),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(
        download.status(),
        StatusCode::NOT_FOUND,
        "the download route must bootstrap the graph before reading attachment tables"
    );
    let migrated: i64 = graph(&fx)
        .query_row("SELECT count(*) FROM attachment", [], |row| row.get(0))
        .expect("migration 15 must be re-applied by the download route");
    assert_eq!(migrated, 0);
}

/// The spool bound is process-wide: many partial chunked uploads must not be
/// able to hold more than [`MAX_CONCURRENT_ATTACHMENT_SPOOLS`] anonymous
/// spool files at once. A new upload is rejected with 503 while the bound is
/// full, and the capacity returns when the stalls disconnect.
#[tokio::test]
async fn concurrent_upload_spools_hit_a_process_wide_bound_and_release() {
    let server = spawn_child_server();
    let created = mcp_call(
        server.port,
        r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"create_workspace","arguments":{"name":"fixture","visibility":"private"}},"id":1}"#,
    );
    let workspace = created["result"]["workspace"]["workspaceId"]
        .as_str()
        .expect("the workspace id")
        .to_owned();
    let seeded = mcp_call(
        server.port,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_entities","arguments":{{"workspaceId":"{workspace}","entities":[{{"name":"Alice","entityType":"person","observations":[]}}]}}}},"id":2}}"#
        ),
    );
    assert!(
        seeded.get("error").is_none() && seeded["result"]["isError"].as_bool() != Some(true),
        "seeding failed: {seeded}"
    );

    // Open more stalled chunked uploads than the process-wide spool bound.
    // Each sends one chunk and leaves the body unterminated, so its handler
    // parks on the next frame; the bound admits at most the constant's worth
    // of them before later uploads are rejected.
    let stalls: Vec<TcpStream> = (0..mcpmem::http::MAX_CONCURRENT_ATTACHMENT_SPOOLS * 2)
        .map(|index| {
            open_stalled_upload(
                server.port,
                &format!(
                    "/ui/attachments?workspaceId={workspace}&entityName=Alice&filename=stalled-{index}.txt"
                ),
                "text/plain",
            )
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));

    // With the bound full, a fresh complete upload must be rejected with 503.
    let mut rejected = None;
    for attempt in 0..200 {
        let (status, _, body) = raw_chunked_upload(
            server.port,
            &format!(
                "/ui/attachments?workspaceId={workspace}&entityName=Alice&filename=probe-{attempt}.txt"
            ),
            "text/plain",
            &[b"abc".to_vec()],
        );
        if status == 503 {
            rejected = Some(body);
            break;
        }
        assert_eq!(
            status,
            201,
            "an upload that did not hit the spool bound must still succeed: {}",
            String::from_utf8_lossy(&body)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let rejected = rejected.expect("the process-wide spool bound never filled");
    let rejected: Value = serde_json::from_slice(&rejected).expect("a json rejection body");
    assert!(
        rejected["error"]
            .as_str()
            .unwrap()
            .contains("spool capacity"),
        "the rejection must name the spool bound: {rejected}"
    );

    // Disconnecting the stalled uploads must free their capacity.
    drop(stalls);
    std::thread::sleep(Duration::from_millis(300));
    let (status, _, body) = raw_chunked_upload(
        server.port,
        &format!(
            "/ui/attachments?workspaceId={workspace}&entityName=Alice&filename=after-disconnect.txt"
        ),
        "text/plain",
        &[b"abc".to_vec()],
    );
    assert_eq!(
        status,
        201,
        "abandoned uploads must release their spool capacity: {}",
        String::from_utf8_lossy(&body)
    );
}

/// A stalled upload holds its spool permit only until the idle timeout.
/// The slot must free by itself, without a disconnect, so four idle
/// trickles cannot starve every upload forever.
#[tokio::test]
async fn idle_upload_times_out_and_releases_its_spool_permit() {
    let server = spawn_child_server();
    let created = mcp_call(
        server.port,
        r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"create_workspace","arguments":{"name":"fixture","visibility":"private"}},"id":1}"#,
    );
    let workspace = created["result"]["workspace"]["workspaceId"]
        .as_str()
        .expect("the workspace id")
        .to_owned();
    let seeded = mcp_call(
        server.port,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"name":"create_entities","arguments":{{"workspaceId":"{workspace}","entities":[{{"name":"Alice","entityType":"person","observations":[]}}]}}}},"id":2}}"#
        ),
    );
    assert!(
        seeded.get("error").is_none() && seeded["result"]["isError"].as_bool() != Some(true),
        "seeding failed: {seeded}"
    );

    // One stalled upload holds a permit; the server answers 408 when the
    // next frame does not arrive inside SPOOL_IDLE_TIMEOUT.
    let mut stall = open_stalled_upload(
        server.port,
        &format!("/ui/attachments?workspaceId={workspace}&entityName=Alice&filename=idle.txt"),
        "text/plain",
    );
    let mut reply = Vec::new();
    let mut buf = [0_u8; 1024];
    let start = std::time::Instant::now();
    loop {
        match stall.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => reply.extend_from_slice(&buf[..n]),
        }
        if start.elapsed() > Duration::from_secs(30) {
            break;
        }
    }
    let reply = String::from_utf8_lossy(&reply);
    assert!(
        reply.contains("408"),
        "an idle upload must time out with HTTP 408: {reply}"
    );
    assert!(
        reply.contains("idle timeout"),
        "the timeout must name the idle rule: {reply}"
    );

    // The permit is free again: a complete upload succeeds at once.
    std::thread::sleep(Duration::from_millis(300));
    let (status, _, body) = raw_chunked_upload(
        server.port,
        &format!(
            "/ui/attachments?workspaceId={workspace}&entityName=Alice&filename=after-idle.txt"
        ),
        "text/plain",
        &[b"abc".to_vec()],
    );
    assert_eq!(
        status,
        201,
        "an idle upload must release its spool capacity: {}",
        String::from_utf8_lossy(&body)
    );
}

/// Send one chunk of a chunked upload and keep the connection open with no
/// terminating chunk, so the handler parks holding its spool and permit.
fn open_stalled_upload(port: u16, path: &str, mime: &str) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer attachment-http-test-bearer\r\nContent-Type: {mime}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).unwrap();
    write!(&mut stream, "1\r\nx\r\n").unwrap();
    stream.flush().unwrap();
    stream
}
