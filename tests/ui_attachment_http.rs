//! The attachment adapter contract at the `/ui/api/attachments` base.
//!
//! Task 4 moved the attachment handlers from the legacy `/ui/*` paths to
//! this base with unchanged semantics. This file pins the moved contract:
//! the raw-body upload protocol, the gates, and the measured response
//! fields. The green suite in `attachment_http.rs` pins the same behavior
//! through the real transport; this file pins the contract on the router.
//!
//! The upload is a raw-body `POST`. The query carries `workspaceId`,
//! `entityName`, and `filename`. The `Content-Type` header carries the MIME
//! type. There is no multipart adapter.

#![cfg(feature = "oauth")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{Body, Bytes};
use axum::http::{Request, Response, StatusCode, header};
use futures::stream;
use http_body_util::BodyExt;
use mcpmem::principals::PrincipalEntry;
use mcpmem::tools::ToolCategory;
use mcpmem_oauth::store::{Grant, Store, TokenKind};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

mod support;

/// The measured fields of one attachment row. The plan names them: the
/// `status` values, `errorStage`, and `pageCount`. A new field changes this
/// list, and the tests below assert the list exactly.
const ATTACHMENT_FIELDS: [&str; 9] = [
    "attachmentId",
    "errorStage",
    "filename",
    "lastError",
    "mime",
    "pageCount",
    "revision",
    "sizeBytes",
    "status",
];

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

