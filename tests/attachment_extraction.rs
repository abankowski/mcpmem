//! Integration tests for the attachment extraction worker: the extractor
//! crate's worker, the OCR credential seam, and the `--role extractor`
//! separate-process flow.
//!
//! The extractor-dependent tests need the root `extractor` Cargo feature
//! (`cargo test --test attachment_extraction --features extractor`). The
//! real-PDF test `pdf_render_routes_page_to_vision` needs the external
//! Poppler commands `pdfinfo` and `pdftoppm` on `PATH` and fails when they
//! are missing.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::workspace::{WorkspaceAccess, WorkspaceRegistry};
use mcpmem_core::attachments::{AttachmentLimits, AttachmentRepository};
use mcpmem_core::events::now_us;
use mcpmem_core::graph::GraphHandle;
use mcpmem_core::types::EntityInput;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

#[cfg(feature = "extractor")]
use mcpmem_extractor::{
    DEFAULT_VISION_ENDPOINT, ExtractionReport, ExtractionWorker, OcrConfig, OcrError, OcrProvider,
    PrimaryProvider, VisionOcr, VisionSettings, resolve_ocr, resolve_vision,
};

// ── Shared fixtures ──────────────────────────────────────────────────────

/// The committed two-page PDF fixture. Poppler renders one non-empty image
/// per page, with distinct text on each page.
static PDF: &[u8] = include_bytes!("fixtures/two-page-ocr.pdf");

fn limits() -> AttachmentLimits {
    AttachmentLimits {
        max_bytes: 52_428_800,
        workspace_byte_budget: 268_435_456,
        allow_mime: vec![
            "text/*".into(),
            "text/markdown".into(),
            "application/pdf".into(),
        ],
    }
}

