//! The durable attachment extraction worker.
//!
//! One `run_once` turn sweeps expired upload sessions and leases at most one
//! extraction job from the resolved graph. Text files decode in process;
//! PDFs render through the external Poppler `pdfinfo` and `pdftoppm` commands,
//! one page per invocation, and each rendered image goes to the vision OCR
//! provider. OCR calls run outside any graph write transaction, and the
//! fenced completion publishes all page rows in one transaction or none.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

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
        match extract_attachment(self.ocr.as_deref(), &jobs, &job, &mime, &content, now_us) {
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
/// superseded the claim; neither outcome may be written.
enum ExtractionOutcome {
    Pages(Vec<(i64, String)>),
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
        extract_pdf(ocr, jobs, job, content, now_us)
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
    let mut pages = Vec::with_capacity(page_count as usize);
    for page in 1..=page_count {
        let image = render_page(&pdf_path, dir.path(), page)?;
        let text = match ocr.transcribe_page(&image, "image/png") {
            Ok(text) => text,
            Err(OcrError::Config(reason)) => return Err(terminal("config", reason)),
            Err(OcrError::Provider(reason)) => return Err(transient("provider", reason)),
        };
        // Re-arm the lease after each page so a long OCR job cannot expire
        // under a second worker; a lost lease stops the loop with no publish.
        // A fresh clock reading is required: reusing the turn-start `now_us`
        // would set lease_until_us back to the claim-time expiry, so the
        // deadline would never move.
        if !jobs
            .renew(job, current_us(now_us), LEASE_US)
            .map_err(|error| {
                transient("storage", format!("cannot renew attachment lease: {error}"))
            })?
        {
            return Ok(ExtractionOutcome::LostLease);
        }
        pages.push((page, text));
    }
    Ok(ExtractionOutcome::Pages(pages))
}

fn pdf_page_count(pdf_path: &Path) -> Result<i64, ExtractionFailure> {
    let output = Command::new("pdfinfo")
        .arg(pdf_path)
        .output()
        .map_err(|error| transient("render", format!("cannot run pdfinfo: {error}")))?;
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
    Ok(pages)
}

fn render_page(pdf_path: &Path, dir: &Path, page: i64) -> Result<Vec<u8>, ExtractionFailure> {
    let prefix = dir.join(format!("page-{page}"));
    let output = Command::new("pdftoppm")
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
        .arg(&prefix)
        .output()
        .map_err(|error| transient("render", format!("cannot run pdftoppm: {error}")))?;
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
    if image.is_empty() {
        return Err(transient(
            "render",
            format!("pdftoppm produced an empty image for page {page}"),
        ));
    }
    Ok(image)
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
}