async fn fixture() -> Fixture {
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
    let server = support::server(
        Some(oauth.clone()),
        support::Scopes {
            bearer: Vec::new(),
            enabled: ToolCategory::ALL.to_vec(),
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
    format!("/ui/api/attachments?workspaceId={ws}&entityName={entity}&filename={filename}")
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

/// The status and the body text of one response. The gate tests print the
/// body in the assertion message, so a wrong status names its reason.
async fn status_and_text(response: Response<Body>) -> (StatusCode, String) {
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
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

/// A body whose first poll records itself. The gates must reject the request
/// before the spool reads the body, so a rejected upload leaves this count
/// at zero.
fn never_read_body(count: Arc<AtomicUsize>) -> Body {
    let polled = Arc::clone(&count);
    Body::from_stream(stream::once(async move {
        polled.fetch_add(1, Ordering::SeqCst);
        Ok::<Bytes, std::io::Error>(Bytes::from_static(b"never read"))
    }))
}

/// The exact key set of a JSON object. A response with an extra or a missing
/// field fails the equality below; the measured-fields rule is exact.
fn field_set(object: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = object
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// A raw upload body with `filename` and `entityName` in the query and the
/// MIME type in `Content-Type` returns status `uploaded`. The list returns
/// the row. The metadata and the download return the stored facts.
#[tokio::test]
async fn upload_list_metadata_and_download_at_the_api_base() {
    let fx = fixture().await;
    let post = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "hello.txt"),
        Body::from("hello"),
        Some("text/plain"),
    )
    .await;
    assert_eq!(post.status(), StatusCode::CREATED);
    let body = data(post).await;
    let id = body["attachmentId"].as_i64().expect("an attachment id");
    assert_eq!(
        body,
        json!({"attachmentId": id, "status": "uploaded"}),
        "the upload reply carries exactly the measured fields"
    );
    let expected_row = json!({
        "attachmentId": id,
        "filename": "hello.txt",
        "mime": "text/plain",
        "sizeBytes": 5,
        "status": "uploaded",
        "revision": 1,
        "errorStage": Value::Null,
        "lastError": Value::Null,
        "pageCount": 0,
    });

    let list = send(
        &fx.server,
        &fx.owner,
        "GET",
        &format!(
            "/ui/api/attachments?workspaceId={}&entityName=Alice",
            fx.workspace
        ),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(list.status(), StatusCode::OK);
    let list = data(list).await;
    assert_eq!(list["attachments"], json!([expected_row]));
    assert_eq!(
        field_set(&list["attachments"][0]),
        ATTACHMENT_FIELDS,
        "the list row exposes exactly the measured fields"
    );

    let metadata = send(
        &fx.server,
        &fx.owner,
        "GET",
        &format!("/ui/api/attachments/{id}?workspaceId={}", fx.workspace),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(metadata.status(), StatusCode::OK);
    let metadata = data(metadata).await;
    let mut expected_metadata = expected_row;
    expected_metadata["entityName"] = json!("Alice");
    assert_eq!(metadata, expected_metadata);
    let mut expected_keys: Vec<&str> = ATTACHMENT_FIELDS.to_vec();
    expected_keys.push("entityName");
    expected_keys.sort_unstable();
    assert_eq!(
        field_set(&metadata),
        expected_keys,
        "the metadata exposes exactly the measured fields and the entity"
    );

    let download = send(
        &fx.server,
        &fx.owner,
        "GET",
        &format!(
            "/ui/api/attachments/{id}/download?workspaceId={}",
            fx.workspace
        ),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(download.headers()[header::CONTENT_TYPE], "text/plain");
    assert_eq!(
        download.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"hello.txt\""
    );
    let bytes = download.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"hello", "the download returns the stored bytes");
}

/// The MIME type outside the allowlist rejects the upload. The rejection
/// names the MIME rule and stores no row. The HTTP code is 415
/// (`UNSUPPORTED_MEDIA_TYPE`), the semantically correct code for a refused
/// media type; the plan text in task-10-brief.md that says 400 is stale.
#[tokio::test]
async fn a_mime_outside_the_allowlist_rejects_and_names_the_rule() {
    let fx = fixture().await;
    let response = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "x.bin"),
        Body::from("bad"),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let body = data(response).await;
    assert!(
        body["error"].as_str().unwrap().contains("MIME"),
        "the rejection names the MIME rule: {body}"
    );
    assert_eq!(attachment_count(&fx), 0, "no row survives the rejection");
}

/// A byte count of 52,428,801 rejects with 413. The advertised
/// `Content-Length` trips the cap before the handler reads any body byte, so
/// this test reads no body. The green suite pins the acceptance of exactly
/// 52,428,800 bytes.
#[tokio::test]
async fn one_byte_above_the_cap_rejects_with_413_and_reads_no_body() {
    let fx = fixture().await;
    let count = Arc::new(AtomicUsize::new(0));
    let request = Request::post(upload_path(&fx.workspace, "Alice", "too-large.txt"))
        .header(header::AUTHORIZATION, format!("Bearer {}", fx.owner))
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::CONTENT_LENGTH, "52428801")
        .body(never_read_body(Arc::clone(&count)))
        .unwrap();
    let response = fx.server.request(request).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = data(response).await;
    assert!(
        body["error"].as_str().unwrap().contains("per-file"),
        "the rejection names the per-file limit: {body}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "the advertised oversize reads no body byte"
    );
    assert_eq!(attachment_count(&fx), 0);
}

/// The workspace byte budget gates the upload. The fixture stores five rows
/// near the default budget and leaves a headroom of 56 bytes; an upload of
/// 100 bytes must then exceed the budget. The rejection names the budget in
/// the error body. The green suite pins the same "budget" message through
/// the MCP tools. The plan text in task-10-brief.md that says 409 is stale;
/// the adapter groups `Size` and `WorkspaceBudget` under 413.
#[tokio::test]
async fn workspace_budget_exhaustion_rejects_the_upload() {
    let fx = fixture().await;
    let conn = graph(&fx);
    let entity_id: i64 = conn
        .query_row("SELECT id FROM entity WHERE name='Alice'", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut seed = conn
        .prepare(
            "INSERT INTO attachment(entity_id,filename,mime,size_bytes,sha256,content,status,revision,last_error,error_stage,created_us)
             VALUES(?1,?2,'text/plain',?3,zeroblob(32),zeroblob(?3),'uploaded',1,NULL,NULL,1)",
        )
        .unwrap();
    // Five near-cap rows (5 x 53,687,080 = 268,435,400) leave 56 bytes of
    // the default 268,435,456-byte budget.
    for index in 0..5 {
        seed.execute(params![entity_id, format!("seed-{index}.txt"), 53_687_080_i64])
            .unwrap();
    }
    drop(seed);
    drop(conn);

    let response = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "over-budget.txt"),
        Body::from(vec![b'x'; 100]),
        Some("text/plain"),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "the budget rejection is a 413 client error"
    );
    let body = data(response).await;
    assert!(
        body["error"].as_str().unwrap().contains("budget"),
        "the rejection names the budget rule: {body}"
    );
    assert_eq!(
        attachment_count(&fx),
        5,
        "the rejected upload stores no new row"
    );
}

/// The upload needs the `attachments` scope and a workspace write grant.
/// The reader holds the scope and a read grant, so its upload answers the
/// same 404 as a denied workspace; the token without the scope answers 403
/// and names the scope. Both gates reject before the handler reads the body.
/// The reader can list with its read grant. The plan text in
/// task-10-brief.md that says the reader upload answers 403 is stale: the
/// denied-workspace rule answers 404, and the green suite pins it.
#[tokio::test]
async fn a_reader_cannot_upload_and_a_token_without_scope_is_denied() {
    let fx = fixture().await;
    let count = Arc::new(AtomicUsize::new(0));
    let denied = send(
        &fx.server,
        &fx.reader,
        "POST",
        &upload_path(&fx.workspace, "Alice", "denied.txt"),
        never_read_body(Arc::clone(&count)),
        Some("text/plain"),
    )
    .await;
    assert_eq!(
        denied.status(),
        StatusCode::NOT_FOUND,
        "a denied workspace hides the row: {}",
        data(denied).await
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "the reader gate rejects before the body reads"
    );

    let count = Arc::new(AtomicUsize::new(0));
    let unscoped = send(
        &fx.server,
        &fx.writer_without_scope,
        "POST",
        &upload_path(&fx.workspace, "Alice", "denied.txt"),
        never_read_body(Arc::clone(&count)),
        Some("text/plain"),
    )
    .await;
    assert_eq!(unscoped.status(), StatusCode::FORBIDDEN);
    assert!(
        unscoped.headers()[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .contains("attachments"),
        "the scope rejection names the attachments scope"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "the gate rejects before the body reads"
    );
    assert_eq!(attachment_count(&fx), 0);

    let list = send(
        &fx.server,
        &fx.reader,
        "GET",
        &format!(
            "/ui/api/attachments?workspaceId={}&entityName=Alice",
            fx.workspace
        ),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(list.status(), StatusCode::OK, "the read grant serves the list");
}

/// An unknown workspace answers 404 on every route and hides the stored
/// rows. The unknown id is a valid UUID with no workspace row: the registry
/// reads the row and reports `NotFound`. The upload does not read the body.
/// The delete with the wrong workspace leaves the row untouched.
#[tokio::test]
async fn an_unknown_workspace_returns_the_same_404_and_hides_existence() {
    let fx = fixture().await;
    let unknown = "00000000-0000-0000-0000-000000000000";
    let count = Arc::new(AtomicUsize::new(0));
    let upload = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(unknown, "Alice", "ghost.txt"),
        never_read_body(Arc::clone(&count)),
        Some("text/plain"),
    )
    .await;
    let (status, body) = status_and_text(upload).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "upload: {body}");
    assert_eq!(count.load(Ordering::SeqCst), 0, "the gate rejects first");
    assert_eq!(attachment_count(&fx), 0);

    let list = send(
        &fx.server,
        &fx.owner,
        "GET",
        &format!(
            "/ui/api/attachments?workspaceId={unknown}&entityName=Alice"
        ),
        Body::empty(),
        None,
    )
    .await;
    let (status, body) = status_and_text(list).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "list: {body}");

    let uploaded = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "real.txt"),
        Body::from("real"),
        Some("text/plain"),
    )
    .await;
    let id = data(uploaded).await["attachmentId"].as_i64().unwrap();
    let deleted = send(
        &fx.server,
        &fx.owner,
        "DELETE",
        &format!("/ui/api/attachments/{id}?workspaceId={unknown}"),
        Body::empty(),
        None,
    )
    .await;
    let (status, body) = status_and_text(deleted).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "delete: {body}");
    assert_eq!(
        attachment_count(&fx),
        1,
        "the wrong workspace cannot delete the row"
    );
    let deleted = send(
        &fx.server,
        &fx.owner,
        "DELETE",
        &format!("/ui/api/attachments/{id}?workspaceId={}", fx.workspace),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(attachment_count(&fx), 0);
}