fn digest(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn graph(path: &Path) -> GraphHandle {
    GraphHandle::new(
        path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(8).unwrap(),
        1,
    )
    .unwrap()
}

fn entity(name: &str) -> EntityInput {
    EntityInput {
        name: name.into(),
        entity_type: "note".into(),
        observations: vec![],
        attributes: None,
    }
}

/// One fresh graph file with one live entity. Returns the path and the
/// entity id.
fn test_graph(dir: &tempfile::TempDir) -> (std::path::PathBuf, i64) {
    let path = dir.path().join("graph.sqlite");
    let handle = graph(&path);
    handle.create_entities(&[entity("doc")]).unwrap();
    drop(handle);
    let conn = Connection::open(&path).unwrap();
    mcpmem_core::schema::initialize_database(&conn).unwrap();
    let entity_id: i64 = conn
        .query_row("SELECT id FROM entity WHERE name='doc'", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(conn);
    (path, entity_id)
}

/// Store one attachment as a finished upload would, returning its id.
fn upload(path: &Path, entity_id: i64, filename: &str, mime: &str, content: &[u8]) -> i64 {
    let conn = Connection::open(path).unwrap();
    let id = AttachmentRepository::new(&conn)
        .store_reader(
            entity_id,
            filename,
            mime,
            &mut std::io::Cursor::new(content),
            content.len() as i64,
            &digest(content),
            &limits(),
            now_us(),
        )
        .unwrap();
    drop(conn);
    id
}

fn attachment_status(path: &Path, attachment: i64) -> String {
    let conn = Connection::open(path).unwrap();
    conn.query_row(
        "SELECT status FROM attachment WHERE id=?1",
        [attachment],
        |row| row.get(0),
    )
    .unwrap()
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn count_where(conn: &Connection, table: &str, column: &str, id: i64) -> i64 {
    conn.query_row(
        &format!("SELECT count(*) FROM {table} WHERE {column}=?1"),
        rusqlite::params![id],
        |row| row.get(0),
    )
    .unwrap()
}

// ── Fake vision endpoint ─────────────────────────────────────────────────

/// A one-shot HTTP server that records every request's Authorization header
/// and body, and answers each request with `transcribed page N` for the Nth
/// request. Std threads and sockets, so the blocking vision client never
/// contends with a test runtime.
struct FakeVision {
    url: String,
    state: Arc<Mutex<Vec<(String, String)>>>,
}

impl FakeVision {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake vision listener");
        let addr = listener.local_addr().expect("read back the vision port");
        let state = Arc::new(Mutex::new(Vec::new()));
        let thread_state = Arc::clone(&state);
        let _ = std::thread::Builder::new()
            .name("attachment-vision-fake".into())
            .spawn(move || {
                loop {
                    // A dying client is an accept error, never a server fault.
                    let Some((conn, _)) = listener.accept().ok() else {
                        continue;
                    };
                    let state = Arc::clone(&thread_state);
                    std::thread::spawn(move || serve_vision(conn, &state));
                }
            });
        Self {
            url: format!("http://{addr}/v1/chat/completions"),
            state,
        }
    }

    /// Every request the server has seen, in arrival order, as
    /// `(authorization, body)`.
    fn recorded(&self) -> Vec<(String, String)> {
        self.state.lock().clone()
    }
}

fn serve_vision(mut conn: TcpStream, state: &Arc<Mutex<Vec<(String, String)>>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut headers_end = None;
    loop {
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if headers_end.is_none() {
            headers_end = buf
                .windows(4)
                .position(|window| window == b"\r\n\r\n".as_slice());
        }
        let Some(end) = headers_end else { continue };
        let head = String::from_utf8_lossy(&buf[..end]);
        // reqwest writes header names in lowercase; names are case-insensitive.
        let header = |name: &str| {
            head.lines().find_map(|line| {
                let (line_name, value) = line.split_once(':')?;
                line_name
                    .trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        };
        let content_length = header("Content-Length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let body_start = end + 4;
        if buf.len() < body_start + content_length {
            continue;
        }
        let authorization = header("Authorization").unwrap_or_default();
        let body =
            String::from_utf8_lossy(&buf[body_start..body_start + content_length]).into_owned();
        let mut calls = state.lock();
        calls.push((authorization, body));
        let page = calls.len();
        drop(calls);
        let payload = format!(
            "{{\"choices\":[{{\"message\":{{\"content\":\"transcribed page {page}\"}}}}]}}"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{payload}",
            payload.len()
        );
        let _ = conn.write_all(response.as_bytes());
        return;
    }
}

// ── Environment isolation ────────────────────────────────────────────────

/// Serializes process-environment mutations across the tests of this file.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets one environment variable and restores the prior value on drop,
/// including across a panic.
struct EnvSetter<'a> {
    key: &'a str,
    prior: Option<std::ffi::OsString>,
    had_prior: bool,
}

impl EnvSetter<'_> {
    fn new(key: &'static str, value: Option<&str>) -> EnvSetter<'static> {
        let prior = std::env::var_os(key);
        let had_prior = prior.is_some();
        match value {
            // An empty value means "absent" to every config probe, so an
            // ambient deployment value cannot leak into the test.
            Some(value) => {
                // SAFETY: guarded by ENV_LOCK; the value is written back or
                // removed on drop, also under the lock.
                unsafe { std::env::set_var(key, value) }
            }
            None => {
                // SAFETY: guarded by ENV_LOCK; see new().
                unsafe { std::env::remove_var(key) }
            }
        }
        EnvSetter {
            key,
            prior,
            had_prior,
        }
    }
}

impl Drop for EnvSetter<'_> {
    fn drop(&mut self) {
        if self.had_prior {
            // SAFETY: guarded by ENV_LOCK, see new().
            unsafe { std::env::set_var(self.key, self.prior.as_ref().unwrap()) }
        } else {
            // SAFETY: guarded by ENV_LOCK, see new().
            unsafe { std::env::remove_var(self.key) }
        }
    }
}

/// Runs `run` with the embedding environment pinned to the file layer: every
/// `MCP_MEMORY_*` embedding variable is absent. `api_key` optionally names an
/// ambient key to prove the environment-over-file precedence.
fn with_embedding_env(run: impl FnOnce(), api_key: Option<&str>) {
    let _guard = ENV_LOCK.lock();
    let _key = EnvSetter::new("MCP_MEMORY_OPENAI_API_KEY", api_key);
    let _url = EnvSetter::new("MCP_MEMORY_OPENAI_URL", Some(""));
    let _ollama = EnvSetter::new("MCP_MEMORY_OLLAMA_URL", Some(""));
    run();
}

/// Runs `run` with `PATH` replaced by `path` alone.
fn with_path(path: &str, run: impl FnOnce()) {
    let _guard = ENV_LOCK.lock();
    let _path = EnvSetter::new("PATH", Some(path));
    run();
}

// ── Text extraction ──────────────────────────────────────────────────────

#[cfg(feature = "extractor")]
#[test]
fn text_extraction_without_ocr_moves_uploaded_to_ready() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    // A BOM and CRLF exercise the normalize step: the stored page must be the
    // decoded, normalized text.
    let attachment = upload(
        &path,
        entity_id,
        "memo.txt",
        "text/plain",
        "\u{feff}line one\r\nline two\n".as_bytes(),
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(attachment_status(&path, attachment), "uploaded");
    assert_eq!(
        count(&conn, "attachment_job"),
        1,
        "a finished upload has one pending extraction job"
    );

    let worker = ExtractionWorker::new(&path, None);
    let report = worker.run_once(now_us()).unwrap();
    assert_eq!(
        report,
        ExtractionReport {
            claimed: 1,
            committed: 1,
            retried: 0,
            dead: 0,
            expired_sessions: 0,
        }
    );

    assert_eq!(attachment_status(&path, attachment), "ready");
    let (stage, error, revision): (Option<String>, Option<String>, i64) = conn
        .query_row(
            "SELECT error_stage, last_error, revision FROM attachment WHERE id=?1",
            [attachment],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(stage, None, "success clears the error stage");
    assert_eq!(error, None, "success clears the error message");
    assert_eq!(
        revision, 2,
        "the repository starts an uploaded attachment at revision 1, so the first \
         extraction publishes revision 2"
    );

    let pages: Vec<(i64, String)> = conn
        .prepare("SELECT page, text FROM attachment_text WHERE attachment_id=?1 ORDER BY page")
        .unwrap()
        .query_map([attachment], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        pages,
        vec![(1, "line one\nline two\n".to_owned())],
        "page 1 holds the BOM-stripped, CRLF-normalized text"
    );
    let chunks: Vec<(i64, i64, i64, String)> = conn
        .prepare(
            "SELECT chunk_index, page, segment_index, text FROM attachment_chunk \
             WHERE attachment_id=?1 ORDER BY chunk_index",
        )
        .unwrap()
        .query_map([attachment], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(chunks, vec![(0, 1, 0, "line one\nline two\n".to_owned())]);
    let enqueued: i64 = conn
        .query_row(
            "SELECT count(*) FROM chunk_index_job WHERE owner_kind='attachment' AND owner_id=?1",
            [attachment],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        enqueued, 1,
        "the ready attachment enqueues its embedding job"
    );
    assert_eq!(
        conn.query_row(
            "SELECT state FROM attachment_job WHERE attachment_id=?1",
            [attachment],
            |row| row.get::<_, String>(0),
        )
        .unwrap(),
        "done"
    );
}

#[cfg(feature = "extractor")]
#[test]
fn invalid_utf8_text_fails_at_decode_stage() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let attachment = upload(
        &path,
        entity_id,
        "broken.txt",
        "text/plain",
        &[0xff, 0xfe, 0x00],
    );

    let worker = ExtractionWorker::new(&path, None);
    let report = worker.run_once(now_us()).unwrap();
    assert_eq!(
        report,
        ExtractionReport {
            claimed: 1,
            committed: 0,
            retried: 0,
            dead: 1,
            expired_sessions: 0,
        }
    );

    let conn = Connection::open(&path).unwrap();
    let (status, stage, error): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, error_stage, last_error FROM attachment WHERE id=?1",
            [attachment],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "error");
    assert_eq!(stage.as_deref(), Some("decode"));
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("UTF-8")),
        "the failure names the decode rule: {error:?}"
    );
    assert_eq!(count(&conn, "attachment_text"), 0, "no partial page rows");
    assert_eq!(
        count(&conn, "attachment_chunk"),
        0,
        "no partial segment rows"
    );
    assert_eq!(
        conn.query_row(
            "SELECT state FROM attachment_job WHERE attachment_id=?1",
            [attachment],
            |row| row.get::<_, String>(0),
        )
        .unwrap(),
        "dead"
    );
}

// ── OCR configuration failures ───────────────────────────────────────────

#[cfg(feature = "extractor")]
#[test]
fn pdf_without_ocr_config_fails_at_config_stage() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let attachment = upload(&path, entity_id, "scan.pdf", "application/pdf", PDF);

    let worker = ExtractionWorker::new(&path, None);
    let report = worker.run_once(now_us()).unwrap();
    assert_eq!(report.dead, 1);
    assert_eq!(report.retried, 0);

    let conn = Connection::open(&path).unwrap();
    let (status, stage, error): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, error_stage, last_error FROM attachment WHERE id=?1",
            [attachment],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "error");
    assert_eq!(stage.as_deref(), Some("config"));
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("[ocr]")),
        "the failure names the missing section: {error:?}"
    );
    assert_eq!(count(&conn, "attachment_text"), 0, "no page rows");
}

