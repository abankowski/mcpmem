//! The attachment adapters under `/ui/api/attachments*`: list, upload,
//! metadata, page text, download, and delete.
//!
//! Every route resolves the category, the credential, the `attachments`
//! scope, and the workspace grant before it touches a file or reads a body.
//! The raw upload and blob download move at most [`ATTACHMENT_STREAM_CHUNK`]
//! bytes per spool or SQLite operation; the MCP JSON-RPC body limit is
//! unchanged.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::StreamExt;
use mcpmem_core::attachments::{AttachmentError, AttachmentLimits, AttachmentRepository};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::error;

use crate::http::{
    HttpState, bad_request, not_found, principal_of, ui_error, ui_insufficient_scope,
    ui_unauthorized, workspace_failure,
};
use crate::server::attachments_enabled;
use crate::workspace::{WorkspaceAccess, WorkspaceError};

/// The raw upload and blob download move at most this many bytes per spool or
/// SQLite operation.
const ATTACHMENT_STREAM_CHUNK: usize = 64 * 1024;

/// How long one spool may wait for the next body frame. A client trickling
/// a frame slower than this holds its spool permit without progress, so an
/// idle upload releases its slot; the HTTP answer is 408 and the client
/// retries later.
const SPOOL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Register the attachment routes. The upload body is read as a raw stream,
/// so the route carries no body-size cap of its own; the per-file byte cap
/// is enforced while spooling.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router
        .route(
            "/ui/api/attachments",
            get(list_attachments_handler)
                .post(post_attachment_handler)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/ui/api/attachments/{id}",
            get(get_attachment_handler).delete(delete_attachment_handler),
        )
        .route(
            "/ui/api/attachments/{id}/pages",
            get(get_attachment_page_handler),
        )
        .route(
            "/ui/api/attachments/{id}/download",
            get(download_attachment_handler),
        )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentView {
    attachment_id: i64,
    filename: String,
    mime: String,
    size_bytes: i64,
    status: String,
    revision: i64,
    error_stage: Option<String>,
    last_error: Option<String>,
    page_count: Option<i64>,
}

fn attachment_row(
    conn: &Connection,
    id: i64,
    entity_name: bool,
) -> std::result::Result<(AttachmentView, Option<String>), Box<Response>> {
    let entity_select = if entity_name { "e.name" } else { "NULL" };
    let (view, entity) = conn
        .query_row(
            &format!(
                "SELECT a.id,a.filename,a.mime,a.size_bytes,a.status,a.revision,
                        a.error_stage,a.last_error,
                        (SELECT count(*) FROM attachment_text t WHERE t.attachment_id=a.id),
                        {entity_select}
                 FROM attachment a
                 JOIN entity e ON e.id=a.entity_id
                 JOIN entity_revision r ON r.entity_id=e.id
                 WHERE a.id=?1 AND e.flags=0 AND r.deleted=0"
            ),
            [id],
            |row| {
                Ok((
                    AttachmentView {
                        attachment_id: row.get(0)?,
                        filename: row.get(1)?,
                        mime: row.get(2)?,
                        size_bytes: row.get(3)?,
                        status: row.get(4)?,
                        revision: row.get(5)?,
                        error_stage: row.get(6)?,
                        last_error: row.get(7)?,
                        page_count: if entity_name { Some(row.get(8)?) } else { None },
                    },
                    row.get(9)?,
                ))
            },
        )
        .optional()
        .map_err(|error| attachment_db_error(&error))?
        .ok_or_else(|| Box::new(not_found()))?;
    let entity = if entity_name { entity } else { None };
    Ok((view, entity))
}

