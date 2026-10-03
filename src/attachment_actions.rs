//! MCP attachment handlers over the graph selected for this request.
//!
//! The core repository owns every upload mutation and its short transactions.
//! A request opens one connection after the MCP scope and workspace gates pass.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use mcpmem_core::attachments::{
    AttachmentLimits, AttachmentRepository, CHUNK_BYTES, UPLOAD_TTL_US,
};
use mcpmem_core::events::{now_us, sql_error};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::config_file::AttachmentsSection;
use crate::errors::{MCSError, Result};
use crate::workspace::WorkspaceRegistry;

#[derive(Clone)]
pub(crate) struct ToolSettings {
    pub limits: Arc<AttachmentLimits>,
    pub busy_timeout_ms: u64,
}

// A dispatch receives a registry, not the MCPServer. Key the settings by that
// registry so independent server instances cannot borrow each other's limits.
type SettingsEntry = (Weak<WorkspaceRegistry>, ToolSettings);
static SETTINGS: LazyLock<Mutex<Vec<SettingsEntry>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub(crate) fn configure(
    registry: &Arc<WorkspaceRegistry>,
    attachments: &AttachmentsSection,
    busy_timeout_ms: u64,
) {
    let mut settings = SETTINGS.lock().expect("attachment settings lock poisoned");
    settings.retain(|(owner, _)| owner.strong_count() != 0);
    settings.push((
        Arc::downgrade(registry),
        ToolSettings {
            limits: Arc::new(AttachmentLimits {
                max_bytes: attachments.max_bytes,
                workspace_byte_budget: attachments.workspace_byte_budget,
                allow_mime: attachments.allow_mime.clone(),
            }),
            busy_timeout_ms,
        },
    ));
}

pub(crate) fn settings(registry: &WorkspaceRegistry) -> Result<ToolSettings> {
    SETTINGS
        .lock()
        .expect("attachment settings lock poisoned")
        .iter()
        .find(|(owner, _)| std::ptr::eq(owner.as_ptr(), registry))
        .map(|(_, settings)| settings.clone())
        .ok_or_else(|| MCSError::MemoryError("attachment settings not initialized".into()))
}

const PAGE_CHARS: i64 = 4_096;

fn invalid(reason: impl Into<String>) -> MCSError {
    MCSError::InvalidParams(reason.into())
}

fn arguments(args: Option<&Value>) -> Result<&Value> {
    args.filter(|args| args.is_object())
        .ok_or_else(|| invalid("attachment arguments must be an object"))
}

fn string<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("'{name}' must be a string")))
}

fn integer(args: &Value, name: &str) -> Result<i64> {
    args.get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid(format!("'{name}' must be an integer")))
}

fn optional_integer(args: &Value, name: &str, default: i64) -> Result<i64> {
    match args.get(name) {
        None => Ok(default),
        Some(_) => integer(args, name),
    }
}

fn nonnegative(args: &Value, name: &str) -> Result<i64> {
    let value = integer(args, name)?;
    if value < 0 {
        return Err(invalid(format!("'{name}' must not be negative")));
    }
    Ok(value)
}

fn upload_id(args: &Value) -> Result<Uuid> {
    Uuid::parse_str(string(args, "uploadId")?).map_err(|_| invalid("'uploadId' must be a UUID"))
}

fn attachment_id(args: &Value) -> Result<i64> {
    let value = integer(args, "attachmentId")?;
    if value <= 0 {
        return Err(invalid("'attachmentId' must be positive"));
    }
    Ok(value)
}

fn sha256(args: &Value) -> Result<[u8; 32]> {
    let hex = string(args, "sha256")?.as_bytes();
    if hex.len() != 64
        || !hex
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid(
            "'sha256' must be 64 lowercase hexadecimal characters",
        ));
    }
    let mut bytes = [0; 32];
    let (pairs, _) = hex.as_chunks::<2>();
    for (value, pair) in bytes.iter_mut().zip(pairs) {
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => unreachable!("validated lowercase hex"),
        };
        *value = (digit(pair[0]) << 4) | digit(pair[1]);
    }
    Ok(bytes)
}

fn open_graph(path: &Path, busy_timeout_ms: u64) -> Result<Connection> {
    // The selected WorkspaceHandles entry already initialized and migrated this
    // graph. READ_WRITE without CREATE refuses a missing file instead of making
    // an empty graph after a concurrent workspace removal.
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(sql_error)?;
    conn.busy_timeout(Duration::from_millis(busy_timeout_ms))
        .map_err(sql_error)?;
    Ok(conn)
}