#[cfg(feature = "extractor")]
#[test]
fn unknown_ocr_provider_fails_pdf_at_config_stage() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let attachment = upload(&path, entity_id, "scan.pdf", "application/pdf", PDF);

    let ocr = resolve_ocr(
        Some(&OcrConfig {
            provider: "bogus".into(),
            model: None,
            vision_url: None,
            api_key_file: None,
        }),
        &PrimaryProvider {
            kind: Some("openai".into()),
            api_key: Some("key".into()),
        },
    )
    .expect("a rejected section still produces a provider object");
    let worker = ExtractionWorker::new(&path, Some(ocr));
    let report = worker.run_once(now_us()).unwrap();
    assert_eq!(report.dead, 1);
    assert_eq!(report.retried, 0);

    let conn = Connection::open(&path).unwrap();
    let (status, stage, error): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, error_stage, last_error FROM attachment WHERE id=?1",
            [attachment],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "error");
    assert_eq!(stage.as_deref(), Some("config"));
    assert_eq!(
        error.as_deref(),
        Some("unsupported ocr provider 'bogus'"),
        "the exact named provider error is stored"
    );
    assert_eq!(count(&conn, "attachment_text"), 0);
}

#[cfg(feature = "extractor")]
#[test]
fn explicit_openai_key_file_errors_are_terminal_config_and_text_still_extracts() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let missing_key = dir.path().join("missing-vision-key");

    // Two settings: a missing key file and an empty one. Both are terminal
    // `config` failures for a PDF, with no request; the same provider still
    // extracts text.
    for key_path in [missing_key.to_string_lossy().into_owned(), {
        let empty = dir.path().join("empty-vision-key");
        std::fs::write(&empty, b"").unwrap();
        empty.to_string_lossy().into_owned()
    }] {
        let attachment = upload(
            &path,
            entity_id,
            &format!("scan-{key_path}.pdf"),
            "application/pdf",
            PDF,
        );
        let ocr = resolve_ocr(
            Some(&OcrConfig {
                provider: "openai".into(),
                model: None,
                vision_url: None,
                api_key_file: Some(key_path.clone()),
            }),
            &PrimaryProvider::default(),
        )
        .expect("an unresolvable section still produces a provider object");
        let worker = ExtractionWorker::new(&path, Some(ocr));
        assert_eq!(worker.run_once(now_us()).unwrap().dead, 1);
        let conn = Connection::open(&path).unwrap();
        let (status, stage, error): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, error_stage, last_error FROM attachment WHERE id=?1",
                [attachment],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "error");
        assert_eq!(stage.as_deref(), Some("config"));
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("api-key-file")),
            "{key_path}: the failure names the key file rule: {error:?}"
        );
        assert_eq!(
            count_where(&conn, "attachment_text", "attachment_id", attachment),
            0,
            "a config failure publishes no page rows"
        );

        // The same settings leave text extraction untouched.
        let text_id = upload(
            &path,
            entity_id,
            &format!("memo-{key_path}.txt"),
            "text/plain",
            b"text works",
        );
        let text_worker = ExtractionWorker::new(
            &path,
            resolve_ocr(
                Some(&OcrConfig {
                    provider: "openai".into(),
                    model: None,
                    vision_url: None,
                    api_key_file: Some(key_path),
                }),
                &PrimaryProvider::default(),
            ),
        );
        assert_eq!(text_worker.run_once(now_us()).unwrap().committed, 1);
        assert_eq!(attachment_status(&path, text_id), "ready");
    }
}