/// `GET /ui/api/attachments` — the metadata rows of one entity's
/// attachments, newest first, in the same shape the MCP list tool returns.
async fn list_attachments_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let entity_name = match params.get("entityName").filter(|s| !s.is_empty()) {
        Some(name) => name.clone(),
        None => return bad_request("missing 'entityName' parameter"),
    };
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "list_attachments",
        WorkspaceAccess::Read,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let limit = params
        .get("limit")
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, 1000);
    match attachment_result(path, move |conn| {
        let mut rows = conn
            .prepare(
                "SELECT a.id,a.filename,a.mime,a.size_bytes,a.status,a.revision,
                        a.error_stage,a.last_error,
                        (SELECT count(*) FROM attachment_text t WHERE t.attachment_id=a.id)
                 FROM attachment a
                 JOIN entity e ON e.id=a.entity_id
                 JOIN entity_revision r ON r.entity_id=e.id
                 WHERE e.name=?1 AND e.flags=0 AND r.deleted=0
                 ORDER BY a.id DESC LIMIT ?2",
            )
            .map_err(|error| attachment_db_error(&error))?;
        let rows = rows
            .query_map(params![entity_name, limit], |row| {
                Ok(AttachmentView {
                    attachment_id: row.get(0)?,
                    filename: row.get(1)?,
                    mime: row.get(2)?,
                    size_bytes: row.get(3)?,
                    status: row.get(4)?,
                    revision: row.get(5)?,
                    error_stage: row.get(6)?,
                    last_error: row.get(7)?,
                    page_count: row.get(8)?,
                })
            })
            .map_err(|error| attachment_db_error(&error))?;
        let attachments = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| attachment_db_error(&error))?;
        Ok(Json(json!({ "attachments": attachments })))
    })
    .await
    {
        Ok(response) => response.into_response(),
        Err(response) => *response,
    }
}

/// `GET /ui/api/attachments/{id}` — one attachment's metadata plus the entity
/// it belongs to.
async fn get_attachment_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "get_attachment",
        WorkspaceAccess::Read,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let id = match attachment_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    match attachment_result(path, move |conn| {
        let (view, entity_name) = attachment_row(conn, id, true)?;
        let mut payload = serde_json::to_value(view).map_err(|error| {
            error!("attachment metadata serialize error: {error}");
            Box::new(ui_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error",
            ))
        })?;
        payload["entityName"] = serde_json::Value::String(entity_name.unwrap_or_default());
        Ok(Json(payload))
    })
    .await
    {
        Ok(response) => response.into_response(),
        Err(response) => *response,
    }
}

/// `GET /ui/api/attachments/{id}/pages` — one bounded span of one extracted
/// page, addressed by Unicode-scalar offsets.
async fn get_attachment_page_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "get_attachment_page",
        WorkspaceAccess::Read,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let id = match attachment_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    let page = match attachment_query_number(&params, "page", None, 1) {
        Ok(page) => page,
        Err(response) => return *response,
    };
    let offset = match attachment_query_number(&params, "offset", Some(0), 0) {
        Ok(offset) => offset,
        Err(response) => return *response,
    };
    let limit = match params
        .get("maxChars")
        .and_then(|raw| raw.parse::<i64>().ok())
    {
        Some(limit) if (1..=4096).contains(&limit) => limit,
        Some(_) => return bad_request("'maxChars' must be between 1 and 4096"),
        None => 4096,
    };
    attachment_result(path, move |conn| {
        let text: Option<String> = conn
            .query_row(
                "SELECT t.text FROM attachment_text t
                 JOIN attachment a ON a.id=t.attachment_id
                 JOIN entity e ON e.id=a.entity_id
                 JOIN entity_revision r ON r.entity_id=e.id
                 WHERE t.attachment_id=?1 AND t.page=?2
                   AND e.flags=0 AND r.deleted=0",
                [id, page],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| attachment_db_error(&error))?;
        let Some(text) = text else {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(json!({ "code": "not_found", "message": "no such page" })),
            ));
        };
        let start: usize = text.chars().take(offset as usize).map(char::len_utf8).sum();
        let rest = &text[start..];
        let remaining = rest.chars().count();
        let taken = remaining.min(limit as usize);
        let end = rest
            .char_indices()
            .nth(taken)
            .map_or(text.len(), |(index, _)| start + index);
        let response = json!({
            "page": page,
            "text": &text[start..end],
            "offset": offset,
            "nextOffset": offset + taken as i64,
            "eof": taken == remaining,
        });
        Ok((StatusCode::OK, Json(response)))
    })
    .await
    .map_or_else(
        |response| *response,
        |(status, body)| (status, body).into_response(),
    )
}