fn live_entity_id(conn: &Connection, name: &str) -> Result<i64> {
    conn.query_row(
        "SELECT e.id FROM entity e JOIN entity_revision r ON r.entity_id=e.id
         WHERE e.name=?1 AND e.flags=0 AND r.deleted=0",
        [name],
        |row| row.get(0),
    )
    .optional()
    .map_err(sql_error)?
    .ok_or_else(|| invalid("attachment entity not found"))
}

fn live_attachment(conn: &Connection, id: i64) -> Result<Value> {
    conn.query_row(
        "SELECT a.id,e.name,a.filename,a.mime,a.size_bytes,a.status,a.revision,
                a.error_stage,a.last_error,
                (SELECT count(*) FROM attachment_text t WHERE t.attachment_id=a.id)
         FROM attachment a
         JOIN entity e ON e.id=a.entity_id AND e.flags=0
         JOIN entity_revision r ON r.entity_id=e.id AND r.deleted=0
         WHERE a.id=?1",
        [id],
        |row| {
            Ok(json!({
                "attachmentId": row.get::<_, i64>(0)?,
                "entityName": row.get::<_, String>(1)?,
                "filename": row.get::<_, String>(2)?,
                "mime": row.get::<_, String>(3)?,
                "sizeBytes": row.get::<_, i64>(4)?,
                "status": row.get::<_, String>(5)?,
                "revision": row.get::<_, i64>(6)?,
                "errorStage": row.get::<_, Option<String>>(7)?,
                "lastError": row.get::<_, Option<String>>(8)?,
                "pageCount": row.get::<_, i64>(9)?,
            }))
        },
    )
    .optional()
    .map_err(sql_error)?
    .ok_or_else(|| invalid("attachment not found"))
}

fn text_response(body: &Value) -> Result<Value> {
    Ok(json!({"content": [{"type": "text", "text": body.to_string()}]}))
}

/// The dispatcher calls this only after the category, principal, and workspace
/// checks. The core repository owns upload sessions and file finalization.
pub fn handle(
    name: &str,
    args: Option<&Value>,
    principal_id: &str,
    graph_path: &Path,
    limits: &AttachmentLimits,
    busy_timeout_ms: u64,
) -> Result<Value> {
    let args = arguments(args)?;
    let conn = open_graph(graph_path, busy_timeout_ms)?;
    let repository = AttachmentRepository::new(&conn);
    let response = match name {
        "begin_attachment_upload" => {
            let entity_id = live_entity_id(&conn, string(args, "entityName")?)?;
            let filename = string(args, "filename")?;
            let mime = string(args, "mime")?;
            let expected_bytes = nonnegative(args, "expectedBytes")?;
            let digest = sha256(args)?;
            let expires_us = now_us()
                .checked_add(UPLOAD_TTL_US)
                .ok_or_else(|| invalid("upload expiry is out of range"))?;
            let upload = repository.begin_upload(
                principal_id,
                entity_id,
                filename,
                mime,
                expected_bytes,
                &digest,
                expires_us,
                limits,
            )?;
            json!({"uploadId": upload.to_string(), "nextIndex": 0})
        }
        "append_attachment_chunk" => {
            let upload = upload_id(args)?;
            let index = nonnegative(args, "index")?;
            let encoded = string(args, "content")?;
            if encoded.len() > CHUNK_BYTES.div_ceil(3) * 4 {
                return Err(invalid("attachment chunk exceeds 1,048,576 decoded bytes"));
            }
            let content = STANDARD
                .decode(encoded)
                .map_err(|_| invalid("'content' must be base64"))?;
            if content.is_empty() {
                return Err(invalid("attachment chunk must not be empty"));
            }
            if content.len() > CHUNK_BYTES {
                return Err(invalid("attachment chunk exceeds 1,048,576 decoded bytes"));
            }
            validate_session_limits(&conn, upload, limits)?;
            let (next_index, received_bytes) =
                repository.append_chunk(principal_id, upload, index, &content, now_us())?;
            json!({"nextIndex": next_index, "receivedBytes": received_bytes})
        }
        "finish_attachment_upload" => {
            let upload = upload_id(args)?;
            let id = repository.finish_upload(principal_id, upload, now_us(), limits)?;
            json!({"attachmentId": id, "status": "uploaded"})
        }
        "cancel_attachment_upload" => {
            repository.cancel_upload(principal_id, upload_id(args)?, now_us())?;
            json!({})
        }
        "list_attachments" => {
            let entity_id = live_entity_id(&conn, string(args, "entityName")?)?;
            let mut statement = conn
                .prepare("SELECT id FROM attachment WHERE entity_id=?1 ORDER BY id")
                .map_err(sql_error)?;
            let ids = statement
                .query_map([entity_id], |row| row.get::<_, i64>(0))
                .map_err(sql_error)?;
            let mut attachments = Vec::new();
            for id in ids {
                let mut metadata = live_attachment(&conn, id.map_err(sql_error)?)?;
                metadata
                    .as_object_mut()
                    .expect("metadata is an object")
                    .remove("entityName");
                attachments.push(metadata);
            }
            json!({"attachments": attachments})
        }
        "get_attachment" => live_attachment(&conn, attachment_id(args)?)?,
        "read_attachment_chunk" => read_chunk(&conn, args)?,
        "get_attachment_page" => read_page(&conn, args)?,
        "delete_attachment" => {
            let id = attachment_id(args)?;
            live_attachment(&conn, id)?;
            repository.delete_attachment(id)?;
            json!({})
        }
        _ => return Err(MCSError::MethodNotFound(name.to_owned())),
    };
    text_response(&response)
}