// ── Credential routing ───────────────────────────────────────────────────

#[cfg(feature = "extractor")]
#[test]
fn inherited_first_party_openai_resolves_the_default_vision_host() {
    with_embedding_env(
        || {
            let dir = tempfile::tempdir().unwrap();
            let key_path = dir.path().join("embedding-key");
            std::fs::write(&key_path, b"first-key\n").unwrap();
            let file = mcpmem::config_file::FileConfig {
                indexer: mcpmem::config_file::IndexerSection {
                    provider: Some("openai".into()),
                    model: Some("embedding-model".into()),
                    dimensions: Some(2),
                    openai_url: Some("http://127.0.0.1:9/v1/embeddings".into()),
                    openai_api_key_file: Some(key_path.to_string_lossy().into_owned()),
                    ..mcpmem::config_file::IndexerSection::default()
                },
                ocr: Some(mcpmem::config_file::OcrSection {
                    provider: "inherit".into(),
                    model: Some("vision-model".into()),
                    vision_url: None,
                    api_key_file: None,
                }),
                ..mcpmem::config_file::FileConfig::default()
            };

            // The root seam derives the primary from the same `indexer_settings`
            // precedence the embedding worker uses.
            let spec = mcpmem::config_file::profile_spec(Some(&file))
                .unwrap()
                .unwrap();
            let settings = mcpmem::config_file::indexer_settings(Some(&file)).unwrap();
            let primary = PrimaryProvider {
                kind: Some(spec.provider_kind),
                api_key: settings.openai_api_key,
            };
            let vision = resolve_vision(
                &OcrConfig {
                    provider: "inherit".into(),
                    model: Some("vision-model".into()),
                    vision_url: None,
                    api_key_file: None,
                },
                &primary,
            )
            .expect("first-party inheritance resolves without a vision-url");
            assert_eq!(
                vision.endpoint, DEFAULT_VISION_ENDPOINT,
                "only a first-party provider may use the default OpenAI vision host"
            );
            assert_eq!(vision.api_key, "first-key");
            assert_eq!(vision.model, "vision-model");
        },
        None,
    );
}

#[cfg(feature = "extractor")]
#[test]
fn inherited_openai_compatible_key_reaches_only_the_vision_endpoint() {
    with_embedding_env(
        || {
            let dir = tempfile::tempdir().unwrap();
            let key_path = dir.path().join("embedding-key");
            std::fs::write(&key_path, b"embed-key\n").unwrap();

            let vision = FakeVision::start();
            let embed = FakeVision::start();
            let file = mcpmem::config_file::FileConfig {
                indexer: mcpmem::config_file::IndexerSection {
                    provider: Some("openai-compatible".into()),
                    model: Some("embedding-model".into()),
                    dimensions: Some(2),
                    openai_url: Some(embed.url.clone()),
                    openai_api_key_file: Some(key_path.to_string_lossy().into_owned()),
                    ..mcpmem::config_file::IndexerSection::default()
                },
                ocr: Some(mcpmem::config_file::OcrSection {
                    provider: "inherit".into(),
                    model: Some("vision-model".into()),
                    vision_url: Some(vision.url.clone()),
                    api_key_file: None,
                }),
                ..mcpmem::config_file::FileConfig::default()
            };

            let (path, entity_id) = test_graph(&dir);
            upload(&path, entity_id, "scan.pdf", "application/pdf", PDF);
            let ocr = mcpmem::runtime::ocr_provider(file.ocr.as_ref(), Some(&file));
            let worker = ExtractionWorker::new(&path, ocr);
            let report = worker.run_once(now_us()).unwrap();
            assert_eq!(report.committed, 1, "both pages transcribe");

            let calls = vision.recorded();
            assert_eq!(calls.len(), 2, "one vision call per rendered page");
            for (authorization, body) in &calls {
                assert_eq!(
                    authorization, "Bearer embed-key",
                    "the inherited primary key must authenticate every vision call"
                );
                assert!(body.contains("\"model\":\"vision-model\""), "{body}");
                assert!(body.contains("data:image/png;base64,"), "{body}");
            }
            assert!(
                embed.recorded().is_empty(),
                "the embeddings endpoint must never receive an OCR request"
            );
        },
        None,
    );
}

