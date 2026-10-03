//! The durable attachment extraction worker.
//!
//! One `run_once` turn sweeps expired upload sessions and leases at most one
//! extraction job from the resolved graph. Text files decode in process;
//! PDFs render through the external Poppler `pdfinfo` and `pdftoppm` commands,
//! one page per invocation, and each rendered image goes to the vision OCR
//! provider. OCR calls run outside any graph write transaction, and the
//! fenced completion publishes all page rows in one transaction or none.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mcpmem_core::attachments::{
    AttachmentError, AttachmentJob, AttachmentJobRepository, AttachmentRepository,
};
use mcpmem_core::errors::MCSError;
use rusqlite::Connection;
use thiserror::Error;

use crate::ocr::{OcrError, OcrProvider};

/// One claimed job leases the attachment for this long.
const LEASE_US: i64 = 30_000_000;
/// Failures allowed before a job is dead-lettered. Mirrors the indexer and
/// webhook workers' bound.
const MAX_ATTEMPTS: i64 = 8;
/// The delay before the next attempt after a transient failure.
const RETRY_DELAY_US: i64 = 1_000_000;
/// Render resolution in dots per inch.
const RENDER_DPI: &str = "150";
/// `RENDER_DPI` as a number, for the pixel-cap arithmetic.
const RENDER_DPI_NUM: i64 = 150;
/// The hard deadline for one `pdfinfo` or `pdftoppm` invocation. A stalled
/// renderer is killed, never left to hold the worker loop. The render and
/// vision windows together must fit inside [`LEASE_US`]; a test pins that
/// invariant against [`crate::ocr::VISION_TIMEOUT_US`].
const RENDER_DEADLINE_US: i64 = 8_000_000;
/// PDFs with more pages are refused before any render. It equals
/// `MAX_ATTEMPTS` times [`MAX_PAGES_PER_TURN`], so a maximum-size PDF
/// completes within the eight-claim dead-letter bound.
const MAX_PDF_PAGES: i64 = 64;
/// The most pages one claim transcribes. A longer PDF keeps its lease per
/// page, defers at the bound, and resumes from its durable checkpoints on a
/// later claim, so the role loop rotates instead of holding one workspace
/// for a whole document.
const MAX_PAGES_PER_TURN: i64 = 8;
/// The most pixels one rendered page may produce at `RENDER_DPI`. A page
/// box whose raster exceeds the cap is refused from the `pdfinfo` size line
/// before any render.
const MAX_RENDER_PIXELS: i64 = 16_777_216;
/// The most bytes one rendered PNG may occupy. A page whose image exceeds
/// the cap is refused after the render, before the base64 OCR encode.
const MAX_RENDER_BYTES: usize = 33_554_432;

/// Use the live clock after slow OCR, but retain a caller's simulated future
/// clock for deterministic retry tests. Never move a lease deadline back.
fn current_us(floor_us: i64) -> i64 {
    floor_us.max(mcpmem_core::events::now_us())
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct ExtractionReport {
    pub claimed: usize,
    pub committed: usize,
    pub retried: usize,
    pub dead: usize,
    pub expired_sessions: u64,
}

#[derive(Debug, Error)]
pub enum ExtractionError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("core error: {0}")]
    Core(#[from] MCSError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<AttachmentError> for ExtractionError {
    fn from(error: AttachmentError) -> Self {
        ExtractionError::Core(MCSError::from(error))
    }
}

pub struct ExtractionWorker {
    database: PathBuf,
    ocr: Option<Arc<dyn OcrProvider>>,
}

impl ExtractionWorker {
    /// `ocr` is `None` when no `[ocr]` section exists. PDF jobs then fail at
    /// stage `config`; text extraction never needs it.
    pub fn new(database: impl AsRef<Path>, ocr: Option<Arc<dyn OcrProvider>>) -> Self {
        Self {
            database: database.as_ref().to_path_buf(),
            ocr,
        }
    }