/// Read one bounded span of a stored blob through SQLite's incremental blob
/// I/O, so a download never materializes the whole file in Rust memory.
fn read_blob_chunk(
    conn: &Connection,
    row_id: i64,
    offset: i64,
    amount: usize,
) -> std::result::Result<Option<Vec<u8>>, Box<Response>> {
    let blob = conn
        .blob_open("main", "attachment", "content", row_id, true)
        .map_err(|error| attachment_db_error(&error))?;
    if offset < 0 || offset as usize >= blob.len() {
        return Ok(None);
    }
    let at = offset as usize;
    let take = (blob.len() - at).min(amount);
    let mut buffer = vec![0_u8; take];
    blob.read_at(&mut buffer, at)
        .map_err(|error| attachment_db_error(&error))?;
    Ok(Some(buffer))
}

/// A download filename is response-header material: strip the bytes that can
/// split or corrupt the header (line breaks, quotes, backslashes, control
/// characters). A name reduced to nothing falls back to the bare directive.
fn download_disposition(filename: &str) -> HeaderValue {
    let clean: String = filename
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    match HeaderValue::from_str(&format!("attachment; filename=\"{clean}\"")) {
        Ok(value) => value,
        Err(_) => HeaderValue::from_static("attachment"),
    }
}

/// `GET /ui/api/attachments/{id}/download` — the stored bytes, streamed in
/// bounded pieces straight out of the SQLite blob. Same `attachments` scope
/// and workspace read access as the other read routes; the MIME type and the
/// safe filename come from the stored row.
async fn download_attachment_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "read_attachment_chunk",
        WorkspaceAccess::Read,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let id = match attachment_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    // The download stream reads the blob after the metadata query. Run the
    // metadata query through `attachment_result`, which bootstraps a
    // pre-upgrade workspace before any attachment SQL; the stream then opens
    // a fresh read connection on the migrated graph.
    let (mime, filename, size) = match attachment_result(path.clone(), move |conn| {
        let row = conn
            .query_row(
                "SELECT a.mime,a.filename,a.size_bytes
                 FROM attachment a
                 JOIN entity e ON e.id=a.entity_id
                 JOIN entity_revision r ON r.entity_id=e.id
                 WHERE a.id=?1 AND e.flags=0 AND r.deleted=0",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| attachment_db_error(&error))?
            .ok_or_else(|| Box::new(not_found()))?;
        Ok::<_, Box<Response>>(row)
    })
    .await
    {
        Ok(row) => row,
        Err(response) => return *response,
    };
    let conn = match Connection::open(path)
        .and_then(|conn| {
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            Ok(conn)
        })
        .map_err(|error| attachment_db_error(&error))
    {
        Ok(conn) => conn,
        Err(response) => return *response,
    };
    let content_type = HeaderValue::from_str(&mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    // `unfold` panics when a consumer polls it after it returned
    // `Ready(None)`, and the response-compression layer does exactly that
    // once when it finalizes an encoded body. A client that advertises
    // `Accept-Encoding` therefore killed the connection without a response
    // (`ERR_EMPTY_RESPONSE` in the browser, nothing in the log file) on the
    // final chunk of every download. Fusing the stream turns that post-end
    // poll into a no-op: the stream stops forwarding polls once it is done,
    // and the bytes stream exactly as before.
    let stream = futures::stream::unfold(Some((conn, id, 0_i64, size)), |state| async move {
        let (conn, row_id, offset, size) = state?;
        if offset >= size {
            return None;
        }
        let chunk = match read_blob_chunk(&conn, row_id, offset, ATTACHMENT_STREAM_CHUNK) {
            Ok(Some(bytes)) => bytes,
            Ok(None) | Err(_) => {
                return Some((
                    Err::<Bytes, std::io::Error>(std::io::Error::other("attachment read failed")),
                    None,
                ));
            }
        };
        let next_offset = offset + chunk.len() as i64;
        let next_state = (next_offset < size).then_some((conn, row_id, next_offset, size));
        Some((Ok::<Bytes, std::io::Error>(Bytes::from(chunk)), next_state))
    })
    .fuse();
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, download_disposition(&filename)),
            (
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&size.to_string())
                    .unwrap_or_else(|_| HeaderValue::from_static("0")),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// `DELETE /ui/api/attachments/{id}` — remove the attachment and every row
/// it published (page text, segments, jobs, vectors) in one graph
/// transaction. Requires the `attachments` scope and workspace write access.
async fn delete_attachment_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "delete_attachment",
        WorkspaceAccess::Write,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let id = match attachment_id(&id) {
        Ok(id) => id,
        Err(response) => return *response,
    };
    match attachment_result(path, move |conn| {
        AttachmentRepository::new(conn)
            .delete_attachment(id)
            .map(|()| StatusCode::NO_CONTENT)
            .map_err(attachment_failure)
    })
    .await
    {
        Ok(status) => status.into_response(),
        Err(response) => *response,
    }
}