#[cfg(feature = "extractor")]
#[test]
fn environment_key_wins_over_the_file_for_inherited_ocr() {
    with_embedding_env(
        || {
            let dir = tempfile::tempdir().unwrap();
            let key_path = dir.path().join("embedding-key");
            std::fs::write(&key_path, b"file-key\n").unwrap();

            let vision = FakeVision::start();
            let file = mcpmem::config_file::FileConfig {
                indexer: mcpmem::config_file::IndexerSection {
                    provider: Some("openai-compatible".into()),
                    model: Some("embedding-model".into()),
                    dimensions: Some(2),
                    openai_url: Some("http://127.0.0.1:9/v1/embeddings".into()),
                    openai_api_key_file: Some(key_path.to_string_lossy().into_owned()),
                    ..mcpmem::config_file::IndexerSection::default()
                },
                ocr: Some(mcpmem::config_file::OcrSection {
                    provider: "inherit".into(),
                    model: None,
                    vision_url: Some(vision.url.clone()),
                    api_key_file: None,
                }),
                ..mcpmem::config_file::FileConfig::default()
            };

            let (path, entity_id) = test_graph(&dir);
            upload(&path, entity_id, "scan.pdf", "application/pdf", PDF);
            let ocr = mcpmem::runtime::ocr_provider(file.ocr.as_ref(), Some(&file));
            let worker = ExtractionWorker::new(&path, ocr);
            assert_eq!(worker.run_once(now_us()).unwrap().committed, 1);
            let calls = vision.recorded();
            assert_eq!(calls.len(), 2);
            for (authorization, _) in &calls {
                assert_eq!(
                    authorization, "Bearer env-key",
                    "an ambient MCP_MEMORY_OPENAI_API_KEY wins over the file key"
                );
            }
        },
        Some("env-key"),
    );
}

// ── PDF rendering and vision routing (real Poppler) ──────────────────────

#[cfg(feature = "extractor")]
#[test]
fn pdf_render_routes_page_to_vision() {
    // This test must fail, not skip, when the external renderer is missing:
    // the deployment smoke depends on it. The command is identical in bash
    // and fish.
    assert!(
        Command::new("pdfinfo").arg("-v").output().is_ok(),
        "pdfinfo must be on PATH (external Poppler); run \
         `command -v pdfinfo && command -v pdftoppm && pdfinfo -v && pdftoppm -v`"
    );
    assert!(
        Command::new("pdftoppm").arg("-v").output().is_ok(),
        "pdftoppm must be on PATH (external Poppler)"
    );

    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let attachment = upload(&path, entity_id, "scan.pdf", "application/pdf", PDF);

    let vision = FakeVision::start();
    let provider = VisionOcr::new(VisionSettings {
        endpoint: vision.url.clone(),
        api_key: "vision-key".into(),
        model: "gpt-4o-mini".into(),
    })
    .unwrap();
    let worker = ExtractionWorker::new(&path, Some(Arc::new(provider)));
    let report = worker.run_once(now_us()).unwrap();
    assert_eq!(report.claimed, 1);
    assert_eq!(report.committed, 1);

    let calls = vision.recorded();
    assert_eq!(
        calls.len(),
        2,
        "one vision call per rendered page, not one per document"
    );
    for (authorization, body) in &calls {
        assert_eq!(authorization, "Bearer vision-key");
        assert!(
            body.contains("data:image/png;base64,"),
            "each call carries the rendered page as a PNG data URL"
        );
    }

    let conn = Connection::open(&path).unwrap();
    assert_eq!(attachment_status(&path, attachment), "ready");
    let (stage, error): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT error_stage, last_error FROM attachment WHERE id=?1",
            [attachment],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stage, None);
    assert_eq!(error, None);

    // The fake answered per call order, so the stored page texts pin the
    // exact page order: page 1 is the first render, page 2 the second.
    let pages: Vec<(i64, String)> = conn
        .prepare("SELECT page, text FROM attachment_text WHERE attachment_id=?1 ORDER BY page")
        .unwrap()
        .query_map([attachment], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        pages,
        vec![
            (1, "transcribed page 1".to_owned()),
            (2, "transcribed page 2".to_owned()),
        ],
        "pages are transcribed in render order"
    );
    let segments: Vec<(i64, i64, i64)> = conn
        .prepare(
            "SELECT chunk_index, page, segment_index FROM attachment_chunk \
             WHERE attachment_id=?1 ORDER BY chunk_index",
        )
        .unwrap()
        .query_map([attachment], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(segments, vec![(0, 1, 0), (1, 2, 0)]);
}

#[cfg(feature = "extractor")]
#[test]
fn pdf_page_renewal_moves_the_lease_deadline_past_the_claim_time() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let attachment = upload(&path, entity_id, "renew.pdf", "application/pdf", PDF);
    let claim_time = now_us() - 10_000_000;
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE attachment_job SET next_attempt_us=?1 WHERE attachment_id=?2",
        rusqlite::params![claim_time, attachment],
    )
    .unwrap();
    drop(conn);

    let vision = FakeVision::start();
    let worker = ExtractionWorker::new(
        &path,
        Some(Arc::new(
            VisionOcr::new(VisionSettings {
                endpoint: vision.url,
                api_key: "vision-key".into(),
                model: "gpt-4o-mini".into(),
            })
            .unwrap(),
        )),
    );
    assert_eq!(worker.run_once(claim_time).unwrap().committed, 1);

    let conn = Connection::open(&path).unwrap();
    let lease_until_us: i64 = conn
        .query_row(
            "SELECT lease_until_us FROM attachment_job WHERE attachment_id=?1",
            [attachment],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        lease_until_us > claim_time + 38_000_000,
        "each PDF page must extend the lease from the current time"
    );
}