    /// One bounded turn: sweep expired upload sessions, then claim and
    /// process at most one due extraction job. A lost lease publishes
    /// nothing, and a transient failure keeps the job claimable.
    pub fn run_once(&self, now_us: i64) -> Result<ExtractionReport, ExtractionError> {
        let conn = Connection::open(&self.database)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        mcpmem_core::schema::initialize_database(&conn)?;
        let mut report = ExtractionReport {
            expired_sessions: AttachmentRepository::new(&conn).expire_uploads(now_us)?,
            ..ExtractionReport::default()
        };
        let jobs = AttachmentJobRepository::new(&conn);
        let Some(job) = jobs.claim_due(now_us, LEASE_US)? else {
            return Ok(report);
        };
        report.claimed = 1;
        // The blob is read outside any transaction: rendering and OCR must
        // never hold the graph write lock.
        let (mime, content): (String, Vec<u8>) = conn.query_row(
            "SELECT mime, content FROM attachment WHERE id=?1",
            [job.attachment_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        match extract_attachment(self.ocr.as_deref(), &conn, &jobs, &job, &mime, &content, now_us) {
            Ok(ExtractionOutcome::Pages(pages)) => {
                if jobs.complete(&job, current_us(now_us), &pages)? {
                    report.committed = 1;
                    tracing::info!(
                        attachment_id = job.attachment_id,
                        pages = pages.len(),
                        "attachment extracted"
                    );
                } else {
                    tracing::warn!(
                        attachment_id = job.attachment_id,
                        "attachment lease was lost before commit; the extraction is not published"
                    );
                }
            }
            Ok(ExtractionOutcome::Deferred) => {
                // No write-back: the job keeps its lease, the role loop moves
                // on to the next workspace, and the job re-claims once the
                // lease expires and resumes from its durable checkpoints.
                tracing::warn!(
                    attachment_id = job.attachment_id,
                    "attachment page turn bound reached; the job resumes on a later claim"
                );
            }
            Ok(ExtractionOutcome::LostLease) => {
                tracing::warn!(
                    attachment_id = job.attachment_id,
                    "attachment lease was lost during extraction; the job belongs to another worker"
                );
            }
            Err(ExtractionFailure {
                stage,
                reason,
                dead,
            }) => {
                // The repository dead-letters at its own eight-attempt bound;
                // keep the report and the logs honest about what it records.
                let dead = dead || job.attempts >= MAX_ATTEMPTS;
                let finished_at = current_us(now_us);
                let next_attempt_us = finished_at.saturating_add(RETRY_DELAY_US);
                if jobs.retry(&job, finished_at, next_attempt_us, stage, &reason, dead)? {
                    if dead {
                        report.dead = 1;
                        tracing::error!(
                            attachment_id = job.attachment_id,
                            %stage,
                            %reason,
                            "attachment extraction failed terminally"
                        );
                    } else {
                        report.retried = 1;
                        tracing::warn!(
                            attachment_id = job.attachment_id,
                            %stage,
                            %reason,
                            "attachment extraction failed; scheduled for retry"
                        );
                    }
                } else {
                    tracing::warn!(
                        attachment_id = job.attachment_id,
                        "attachment lease was lost before the failure was recorded"
                    );
                }
            }
        }
        Ok(report)
    }
}

/// What one claim produced. `LostLease` means another worker or a delete
/// superseded the claim; neither outcome may be written. `Deferred` means
/// the turn's page bound was reached: nothing is written back and the job
/// resumes from its checkpoints once the lease expires.
enum ExtractionOutcome {
    Pages(Vec<(i64, String)>),
    Deferred,
    LostLease,
}

#[derive(Debug)]
struct ExtractionFailure {
    stage: &'static str,
    reason: String,
    dead: bool,
}

fn transient(stage: &'static str, reason: impl Into<String>) -> ExtractionFailure {
    ExtractionFailure {
        stage,
        reason: reason.into(),
        dead: false,
    }
}

fn terminal(stage: &'static str, reason: impl Into<String>) -> ExtractionFailure {
    ExtractionFailure {
        stage,
        reason: reason.into(),
        dead: true,
    }
}

fn extract_attachment(
    ocr: Option<&dyn OcrProvider>,
    conn: &Connection,
    jobs: &AttachmentJobRepository<'_>,
    job: &AttachmentJob,
    mime: &str,
    content: &[u8],
    now_us: i64,
) -> Result<ExtractionOutcome, ExtractionFailure> {
    let mime = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    if mime == "application/pdf" {
        extract_pdf(ocr, conn, jobs, job, content, now_us)
    } else {
        // Anything the upload allowlist admitted that is not a PDF is decoded
        // as UTF-8 text; text/* MIME types are the ordinary case.
        extract_text(content).map(ExtractionOutcome::Pages)
    }
}

fn extract_text(content: &[u8]) -> Result<Vec<(i64, String)>, ExtractionFailure> {
    let mut text = String::from_utf8(content.to_vec())
        .map_err(|error| terminal("decode", format!("attachment is not valid UTF-8: {error}")))?;
    // A BOM is an encoding artifact, not text; CRLF becomes the LF the rest
    // of the system expects.
    if let Some(rest) = text.strip_prefix('\u{feff}') {
        text = rest.to_owned();
    }
    if text.contains("\r\n") {
        text = text.replace("\r\n", "\n");
    }
    Ok(vec![(1, text)])
}

fn extract_pdf(
    ocr: Option<&dyn OcrProvider>,
    conn: &Connection,
    jobs: &AttachmentJobRepository<'_>,
    job: &AttachmentJob,
    content: &[u8],
    now_us: i64,
) -> Result<ExtractionOutcome, ExtractionFailure> {
    let Some(ocr) = ocr else {
        return Err(terminal(
            "config",
            "PDF OCR is not configured: add an [ocr] section to the configuration file",
        ));
    };
    let dir = tempfile::tempdir().map_err(|error| {
        transient(
            "storage",
            format!("cannot create a private render directory: {error}"),
        )
    })?;
    let pdf_path = dir.path().join("attachment.pdf");
    std::fs::write(&pdf_path, content).map_err(|error| {
        transient(
            "storage",
            format!("cannot write the private render PDF: {error}"),
        )
    })?;
    let page_count = pdf_page_count(&pdf_path)?;
    // Pages a previous claim already transcribed are durable in
    // `attachment_text`; only the fenced commit publishes them. Resume past
    // them, so a long PDF split across claims never pays its early pages
    // twice. The rows are provisional until the commit rewrites them.
    let mut pages = transcribed_pages(conn, job.attachment_id)?;
    let mut budget = MAX_PAGES_PER_TURN;
    let mut deferred = false;
    if (pages.len() as i64) < page_count {
        for page in (pages.len() as i64) + 1..=page_count {
            if budget <= 0 {
                deferred = true;
                break;
            }
            let image = render_page(&pdf_path, dir.path(), page)?;
            let text = match ocr.transcribe_page(&image, "image/png") {
                Ok(text) => text,
                Err(OcrError::Config(reason)) => return Err(terminal("config", reason)),
                Err(OcrError::Provider(reason)) => return Err(transient("provider", reason)),
            };
            // Re-arm the lease after each page so a long OCR job cannot
            // expire under a second worker; a lost lease stops the loop with
            // no publish. The renewal fence checks the token, the epoch, the
            // deadline, the revision and the entity in one row, so the
            // checkpoint insert below can only ever land for the claim that
            // still owns the job. A fresh clock reading is required: reusing
            // the turn-start `now_us` would set lease_until_us back to the
            // claim-time expiry, so the deadline would never move.
            if !jobs
                .renew(job, current_us(now_us), LEASE_US)
                .map_err(|error| {
                    transient("storage", format!("cannot renew attachment lease: {error}"))
                })?
            {
                return Ok(ExtractionOutcome::LostLease);
            }
            checkpoint_page(conn, job, page, &text, now_us)?;
            pages.push((page, text));
            budget -= 1;
        }
    }
    if deferred {
        Ok(ExtractionOutcome::Deferred)
    } else {
        Ok(ExtractionOutcome::Pages(pages))
    }
}

/// One external command's captured output.
#[derive(Debug)]
struct RunOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run one built command under a hard deadline. A stalled process is killed
/// and reported as a deadline breach, never left to hold the worker loop.
/// Stdout and stderr drain on reader threads so a chatty renderer cannot
/// fill a pipe and stall.
fn run_with_deadline(
    program: &str,
    cmd: &mut Command,
    deadline_us: i64,
) -> Result<RunOutput, ExtractionFailure> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| transient("render", format!("cannot run {program}: {error}")))?;
    let out_reader = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut out = Vec::<u8>::new();
            let mut chunk = [0_u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.extend_from_slice(&chunk[..n]),
                }
            }
            out
        })
    });
    let err_reader = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut err = Vec::<u8>::new();
            let mut chunk = [0_u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => err.extend_from_slice(&chunk[..n]),
                }
            }
            err
        })
    });
    let deadline = Instant::now() + Duration::from_millis((deadline_us / 1_000) as u64);
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| transient("render", format!("cannot wait for {program}: {error}")))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(transient(
                "render",
                format!("{program} exceeded the {}-second deadline", deadline_us / 1_000_000),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    Ok(RunOutput { status, stdout, stderr })
}

/// Parse the rendered pixel count from one pdfinfo size line
/// (`Page size: 612 x 792 pts` or `Page N size: ...`). The conversion rounds
/// every fractional point up, so a page can never slip under the cap.
fn size_line_pixels(line: &str) -> Option<i64> {
    let Some((head, body)) = line.trim().split_once(" size:") else {
        return None;
    };
    if head != "Page" && !head.starts_with("Page ") {
        return None;
    }
    let tokens = body.split_whitespace().collect::<Vec<_>>();
    if tokens.len() < 3 {
        return None;
    }
    let w = tokens[0].parse::<i64>().ok()?;
    let h = tokens[2].parse::<i64>().ok()?;
    // ceil(pts * dpi / 72) in integer arithmetic; the +1 covers the
    // truncation of a fractional source value.
    let px = |pts: i64| (pts * RENDER_DPI_NUM + 71) / 72 + 1;
    Some(px(w) * px(h))
}

/// The largest page raster among pdfinfo's size lines, from the whole
/// stdout.
fn render_pixel_max(stdout: &str) -> Option<i64> {
    let mut largest: Option<i64> = None;
    for line in stdout.lines() {
        if let Some(pixels) = size_line_pixels(line) {
            largest = Some(largest.map(|current| current.max(pixels)).unwrap_or(pixels));
        }
    }
    largest
}

/// Refuse a page whose rendered bytes exceed the OCR encode budget.
fn check_image_size(image: &[u8]) -> Result<(), ExtractionFailure> {
    if image.len() > MAX_RENDER_BYTES {
        return Err(transient(
            "render",
            format!(
                "rendered image is {} bytes; the maximum is {MAX_RENDER_BYTES}",
                image.len()
            ),
        ));
    }
    Ok(())
}

fn pdf_page_count(pdf_path: &Path) -> Result<i64, ExtractionFailure> {
    let output = run_with_deadline("pdfinfo", Command::new("pdfinfo").arg(pdf_path), RENDER_DEADLINE_US)?;
    if !output.status.success() {
        return Err(transient(
            "render",
            format!(
                "pdfinfo exited with {}: {}",
                output.status,
                stderr_excerpt(&output.stderr)
            ),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pages = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Pages:").map(str::trim))
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| transient("render", "pdfinfo reported no usable page count"))?;
    if pages < 1 {
        return Err(transient(
            "render",
            format!("pdfinfo reported {pages} pages"),
        ));
    }
    if pages > MAX_PDF_PAGES {
        return Err(transient(
            "render",
            format!("pdfinfo reported {pages} pages; the maximum is {MAX_PDF_PAGES}"),
        ));
    }
    // The size check runs before any render: a raster that would overflow
    // the server never starts. Non-uniform PDFs print one size line per
    // distinct page size, so the maximum covers the largest page.
    let Some(pixels) = render_pixel_max(&stdout) else {
        return Err(transient("render", "pdfinfo reported no usable page size"));
    };
    if pixels > MAX_RENDER_PIXELS {
        return Err(transient(
            "render",
            format!(
                "the page box renders about {pixels} pixels; the maximum is {MAX_RENDER_PIXELS}"
            ),
        ));
    }
    Ok(pages)
}

fn render_page(pdf_path: &Path, dir: &Path, page: i64) -> Result<Vec<u8>, ExtractionFailure> {
    let prefix = dir.join(format!("page-{page}"));
    let output = run_with_deadline(
        "pdftoppm",
        Command::new("pdftoppm")
            .args([
                "-png",
                "-r",
                RENDER_DPI,
                "-f",
                &page.to_string(),
                "-l",
                &page.to_string(),
                "-singlefile",
            ])
            .arg(pdf_path)
            .arg(&prefix),
        RENDER_DEADLINE_US,
    )?;
    if !output.status.success() {
        return Err(transient(
            "render",
            format!(
                "pdftoppm exited with {}: {}",
                output.status,
                stderr_excerpt(&output.stderr)
            ),
        ));
    }
    let image_path = dir.join(format!("page-{page}.png"));
    let image = std::fs::read(&image_path).map_err(|error| {
        transient(
            "render",
            format!("pdftoppm produced no image for page {page}: {error}"),
        )
    })?;
    // The byte cap is checked here, before the base64 OCR encode, so an
    // oversized raster never leaves the render directory.
    check_image_size(&image)?;
    if image.is_empty() {
        return Err(transient(
            "render",
            format!("pdftoppm produced an empty image for page {page}"),
        ));
    }
    Ok(image)
}

/// The pages a previous claim already transcribed, in page order. The rows
/// are provisional — only the fenced commit publishes them — and a resumed
/// claim continues after them instead of re-paying the OCR work.
fn transcribed_pages(
    conn: &Connection,
    attachment_id: i64,
) -> Result<Vec<(i64, String)>, ExtractionFailure> {
    let pages: Vec<(i64, String)> = conn
        .prepare("SELECT page, text FROM attachment_text WHERE attachment_id=?1 ORDER BY page")
        .map_err(|error| {
            transient("storage", format!("cannot read transcription checkpoints: {error}"))
        })?
        .query_map(
            [attachment_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|error| {
            transient("storage", format!("cannot read transcription checkpoints: {error}"))
        })?
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    Ok(pages)
}

/// One durable row per done page, written only while this attempt still owns
/// the lease. The fenced commit deletes and rewrites the rows, so a partial
/// attempt leaves nothing behind but its own valid transcriptions.
fn checkpoint_page(
    conn: &Connection,
    job: &AttachmentJob,
    page: i64,
    text: &str,
    now_us: i64,
) -> Result<(), ExtractionFailure> {
    let mut chars = 0_i64;
    for (_, _) in text.char_indices() {
        chars += 1;
    }
    conn.execute(
        "INSERT INTO attachment_text(attachment_id,page,text,chars)
         SELECT ?1, ?2, ?3, ?4
         WHERE EXISTS(SELECT 1 FROM attachment_job j
                      JOIN attachment a ON a.id=j.attachment_id
                      WHERE j.attachment_id=?1 AND j.state='leased'
                        AND j.lease_token=?5 AND j.lease_epoch=?6 AND j.lease_until_us>?7)",
        rusqlite::params![
            job.attachment_id,
            page,
            text,
            chars,
            job.lease.token.to_string(),
            job.lease.epoch,
            current_us(now_us),
        ],
    )
    .map_err(|error| {
        transient("storage", format!("cannot checkpoint the transcribed page: {error}"))
    })?;
    Ok(())
}

fn stderr_excerpt(stderr: &[u8]) -> String {
    let excerpt: String = String::from_utf8_lossy(stderr)
        .trim()
        .chars()
        .take(300)
        .collect();
    if excerpt.is_empty() {
        "no diagnostic".to_owned()
    } else {
        excerpt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::VISION_TIMEOUT_US;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn text_decode_removes_a_bom_and_normalizes_crlf() {
        let pages = extract_text("\u{feff}line one\r\nline two\r\n".as_bytes())
            .expect("valid UTF-8 decodes");
        assert_eq!(pages, vec![(1, "line one\nline two\n".to_owned())]);
    }

    #[test]
    fn invalid_utf8_is_a_terminal_decode_failure() {
        let error = extract_text(&[0xff, 0xfe, 0x00]).expect_err("invalid UTF-8 must fail");
        assert_eq!(error.stage, "decode");
        assert!(
            error.dead,
            "a byte sequence that cannot decode never becomes valid"
        );
        assert!(error.reason.contains("UTF-8"), "{}", error.reason);
    }

    #[test]
    fn text_without_crlf_is_unchanged() {
        let pages = extract_text(b"already\nnormalized").expect("plain text decodes");
        assert_eq!(pages, vec![(1, "already\nnormalized".to_owned())]);
    }

    #[test]
    fn the_attempt_bound_matches_the_dead_letter_bound() {
        // The claim SQL refuses an eighth retry only after the eighth attempt
        // ran; this pins the worker constant to the repository's bound so the
        // two cannot drift apart.
        assert_eq!(MAX_ATTEMPTS, 8);
    }

    #[test]
    fn the_render_and_vision_windows_fit_inside_the_lease() {
        // The lease renews only after a transcription and its render return.
        // A render or a vision request that outlived the lease would fail
        // the renewal fence and discard the page, so every in-flight window
        // must stay safely below the claim lease. The vision deadline used
        // to be 60 seconds against a 30-second lease, which lost successful
        // responses at the fence; the two deadlines now fit with a margin.
        assert!(
            RENDER_DEADLINE_US + VISION_TIMEOUT_US < LEASE_US,
            "every in-flight render and vision request must fit inside the claim lease"
        );
        assert!(
            VISION_TIMEOUT_US < LEASE_US,
            "a live vision request must always fit inside the claim lease"
        );
    }

    struct CountingOcr {
        calls: AtomicUsize,
    }

    impl OcrProvider for CountingOcr {
        fn transcribe_page(&self, _image: &[u8], _mime: &str) -> Result<String, OcrError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(format!("transcribed page {call}").into())
        }
    }

    #[test]
    fn pages_per_turn_defer_and_resume_without_redoing_work() {
        // A twelve-page PDF costs one claim for the first eight pages, then
        // a deferral: the transcribed pages stay durable, and the next claim
        // resumes at page nine without re-transcribing pages one through
        // eight. The renderers are fake shell scripts; the database is a
        // real graph file, so the claim, fence, checkpoint and commit paths
        // all run.
        use mcpmem_core::events::now_us;

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(
            bin.join("pdfinfo"),
            b"#!/bin/sh\nprintf 'Pages: 12\\nPage size: 612 x 792 pts\\n'\n",
        )
        .unwrap();
        std::fs::write(
            bin.join("pdftoppm"),
            b"#!/usr/bin/env bash\nprintf x > \"${@: -1}.png\"\n",
        )
        .unwrap();
        for tool in ["pdfinfo", "pdftoppm"] {
            let path = bin.join(tool);
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        let prior_path = std::env::var("PATH").unwrap_or_default();
        let combined = format!("{}:{}", bin.to_string_lossy(), prior_path);
        // SAFETY: only this test spawns renderers by name; the value is
        // restored before the test returns.
        unsafe {
            std::env::set_var("PATH", &combined);
        }

        let db = dir.path().join("graph.sqlite");
        let conn = Connection::open(&db).unwrap();
        mcpmem_core::schema::initialize_database(&conn).unwrap();
        conn.execute(
            "INSERT INTO type_dict(id,kind,name,count,revision) VALUES(3,0,'note',0,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entity(id,name_hash,name,type_id,created_us,updated_us) \
             VALUES(1,1,'doc',3,1,1)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO entity_revision VALUES(1,1,0)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO attachment(id,entity_id,filename,mime,size_bytes,sha256,content,status,\
             revision,last_error,error_stage,created_us) \
             VALUES(1,1,'scan.pdf','application/pdf',1,zeroblob(32),X'62','uploaded',1,NULL,NULL,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attachment_job VALUES(1,'pending',NULL,0,0,0,0,NULL)",
            [],
        )
        .unwrap();
        drop(conn);

        let counter = Arc::new(CountingOcr {
            calls: AtomicUsize::new(0),
        });
        let ocr_ref = Arc::clone(&counter);
        let ocr = counter as Arc<dyn OcrProvider>;
        let worker = ExtractionWorker::new(&db, Some(Arc::clone(&ocr)));
        let first = worker.run_once(now_us() - 10_000_000).unwrap();
        assert_eq!(first.claimed, 1);
        assert_eq!(first.committed, 0);
        assert_eq!(first.retried, 0, "the turn bound is not a failure");
        assert_eq!(first.dead, 0);
        assert_eq!(
            ocr_ref.calls.load(Ordering::SeqCst),
            8,
            "one turn transcribes at most eight pages"
        );
        let conn = Connection::open(&db).unwrap();
        let transcribed: i64 = conn
            .query_row(
                "SELECT count(*) FROM attachment_text WHERE attachment_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(transcribed, 8, "every done page is checkpointed durably");
        assert_eq!(
            conn.query_row(
                "SELECT state FROM attachment_job WHERE attachment_id=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "leased",
            "the deferred job keeps its lease until it expires"
        );
        drop(conn);

        // The claim after the lease expires resumes at page nine: pages one
        // through eight are not transcribed again — only four more calls,
        // and the remaining pages complete in the same claim.
        let worker = ExtractionWorker::new(&db, Some(Arc::clone(&ocr)));
        let second = worker.run_once(now_us() + 45_000_000).unwrap();
        assert_eq!(second.claimed, 1);
        assert_eq!(second.committed, 1, "the resumed claim completes the PDF");
        assert_eq!(second.retried, 0);
        assert_eq!(second.dead, 0);
        assert_eq!(
            ocr_ref.calls.load(Ordering::SeqCst),
            12,
            "the resumed claim never re-transcribes checkpointed pages"
        );
        let conn = Connection::open(&db).unwrap();
        let (status, revision): (String, i64) = conn
            .query_row(
                "SELECT status, revision FROM attachment WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "ready");
        assert_eq!(revision, 2, "the fenced commit bumps the revision once");
        let pages = conn
            .prepare("SELECT text FROM attachment_text WHERE attachment_id=1 ORDER BY page")
            .unwrap()
            .query_map([], |row| Ok(row.get::<_, String>(0)))
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(pages.len(), 12, "all twelve pages commit in order");
        assert_eq!(
            conn.query_row(
                "SELECT state FROM attachment_job WHERE attachment_id=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "done"
        );
        drop(conn);

        unsafe {
            std::env::set_var("PATH", &prior_path);
        }
    }

    #[test]
    fn renderer_deadline_kills_a_stalled_renderer() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("stalled-renderer");
        std::fs::write(&script, b"#!/bin/sh\nsleep 30\n").unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let started = std::time::Instant::now();
        let mut cmd = Command::new(&script);
        let error = run_with_deadline("pdftoppm", &mut cmd, 1_000_000)
            .expect_err("a stalled renderer must be killed at the deadline");
        assert_eq!(error.stage, "render");
        assert!(error.reason.contains("deadline"), "{}", error.reason);
        assert!(
            std::time::Instant::now() < started + std::time::Duration::from_secs(5),
            "the deadline kills the child instead of waiting for it"
        );
    }

    #[test]
    fn oversized_rendered_bytes_are_refused() {
        let chunk = [0_u8; 4096];
        let mut image = Vec::<u8>::with_capacity(MAX_RENDER_BYTES + 1);
        while image.len() <= MAX_RENDER_BYTES {
            image.extend_from_slice(&chunk);
        }
        let error = check_image_size(&image).expect_err("an oversized image must be refused");
        assert_eq!(error.stage, "render");
        assert!(error.reason.contains("maximum is"), "{}", error.reason);
        assert_eq!(
            image[..MAX_RENDER_BYTES].len(),
            MAX_RENDER_BYTES,
            "the test exercises the exact cap boundary"
        );
        assert!(
            check_image_size(&image[..MAX_RENDER_BYTES]).is_ok(),
            "a page at the cap is allowed"
        );
    }
}