/// The metadata passes the stored status domain through untouched. The
/// extractor writes `extracting`, `ready`, and `error` with an `errorStage`;
/// this test moves one row through all four values and reads each back. The
/// `pageCount` counts the stored page rows. No response adds an invented
/// index state or a chunk count.
#[tokio::test]
async fn metadata_passes_the_status_domain_and_page_count_through() {
    let fx = fixture().await;
    let uploaded = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "status.txt"),
        Body::from("abc"),
        Some("text/plain"),
    )
    .await;
    let id = data(uploaded).await["attachmentId"].as_i64().unwrap();
    let conn = graph(&fx);
    let meta = |id: i64| {
        let fx = &fx;
        async move {
            let response = send(
                &fx.server,
                &fx.owner,
                "GET",
                &format!("/ui/api/attachments/{id}?workspaceId={}", fx.workspace),
                Body::empty(),
                None,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            data(response).await
        }
    };

    conn.execute("UPDATE attachment SET status='extracting' WHERE id=?1", [id])
        .unwrap();
    let extracting = meta(id).await;
    assert_eq!(extracting["status"], "extracting");
    assert_eq!(extracting["errorStage"], Value::Null);
    assert_eq!(
        field_set(&extracting),
        {
            let mut keys: Vec<&str> = ATTACHMENT_FIELDS.to_vec();
            keys.push("entityName");
            keys.sort_unstable();
            keys
        },
        "transient states carry the same measured fields"
    );

    conn.execute(
        "UPDATE attachment SET status='error',error_stage='decode',
          last_error='invalid UTF-8' WHERE id=?1",
        [id],
    )
    .unwrap();
    let error = meta(id).await;
    assert_eq!(error["status"], "error");
    assert_eq!(error["errorStage"], "decode");
    assert_eq!(error["lastError"], "invalid UTF-8");

    conn.execute(
        "INSERT INTO attachment_text(attachment_id,page,text,chars) VALUES(?1,1,'ok',2)",
        [id],
    )
    .unwrap();
    conn.execute(
        "UPDATE attachment SET status='ready',revision=2,
          error_stage=NULL,last_error=NULL WHERE id=?1",
        [id],
    )
    .unwrap();
    let ready = meta(id).await;
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["revision"], 2);
    assert_eq!(
        ready["pageCount"], 1,
        "pageCount counts the stored extracted pages"
    );
    assert_eq!(ready["errorStage"], Value::Null);
}