// ── Transient, terminal, and fenced states ───────────────────────────────

#[cfg(feature = "extractor")]
struct FlakyOcr {
    calls: AtomicUsize,
}

#[cfg(feature = "extractor")]
impl OcrProvider for FlakyOcr {
    fn transcribe_page(&self, _image: &[u8], _mime: &str) -> Result<String, OcrError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(OcrError::Provider("vision down".into()))
        } else {
            Ok("recovered text".into())
        }
    }
}

#[cfg(feature = "extractor")]
#[test]
fn transient_provider_failure_retains_extracting_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let attachment = upload(&path, entity_id, "scan.pdf", "application/pdf", PDF);

    let worker = ExtractionWorker::new(
        &path,
        Some(Arc::new(FlakyOcr {
            calls: AtomicUsize::new(0),
        })),
    );
    let first = worker.run_once(now_us()).unwrap();
    assert_eq!(first.claimed, 1);
    assert_eq!(first.retried, 1);
    assert_eq!(first.committed, 0);
    assert_eq!(first.dead, 0);

    let conn = Connection::open(&path).unwrap();
    let (status, stage, error): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, error_stage, last_error FROM attachment WHERE id=?1",
            [attachment],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        status, "extracting",
        "a transient failure keeps the job alive"
    );
    assert_eq!(stage.as_deref(), Some("provider"));
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("vision down")),
        "{error:?}"
    );
    assert_eq!(count(&conn, "attachment_text"), 0, "no partial pages");
    assert_eq!(
        conn.query_row(
            "SELECT state FROM attachment_job WHERE attachment_id=?1",
            [attachment],
            |row| row.get::<_, String>(0),
        )
        .unwrap(),
        "pending",
        "the job returns to the queue"
    );

    let second = worker.run_once(now_us() + 2_000_000).unwrap();
    assert_eq!(second.claimed, 1);
    assert_eq!(second.committed, 1);
    assert_eq!(second.retried, 0);
    assert_eq!(attachment_status(&path, attachment), "ready");
    let (stage, error): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT error_stage, last_error FROM attachment WHERE id=?1",
            [attachment],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stage, None, "success clears the previous error badge");
    assert_eq!(error, None);
    assert_eq!(
        count(&conn, "attachment_text"),
        2,
        "both PDF pages commit after the retry succeeds"
    );
}

#[cfg(feature = "extractor")]
struct AlwaysFailOcr;

#[cfg(feature = "extractor")]
impl OcrProvider for AlwaysFailOcr {
    fn transcribe_page(&self, _image: &[u8], _mime: &str) -> Result<String, OcrError> {
        Err(OcrError::Provider("still down".into()))
    }
}

#[cfg(feature = "extractor")]
#[test]
fn eight_failed_attempts_set_terminal_error() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let attachment = upload(&path, entity_id, "scan.pdf", "application/pdf", PDF);

    let worker = ExtractionWorker::new(&path, Some(Arc::new(AlwaysFailOcr)));
    for attempt in 1..=8_i64 {
        let report = worker.run_once(now_us() + attempt * 2_000_000).unwrap();
        assert_eq!(report.claimed, 1, "attempt {attempt} claims the job");
        assert_eq!(report.committed, 0);
        assert_eq!(
            report.dead,
            usize::from(attempt == 8),
            "attempt {attempt}: the eighth attempt dead-letters the job"
        );
        assert_eq!(
            report.retried,
            usize::from(attempt < 8),
            "attempt {attempt}: earlier attempts stay retryable"
        );
    }

    let conn = Connection::open(&path).unwrap();
    let (status, stage, error): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, error_stage, last_error FROM attachment WHERE id=?1",
            [attachment],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "error");
    assert_eq!(stage.as_deref(), Some("provider"));
    assert_eq!(error.as_deref(), Some("still down"));
    assert_eq!(count(&conn, "attachment_text"), 0, "no partial pages");
    assert_eq!(
        conn.query_row(
            "SELECT state FROM attachment_job WHERE attachment_id=?1",
            [attachment],
            |row| row.get::<_, String>(0),
        )
        .unwrap(),
        "dead"
    );

    let exhausted = worker.run_once(now_us() + 40_000_000).unwrap();
    assert_eq!(exhausted.claimed, 0, "a dead job is never claimed again");
}

#[cfg(feature = "extractor")]
struct StealingOcr {
    attachment_id: i64,
    graph: std::path::PathBuf,
}

#[cfg(feature = "extractor")]
impl OcrProvider for StealingOcr {
    fn transcribe_page(&self, _image: &[u8], _mime: &str) -> Result<String, OcrError> {
        // A concurrent writer supersedes the attachment between the claim
        // and the commit, exactly what the revision fence must refuse.
        let conn = Connection::open(&self.graph).unwrap();
        conn.execute(
            "UPDATE attachment SET revision=revision+1 WHERE id=?1",
            [self.attachment_id],
        )
        .unwrap();
        Ok("stale page".into())
    }
}