fn validate_session_limits(
    conn: &Connection,
    upload: Uuid,
    limits: &AttachmentLimits,
) -> Result<()> {
    let (mime, expected): (String, i64) = conn
        .query_row(
            "SELECT mime,expected_bytes FROM attachment_upload WHERE upload_id=?1",
            [upload.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?
        .ok_or_else(|| invalid("attachment or upload session not found"))?;
    if expected > limits.max_bytes || expected < 0 {
        return Err(invalid("attachment exceeds the per-file size limit"));
    }
    if !limits.allow_mime.iter().any(|rule| {
        rule == &mime
            || rule.strip_suffix("/*").is_some_and(|prefix| {
                mime.strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            })
    }) {
        return Err(invalid("attachment MIME type is not allowed"));
    }
    let reserved: i64 = conn
        .query_row(
            "SELECT (SELECT COALESCE(SUM(size_bytes),0) FROM attachment) +
                    (SELECT COALESCE(SUM(expected_bytes),0) FROM attachment_upload
                     WHERE attachment_id IS NULL)",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if reserved > limits.workspace_byte_budget {
        return Err(invalid("workspace attachment byte budget exceeded"));
    }
    Ok(())
}

fn read_chunk(conn: &Connection, args: &Value) -> Result<Value> {
    let id = attachment_id(args)?;
    let metadata = live_attachment(conn, id)?;
    let size = metadata["sizeBytes"]
        .as_i64()
        .expect("stored size is an integer");
    let offset = nonnegative(args, "offset")?;
    let length = nonnegative(args, "length")?;
    if length > CHUNK_BYTES as i64 {
        return Err(invalid("'length' must be at most 1,048,576 bytes"));
    }
    if offset > size {
        return Err(invalid("'offset' exceeds attachment size"));
    }
    let count = length.min(size - offset) as usize;
    let mut bytes = vec![0; count];
    if count != 0 {
        let mut blob = conn
            .blob_open("main", "attachment", "content", id, true)
            .map_err(sql_error)?;
        blob.seek(SeekFrom::Start(offset as u64))?;
        blob.read_exact(&mut bytes)?;
    }
    let next_offset = offset + count as i64;
    Ok(json!({"content": STANDARD.encode(bytes), "offset": offset,
        "nextOffset": next_offset, "eof": next_offset == size}))
}

fn read_page(conn: &Connection, args: &Value) -> Result<Value> {
    let id = attachment_id(args)?;
    live_attachment(conn, id)?;
    let page = integer(args, "page")?;
    if page <= 0 {
        return Err(invalid("'page' must be positive"));
    }
    let offset = optional_integer(args, "offset", 0)?;
    if offset < 0 {
        return Err(invalid("'offset' must not be negative"));
    }
    let max_chars = optional_integer(args, "maxChars", PAGE_CHARS)?;
    if !(0..=PAGE_CHARS).contains(&max_chars) {
        return Err(invalid("'maxChars' must be at most 4,096 characters"));
    }
    let (text, chars): (String, i64) = conn
        .query_row(
            "SELECT text,chars FROM attachment_text WHERE attachment_id=?1 AND page=?2",
            params![id, page],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?
        .ok_or_else(|| invalid("attachment page not found"))?;
    if offset > chars {
        return Err(invalid("'offset' exceeds attachment page length"));
    }
    if max_chars == 0 {
        // A zero cap must not read through the page: the loop counter would
        // go negative and the whole page would come back, bypassing the
        // advertised 4,096-character response cap.
        return Ok(json!({"page": page, "text": "", "offset": offset,
            "nextOffset": offset, "eof": offset == chars}));
    }
    let mut selected = String::new();
    let mut next_offset = offset;
    let mut remaining = max_chars;
    for character in text.chars().skip(offset as usize) {
        selected.push(character);
        next_offset += 1;
        remaining -= 1;
        if remaining == 0 {
            break;
        }
    }
    Ok(json!({"page": page, "text": selected, "offset": offset,
        "nextOffset": next_offset, "eof": next_offset == chars}))
}