/// Delete removes the row, its job, and its bytes: the count falls to zero
/// and the same filename uploads again without a duplicate conflict. The
/// freed name is the proof that the row and its blob are gone.
#[tokio::test]
async fn delete_removes_the_row_job_and_bytes() {
    let fx = fixture().await;
    let uploaded = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "gone.txt"),
        Body::from("bytes"),
        Some("text/plain"),
    )
    .await;
    let id = data(uploaded).await["attachmentId"].as_i64().unwrap();
    assert_eq!(attachment_count(&fx), 1);

    let deleted = send(
        &fx.server,
        &fx.owner,
        "DELETE",
        &format!("/ui/api/attachments/{id}?workspaceId={}", fx.workspace),
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(attachment_count(&fx), 0);
    let jobs: i64 = graph(&fx)
        .query_row("SELECT count(*) FROM attachment_job", [], |row| row.get(0))
        .unwrap();
    assert_eq!(jobs, 0, "the delete removes the published job rows");

    let fresh = send(
        &fx.server,
        &fx.owner,
        "POST",
        &upload_path(&fx.workspace, "Alice", "gone.txt"),
        Body::from("again"),
        Some("text/plain"),
    )
    .await;
    assert_eq!(
        fresh.status(),
        StatusCode::CREATED,
        "the freed name and bytes accept a new upload"
    );
}

/// The upload protocol names its inputs: `filename`, `entityName`, and the
/// `Content-Type` header. A missing input answers 400 and names the input.
#[tokio::test]
async fn uploads_require_filename_entity_and_content_type() {
    let fx = fixture().await;
    let cases = [
        (
            format!(
                "/ui/api/attachments?workspaceId={}&entityName=Alice",
                fx.workspace
            ),
            Some("text/plain"),
            "missing 'filename' parameter",
        ),
        (
            format!(
                "/ui/api/attachments?workspaceId={}&filename=named.txt",
                fx.workspace
            ),
            Some("text/plain"),
            "missing 'entityName' parameter",
        ),
        (
            upload_path(&fx.workspace, "Alice", "named.txt"),
            None,
            "missing Content-Type header",
        ),
    ];
    for (path, mime, expected) in cases {
        let response = send(&fx.server, &fx.owner, "POST", &path, Body::from("x"), mime).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        let body = data(response).await;
        assert!(
            body["error"].as_str().unwrap().contains(expected),
            "{path}: {body}"
        );
    }
    assert_eq!(attachment_count(&fx), 0);
}