#[cfg(feature = "extractor")]
#[test]
fn a_lost_lease_publishes_no_pages() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let attachment = upload(&path, entity_id, "scan.pdf", "application/pdf", PDF);

    let worker = ExtractionWorker::new(
        &path,
        Some(Arc::new(StealingOcr {
            attachment_id: attachment,
            graph: path.clone(),
        })),
    );
    let report = worker.run_once(now_us()).unwrap();
    assert_eq!(
        report,
        ExtractionReport {
            claimed: 1,
            committed: 0,
            retried: 0,
            dead: 0,
            expired_sessions: 0,
        },
        "a lost lease can commit neither the pages nor a failure"
    );

    let conn = Connection::open(&path).unwrap();
    assert_eq!(count(&conn, "attachment_text"), 0);
    assert_eq!(count(&conn, "attachment_chunk"), 0);
    assert_eq!(attachment_status(&path, attachment), "extracting");
}

// ── Render failures, absent commands, empty images ───────────────────────

#[cfg(feature = "extractor")]
struct UncalledOcr {
    called: AtomicBool,
}

#[cfg(feature = "extractor")]
impl OcrProvider for UncalledOcr {
    fn transcribe_page(&self, _image: &[u8], _mime: &str) -> Result<String, OcrError> {
        self.called.store(true, Ordering::SeqCst);
        Err(OcrError::Config("this provider must never run".into()))
    }
}

#[cfg(feature = "extractor")]
#[test]
fn render_failures_record_the_render_stage_and_publish_nothing() {
    let cases: &[(&str, &str)] = &[
        // pdftoppm is absent from PATH; pdfinfo answers with two pages.
        ("absent-pdftoppm", "#!/bin/sh\nprintf 'Pages: 2\\n'\n"),
        // pdftoppm exists but exits nonzero with a diagnostic.
        (
            "failing-pdftoppm",
            "#!/bin/sh\necho 'syntax error' >&2\nexit 3\n",
        ),
        // pdftoppm succeeds but writes an empty image.
        (
            "empty-pdftoppm",
            "#!/usr/bin/env bash\n: > \"${@: -1}.png\"\n",
        ),
    ];

    for (name, script) in cases {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("pdfinfo"), b"#!/bin/sh\nprintf 'Pages: 2\\n'\n").unwrap();
        if *name != "absent-pdftoppm" {
            std::fs::write(bin.join("pdftoppm"), script).unwrap();
        }
        for tool in ["pdfinfo", "pdftoppm"] {
            let path = bin.join(tool);
            if path.exists() {
                let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(0o755);
                std::fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let (path, entity_id) = test_graph(&dir);
        let attachment = upload(&path, entity_id, "scan.pdf", "application/pdf", PDF);
        let ocr = Arc::new(UncalledOcr {
            called: AtomicBool::new(false),
        });
        let ocr_ref = Arc::clone(&ocr);
        let worker = ExtractionWorker::new(&path, Some(Arc::clone(&ocr) as Arc<dyn OcrProvider>));
        let mut report = ExtractionReport::default();
        with_path(&bin.to_string_lossy(), || {
            report = worker.run_once(now_us()).unwrap();
        });
        assert_eq!(report.retried, 1, "{name}: a render failure is transient");
        assert_eq!(report.committed, 0);
        assert_eq!(report.dead, 0);
        assert!(
            !ocr_ref.called.load(Ordering::SeqCst),
            "{name}: no OCR call"
        );

        let conn = Connection::open(&path).unwrap();
        let (status, stage, error): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, error_stage, last_error FROM attachment WHERE id=?1",
                [attachment],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "extracting", "{name}");
        assert_eq!(stage.as_deref(), Some("render"), "{name}");
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("pdftoppm")),
            "{name}: the failure names the renderer: {error:?}"
        );
        assert_eq!(count(&conn, "attachment_text"), 0, "{name}");
        assert_eq!(count(&conn, "attachment_chunk"), 0, "{name}");
    }
}

// ── Upload session sweep ─────────────────────────────────────────────────

#[cfg(feature = "extractor")]
#[test]
fn expired_upload_sessions_are_swept_on_an_idle_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let conn = Connection::open(&path).unwrap();
    AttachmentRepository::new(&conn)
        .begin_upload(
            "alice",
            entity_id,
            "stale.txt",
            "text/plain",
            4,
            &digest(b"1234"),
            now_us() - 1,
            &limits(),
        )
        .unwrap();
    assert_eq!(count(&conn, "attachment_upload"), 1);
    drop(conn);

    let worker = ExtractionWorker::new(&path, None);
    let report = worker.run_once(now_us()).unwrap();
    assert_eq!(
        report.expired_sessions, 1,
        "the turn sweeps expiring sessions"
    );
    assert_eq!(report.claimed, 0, "the sweep itself claims nothing");

    let conn = Connection::open(&path).unwrap();
    assert_eq!(count(&conn, "attachment_upload"), 0);
    assert_eq!(count(&conn, "attachment_upload_chunk"), 0);
}

// ── Separate-process operation ───────────────────────────────────────────