/// Stream a raw upload body through one anonymous spool file, counting and
/// hashing every byte. [`tempfile::tempfile`] creates the file already
/// unlinked, so a dropped request, an error, or a panic removes the spool
/// with the handle — no path survives. The per-frame copy keeps a full file
/// out of Rust memory; the byte counter alone bounds the upload. An empty
/// body spools as a valid zero-byte file: the repository stores it exactly
/// like the MCP contract's `expectedBytes == 0` upload.
async fn spool_body(
    body: Body,
    limits: &AttachmentLimits,
) -> std::result::Result<(std::fs::File, i64, [u8; 32]), Box<Response>> {
    let mut spool = match tempfile::tempfile() {
        Ok(spool) => spool,
        Err(error) => {
            error!("attachment spool create error: {error}");
            return Err(Box::new(ui_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error",
            )));
        }
    };
    let mut count = 0_i64;
    let mut hasher = Sha256::new();
    let mut frames = Box::pin(body.into_data_stream());
    while let Some(frame) = match tokio::time::timeout(SPOOL_IDLE_TIMEOUT, frames.next()).await {
        Ok(frame) => frame,
        Err(_) => {
            // An idle body must release its spool permit, or four slow
            // trickles would hold every slot forever and block all uploads.
            return Err(Box::new(ui_error(
                StatusCode::REQUEST_TIMEOUT,
                "request_timeout",
                "attachment upload idle timeout; retry later",
            )));
        }
    } {
        let bytes = match frame {
            Ok(bytes) => bytes,
            Err(error) => {
                error!("attachment upload stream error: {error}");
                return Err(Box::new(ui_error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "attachment upload stream failed",
                )));
            }
        };
        count = match count.checked_add(bytes.len() as i64) {
            Some(count) => count,
            None => {
                return Err(Box::new(ui_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    "attachment exceeds the per-file size limit",
                )));
            }
        };
        if count > limits.max_bytes {
            return Err(Box::new(ui_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "attachment exceeds the per-file size limit",
            )));
        }
        let mut rest = bytes.as_ref();
        while !rest.is_empty() {
            let amount = ATTACHMENT_STREAM_CHUNK.min(rest.len());
            if let Err(error) = spool.write_all(&rest[..amount]) {
                error!("attachment spool write error: {error}");
                return Err(Box::new(ui_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "internal error",
                )));
            }
            hasher.update(&rest[..amount]);
            rest = &rest[amount..];
        }
    }
    if let Err(error) = spool.seek(SeekFrom::Start(0)) {
        error!("attachment spool seek error: {error}");
        return Err(Box::new(ui_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "internal error",
        )));
    }
    Ok((spool, count, hasher.finalize().into()))
}

/// Replay a spool one bounded chunk at a time. The file stays unlinked while
/// the repository reads it inside its transaction, so a failed commit leaves
/// no graph row and no spool path behind.
struct SpoolReader {
    file: std::fs::File,
}

impl Read for SpoolReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(out)
    }
}

impl SpoolReader {
    const fn new(spool: std::fs::File) -> Self {
        Self { file: spool }
    }
}

/// `POST /ui/api/attachments` — store a raw upload body against one entity.
/// The query params carry the workspace, the entity, and the filename; the
/// MIME type is the request `Content-Type`. Requires the `attachments`
/// scope and workspace write access.
async fn post_attachment_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: Body,
) -> Response {
    let filename = match params.get("filename").filter(|s| !s.is_empty()) {
        Some(filename) => filename.clone(),
        None => return bad_request("missing 'filename' parameter"),
    };
    let entity_name = match params.get("entityName").filter(|s| !s.is_empty()) {
        Some(name) => name.clone(),
        None => return bad_request("missing 'entityName' parameter"),
    };
    let mime = match headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        Some(mime) if !mime.is_empty() => mime.to_owned(),
        _ => return bad_request("missing Content-Type header"),
    };
    let path = match attachment_path(
        &state,
        &headers,
        &params,
        "begin_attachment_upload",
        WorkspaceAccess::Write,
    )
    .await
    {
        Ok(path) => path,
        Err(response) => return *response,
    };
    if let Some(length) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        && length > state.attachment_limits.max_bytes
    {
        return ui_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "attachment exceeds the per-file size limit",
        );
    }
    // One permit per in-flight spool, held until the storing transaction
    // finishes. A full bound rejects the upload before it opens a spool file,
    // so many partial chunked uploads cannot stack arbitrary temporary disk.
    let _spool = match state.attachment_spools.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return ui_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "attachment spool capacity is full; retry later",
            );
        }
    };
    let (spool, count, digest) = match spool_body(body, &state.attachment_limits).await {
        Ok(spooled) => spooled,
        Err(response) => return *response,
    };
    let limits = Arc::clone(&state.attachment_limits);
    let entity_name_copy = entity_name.clone();
    match attachment_result(path, move |conn| {
        let entity_id = conn
            .query_row(
                "SELECT e.id FROM entity e
                 JOIN entity_revision r ON r.entity_id=e.id
                 WHERE e.name=?1 AND e.flags=0 AND r.deleted=0",
                [entity_name_copy],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| attachment_db_error(&error))?;
        let Some(entity_id) = entity_id else {
            return Err(Box::new(not_found()));
        };
        let mut reader = SpoolReader::new(spool);
        let attachment_id = AttachmentRepository::new(conn)
            .store_reader(
                entity_id,
                &filename,
                &mime,
                &mut reader,
                count,
                &digest,
                &limits,
                mcpmem_core::events::now_us(),
            )
            .map_err(attachment_failure)?;
        Ok(Json(json!({
            "attachmentId": attachment_id,
            "status": "uploaded",
        })))
    })
    .await
    {
        Ok(response) => (StatusCode::CREATED, response).into_response(),
        Err(response) => *response,
    }
}