/// Drains a child's stderr on a background thread into a shared line list,
/// so the child can never block on a full pipe while the test polls.
fn drain_lines(mut output: std::process::ChildStderr) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&lines);
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        let mut line = Vec::<u8>::new();
        loop {
            match output.read(&mut buffer) {
                Ok(0) | Err(_) => {
                    if !line.is_empty() {
                        captured
                            .lock()
                            .push(String::from_utf8_lossy(&line).trim_end().to_owned());
                    }
                    return;
                }
                Ok(count) => {
                    for byte in &buffer[..count] {
                        if *byte == b'\n' {
                            captured
                                .lock()
                                .push(String::from_utf8_lossy(&line).trim_end().to_owned());
                            line.clear();
                        } else {
                            line.push(*byte);
                        }
                    }
                }
            }
        }
    });
    lines
}

fn wait_for_line(lines: &Arc<Mutex<Vec<String>>>, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if lines.lock().iter().any(|line| line.contains(needle)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(feature = "extractor")]
#[test]
fn a_separate_extractor_process_makes_the_upload_ready() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("legacy.sqlite");
    let registry = WorkspaceRegistry::open(&legacy, Some("machine:local")).unwrap();
    let record = registry
        .resolve("machine:local", None, WorkspaceAccess::Owner)
        .unwrap();
    assert_eq!(record.graph_path, legacy);

    let handle = graph(&legacy);
    handle.create_entities(&[entity("doc")]).unwrap();
    drop(handle);
    let conn = Connection::open(&legacy).unwrap();
    mcpmem_core::schema::initialize_database(&conn).unwrap();
    let entity_id: i64 = conn
        .query_row("SELECT id FROM entity WHERE name='doc'", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(conn);
    let attachment = upload(
        &legacy,
        entity_id,
        "memo.txt",
        "text/plain",
        b"durable text\n",
    );
    assert_eq!(attachment_status(&legacy, attachment), "uploaded");

    let binary = env!("CARGO_BIN_EXE_mcpmem");
    let memory = legacy.to_str().unwrap();

    // Process A: the MCP process with attachment tools enabled and no
    // extractor role. It must warn at startup and never extract, while the
    // upload stays durably queued.
    let mut a = Command::new(binary)
        .args([
            "--memory-file",
            memory,
            "--role",
            "mcp",
            "--enable-attachments",
            "--enable-graph-read",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the MCP process");
    let _a_stdin = a.stdin.take();
    let a_lines = drain_lines(a.stderr.take().unwrap());
    assert!(
        wait_for_line(
            &a_lines,
            "runs no `extractor` role",
            Duration::from_secs(30)
        ),
        "the process without an extractor role must warn at startup; stderr:\n{}",
        a_lines.lock().join("\n")
    );
    assert!(
        !a_lines
            .lock()
            .iter()
            .any(|line| line.contains("extractor process is running")),
        "the warning must not claim a remote worker exists"
    );
    assert_eq!(
        attachment_status(&legacy, attachment),
        "uploaded",
        "a process without the extractor role must not extract"
    );
    a.kill().expect("stop the MCP process");
    a.wait().unwrap();

    // Process B: a separate extractor process on the same workspace graph.
    // No local worker on the MCP process is involved.
    let mut b = Command::new(binary)
        .args(["--memory-file", memory, "--role", "extractor"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the extractor process");
    let b_lines = drain_lines(b.stderr.take().unwrap());

    let deadline = Instant::now() + Duration::from_secs(30);
    while attachment_status(&legacy, attachment) != "ready" {
        assert!(
            Instant::now() < deadline,
            "the separate extractor process must claim and complete the job; \
             stderr:\n{}",
            b_lines.lock().join("\n")
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        wait_for_line(&b_lines, "attachment extracted", Duration::from_secs(10)),
        "the extractor process logs the completed extraction; stderr:\n{}",
        b_lines.lock().join("\n")
    );
    b.kill().expect("stop the extractor process");
    b.wait().unwrap();

    let conn = Connection::open(&legacy).unwrap();
    let pages: Vec<(i64, String)> = conn
        .prepare("SELECT page, text FROM attachment_text WHERE attachment_id=?1 ORDER BY page")
        .unwrap()
        .query_map([attachment], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(pages, vec![(1, "durable text\n".to_owned())]);
}

// ── Builds without the extractor feature ─────────────────────────────────

/// A build without the `extractor` feature still accepts an upload and keeps
/// the job durably queued: upload tools never depend on the local role.
#[cfg(not(feature = "extractor"))]
#[test]
fn uploads_are_durable_without_the_extractor_feature() {
    let dir = tempfile::tempdir().unwrap();
    let (path, entity_id) = test_graph(&dir);
    let attachment = upload(
        &path,
        entity_id,
        "memo.txt",
        "text/plain",
        b"durable text\n",
    );
    assert_eq!(attachment_status(&path, attachment), "uploaded");
    let conn = Connection::open(&path).unwrap();
    assert_eq!(count(&conn, "attachment_job"), 1);
    assert_eq!(
        conn.query_row(
            "SELECT state FROM attachment_job WHERE attachment_id=?1",
            [attachment],
            |row| row.get::<_, String>(0),
        )
        .unwrap(),
        "pending"
    );
}