/// What the blocking gate finished with, spelled out so the match in
/// [`attachment_path`] never has to guess at nested result types.
enum AttachmentPath {
    Path(PathBuf),
    Unauthorized,
    MissingScope(&'static str),
    Workspace(WorkspaceError),
}

/// The category, credential, scope, and workspace grant are independent gates.
/// Finish all four before opening a temporary file or reading an upload body.
async fn attachment_path(
    state: &HttpState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
    tool: &'static str,
    access: WorkspaceAccess,
) -> std::result::Result<PathBuf, Box<Response>> {
    if !attachments_enabled() {
        return Err(Box::new(ui_error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "attachments tools are disabled",
        )));
    }
    let auth = state.clone();
    let headers = headers.clone();
    let registry = Arc::clone(&state.registry);
    let requested = params.get("workspaceId").cloned();
    let outcome = tokio::task::spawn_blocking(move || {
        let Some(principal) = principal_of(&auth, &headers) else {
            return AttachmentPath::Unauthorized;
        };
        if let Some(missing) = crate::authz::missing_scope(&principal, tool) {
            return AttachmentPath::MissingScope(missing);
        }
        match registry.resolve(&principal.id, requested.as_deref(), access) {
            Ok(record) => AttachmentPath::Path(record.graph_path),
            Err(error) => AttachmentPath::Workspace(error),
        }
    })
    .await;
    match outcome {
        Ok(AttachmentPath::Path(path)) => Ok(path),
        Ok(AttachmentPath::Unauthorized) => Err(Box::new(ui_unauthorized(state))),
        Ok(AttachmentPath::MissingScope(scope)) => {
            Err(Box::new(ui_insufficient_scope(state, &[scope])))
        }
        Ok(AttachmentPath::Workspace(error)) => Err(Box::new(workspace_failure(&error))),
        Err(error) => {
            error!("attachment access task panicked: {error}");
            Err(Box::new(ui_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error",
            )))
        }
    }
}

fn attachment_db_error(error: &rusqlite::Error) -> Box<Response> {
    error!("attachment graph error: {error}");
    Box::new(ui_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "attachment storage error",
    ))
}

fn attachment_bootstrap_error(error: &mcpmem_core::errors::MCSError) -> Box<Response> {
    error!("attachment graph bootstrap error: {error}");
    Box::new(ui_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "attachment storage error",
    ))
}

fn attachment_failure(error: AttachmentError) -> Box<Response> {
    let (status, code) = match error {
        AttachmentError::DuplicateFilename => (StatusCode::CONFLICT, "conflict"),
        AttachmentError::Mime => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type"),
        AttachmentError::Size | AttachmentError::WorkspaceBudget => {
            (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large")
        }
        AttachmentError::NotFound | AttachmentError::WrongPrincipal => {
            (StatusCode::NOT_FOUND, "not_found")
        }
        AttachmentError::Storage(source) => {
            error!("attachment storage error: {source}");
            return Box::new(ui_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "attachment storage error",
            ));
        }
        _ => (StatusCode::BAD_REQUEST, "bad_request"),
    };
    Box::new(ui_error(status, code, error.to_string()))
}

fn attachment_id(raw: &str) -> std::result::Result<i64, Box<Response>> {
    raw.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| Box::new(bad_request("attachment id must be a positive integer")))
}

fn attachment_query_number(
    params: &HashMap<String, String>,
    name: &str,
    default: Option<i64>,
    min: i64,
) -> std::result::Result<i64, Box<Response>> {
    let number = match params.get(name) {
        Some(raw) => raw.parse::<i64>().ok().filter(|value| *value >= min),
        None => default.filter(|value| *value >= min),
    };
    number.ok_or_else(|| Box::new(bad_request(format!("'{name}' must be an integer >= {min}"))))
}

async fn attachment_result<T, F>(
    path: PathBuf,
    operation: F,
) -> std::result::Result<T, Box<Response>>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> std::result::Result<T, Box<Response>> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        let conn = Connection::open(path).map_err(|error| attachment_db_error(&error))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| attachment_db_error(&error))?;
        // The registry migrates only the legacy graph at startup; every other
        // graph initializes lazily through the WorkspaceHandles get path,
        // which these routes bypass by opening the file directly. Bootstrap
        // the resolved graph here so a first request against a pre-upgrade
        // workspace does not fail with `no such table: attachment`. The
        // existence check keeps the common already-migrated path to one read
        // with no write transaction.
        let migrated: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='attachment')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| attachment_db_error(&error))?;
        if !migrated {
            mcpmem_core::schema::initialize_database(&conn)
                .map_err(|error| attachment_bootstrap_error(&error))?;
        }
        operation(&conn)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => {
            error!("attachment graph task panicked: {error}");
            Err(Box::new(ui_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error",
            )))
        }
    }
}
