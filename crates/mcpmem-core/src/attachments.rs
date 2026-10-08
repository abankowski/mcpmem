//! Durable uploads and fenced attachment extraction.

use std::io::{Read, Write};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::errors::MCSError;
use crate::events::{Lease, lease_until, sql_error};
use crate::graph::TxGuard;
use crate::jobs::{OwnerKind, enqueue_chunk_change};

pub const CHUNK_BYTES: usize = 1_048_576;
pub const SEGMENT_CHARS: usize = 1_600;
pub const UPLOAD_TTL_US: i64 = 3_600_000_000;

#[derive(Clone, Debug)]
pub struct AttachmentLimits {
    pub max_bytes: i64,
    pub workspace_byte_budget: i64,
    pub allow_mime: Vec<String>,
}

#[derive(Debug, Error)]
pub enum AttachmentError {
    #[error("attachment filename must not be empty")]
    Filename,
    #[error("duplicate attachment filename on this entity")]
    DuplicateFilename,
    #[error("attachment MIME type is not allowed")]
    Mime,
    #[error("attachment exceeds the per-file size limit")]
    Size,
    #[error("workspace attachment byte budget exceeded")]
    WorkspaceBudget,
    #[error("attachment or upload session not found")]
    NotFound,
    #[error("upload session belongs to another principal")]
    WrongPrincipal,
    #[error("upload session expired")]
    ExpiredSession,
    #[error("attachment chunks are out of order or replay contents differ")]
    Order,
    #[error("attachment chunk must not be empty")]
    EmptyChunk,
    #[error("attachment byte count is incomplete or differs from the declaration")]
    Incomplete,
    #[error("attachment SHA-256 does not match")]
    HashMismatch,
    #[error("attachment storage failed: {0}")]
    Storage(#[from] MCSError),
}

pub type AttachmentResult<T> = std::result::Result<T, AttachmentError>;

impl From<AttachmentError> for MCSError {
    fn from(error: AttachmentError) -> Self {
        match error {
            AttachmentError::Storage(source) => source,
            other => MCSError::InvalidParams(other.to_string()),
        }
    }
}

fn db(error: rusqlite::Error) -> AttachmentError {
    AttachmentError::Storage(sql_error(error))
}

const fn io(error: std::io::Error) -> AttachmentError {
    AttachmentError::Storage(MCSError::IoError(error))
}

fn validate_file(
    conn: &Connection,
    entity_id: i64,
    filename: &str,
    mime: &str,
    size: i64,
    limits: &AttachmentLimits,
) -> AttachmentResult<()> {
    if filename.trim().is_empty() {
        return Err(AttachmentError::Filename);
    }
    if !limits.allow_mime.iter().any(|rule| {
        rule == mime
            || rule.strip_suffix("/*").is_some_and(|prefix| {
                mime.strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            })
    }) {
        return Err(AttachmentError::Mime);
    }
    if size < 0 || limits.max_bytes < 0 || size > limits.max_bytes {
        return Err(AttachmentError::Size);
    }
    if limits.workspace_byte_budget < 0 {
        return Err(AttachmentError::WorkspaceBudget);
    }
    let live: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM entity e JOIN entity_revision r ON r.entity_id=e.id
             WHERE e.id=?1 AND e.flags=0 AND r.deleted=0)",
            [entity_id],
            |row| row.get(0),
        )
        .map_err(db)?;
    if !live {
        return Err(AttachmentError::NotFound);
    }
    let duplicate: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM attachment WHERE entity_id=?1 AND filename=?2)",
            params![entity_id, filename],
            |row| row.get(0),
        )
        .map_err(db)?;
    if duplicate {
        return Err(AttachmentError::DuplicateFilename);
    }
    Ok(())
}

fn check_budget(
    conn: &Connection,
    size: i64,
    excluded_upload: Option<Uuid>,
    now_us: i64,
    limits: &AttachmentLimits,
) -> AttachmentResult<()> {
    let total: i64 = conn
        .query_row(
            "SELECT
                (SELECT COALESCE(SUM(size_bytes),0) FROM attachment)
                + (SELECT COALESCE(SUM(expected_bytes),0) FROM attachment_upload
                   WHERE attachment_id IS NULL AND upload_id!=?1 AND expires_us>?2)",
            params![
                excluded_upload.unwrap_or_else(Uuid::nil).to_string(),
                now_us
            ],
            |row| row.get(0),
        )
        .map_err(db)?;
    if size > limits.workspace_byte_budget.saturating_sub(total) {
        return Err(AttachmentError::WorkspaceBudget);
    }
    Ok(())
}

struct UploadSession {
    principal: String,
    entity_id: i64,
    filename: String,
    mime: String,
    expected_bytes: i64,
    sha256: [u8; 32],
    received_bytes: i64,
    next_index: i64,
    expires_us: i64,
    attachment_id: Option<i64>,
}

fn session(conn: &Connection, id: Uuid, principal: &str) -> AttachmentResult<UploadSession> {
    let row = conn
        .query_row(
            "SELECT principal_id,entity_id,filename,mime,expected_bytes,expected_sha256,
                    received_bytes,next_index,expires_us,attachment_id
             FROM attachment_upload WHERE upload_id=?1",
            [id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                ))
            },
        )
        .optional()
        .map_err(db)?
        .ok_or(AttachmentError::NotFound)?;
    if row.0 != principal {
        return Err(AttachmentError::WrongPrincipal);
    }
    let digest: [u8; 32] = row.5.try_into().map_err(|_| {
        AttachmentError::Storage(MCSError::MemoryError("invalid stored upload digest".into()))
    })?;
    Ok(UploadSession {
        principal: row.0,
        entity_id: row.1,
        filename: row.2,
        mime: row.3,
        expected_bytes: row.4,
        sha256: digest,
        received_bytes: row.6,
        next_index: row.7,
        expires_us: row.8,
        attachment_id: row.9,
    })
}

// Ten distinct validated inputs cross the finalization boundary; a struct
// here would only move the same arity problem to callers of the pinned API.
#[allow(clippy::too_many_arguments)]
fn write_attachment(
    conn: &Connection,
    entity_id: i64,
    filename: &str,
    mime: &str,
    reader: &mut impl Read,
    expected_bytes: i64,
    expected_sha256: &[u8; 32],
    limits: &AttachmentLimits,
    now_us: i64,
    excluded_upload: Option<Uuid>,
) -> AttachmentResult<i64> {
    validate_file(conn, entity_id, filename, mime, expected_bytes, limits)?;
    check_budget(conn, expected_bytes, excluded_upload, now_us, limits)?;
    conn.execute(
        "INSERT INTO attachment(entity_id,filename,mime,size_bytes,sha256,content,status,revision,created_us)
         VALUES(?1,?2,?3,?4,?5,zeroblob(?4),'uploaded',1,?6)",
        params![entity_id, filename, mime, expected_bytes, expected_sha256.as_slice(), now_us],
    )
    .map_err(db)?;
    let attachment_id = conn.last_insert_rowid();
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut left = expected_bytes;
    if left > 0 {
        let mut blob = conn
            .blob_open("main", "attachment", "content", attachment_id, false)
            .map_err(db)?;
        while left > 0 {
            let amount = buffer.len().min(left as usize);
            let read = reader.read(&mut buffer[..amount]).map_err(io)?;
            if read == 0 {
                return Err(AttachmentError::Incomplete);
            }
            blob.write_all(&buffer[..read]).map_err(io)?;
            hasher.update(&buffer[..read]);
            left -= read as i64;
        }
        drop(blob);
    }
    if reader.read(&mut buffer[..1]).map_err(io)? != 0 {
        return Err(AttachmentError::Incomplete);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    if &digest != expected_sha256 {
        return Err(AttachmentError::HashMismatch);
    }
    conn.execute(
        "INSERT INTO attachment_job(attachment_id,state,lease_epoch,lease_until_us,next_attempt_us,attempts)
         VALUES(?1,'pending',0,0,0,0)",
        [attachment_id],
    )
    .map_err(db)?;
    Ok(attachment_id)
}

fn upload_key(upload_id: Uuid) -> String {
    upload_id.to_string()
}

struct ChunkReader<'a> {
    conn: &'a Connection,
    upload_key: String,
    next: i64,
    remaining_chunks: i64,
    content: Vec<u8>,
    offset: usize,
}

impl Read for ChunkReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.offset < self.content.len() {
                let amount = out.len().min(self.content.len() - self.offset);
                out[..amount].copy_from_slice(&self.content[self.offset..self.offset + amount]);
                self.offset += amount;
                return Ok(amount);
            }
            if self.remaining_chunks == 0 {
                return Ok(0);
            }
            self.content = self.conn.query_row(
                "SELECT content FROM attachment_upload_chunk WHERE upload_id=?1 AND chunk_index=?2",
                params![self.upload_key.clone(), self.next], |row| row.get(0),
            ).map_err(|error| std::io::Error::other(error.to_string()))?;
            self.next += 1;
            self.remaining_chunks -= 1;
            self.offset = 0;
        }
    }
}

pub struct AttachmentRepository<'a> {
    conn: &'a Connection,
}

impl<'a> AttachmentRepository<'a> {
    pub const fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// The caller must pass `expires_us = now_us + UPLOAD_TTL_US` for a
    /// one-hour session; quota admission uses the derived admission time
    /// `expires_us - UPLOAD_TTL_US`. New code must call [`begin_upload_at`]
    /// and pass the admission time explicitly.
    /// The caller must resolve the graph and grant before it calls this method.
    // Eight distinct values are the approved MCP wire contract (R2, R16);
    // an options struct would rename, not reduce, the seam.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_upload(
        &self,
        principal_id: &str,
        entity_id: i64,
        filename: &str,
        mime: &str,
        expected_bytes: i64,
        expected_sha256: &[u8; 32],
        expires_us: i64,
        limits: &AttachmentLimits,
    ) -> AttachmentResult<Uuid> {
        self.begin_upload_at(
            principal_id,
            entity_id,
            filename,
            mime,
            expected_bytes,
            expected_sha256,
            expires_us.saturating_sub(UPLOAD_TTL_US),
            expires_us,
            limits,
        )
    }

    /// The caller passes the admission time `now_us`; the quota counts only
    /// sessions whose `expires_us` is still after `now_us`, so an expired
    /// reservation never blocks a new upload without the extractor sweep.
    /// The caller must pass `expires_us = now_us + UPLOAD_TTL_US` for a
    /// one-hour session.
    /// The caller must resolve the graph and grant before it calls this method.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_upload_at(
        &self,
        principal_id: &str,
        entity_id: i64,
        filename: &str,
        mime: &str,
        expected_bytes: i64,
        expected_sha256: &[u8; 32],
        now_us: i64,
        expires_us: i64,
        limits: &AttachmentLimits,
    ) -> AttachmentResult<Uuid> {
        let tx = TxGuard::begin(self.conn)?;
        validate_file(self.conn, entity_id, filename, mime, expected_bytes, limits)?;
        check_budget(self.conn, expected_bytes, None, now_us, limits)?;
        let upload_id = Uuid::new_v4();
        self.conn
            .execute(
                "INSERT INTO attachment_upload(upload_id,principal_id,entity_id,filename,mime,
             expected_bytes,expected_sha256,received_bytes,next_index,expires_us)
             VALUES(?1,?2,?3,?4,?5,?6,?7,0,0,?8)",
                params![
                    upload_id.to_string(),
                    principal_id,
                    entity_id,
                    filename,
                    mime,
                    expected_bytes,
                    expected_sha256.as_slice(),
                    expires_us
                ],
            )
            .map_err(db)?;
        tx.commit()?;
        Ok(upload_id)
    }

    pub fn append_chunk(
        &self,
        principal_id: &str,
        upload_id: Uuid,
        index: i64,
        content: &[u8],
        now_us: i64,
    ) -> AttachmentResult<(i64, i64)> {
        if content.len() > CHUNK_BYTES {
            return Err(AttachmentError::Size);
        }
        if content.is_empty() {
            return Err(AttachmentError::EmptyChunk);
        }
        let key = upload_key(upload_id);
        let tx = TxGuard::begin(self.conn)?;
        let upload = session(self.conn, upload_id, principal_id)?;
        if upload.attachment_id.is_some() {
            return Err(AttachmentError::Order);
        }
        if upload.expires_us <= now_us {
            return Err(AttachmentError::ExpiredSession);
        }
        let live: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entity e JOIN entity_revision r ON r.entity_id=e.id
             WHERE e.id=?1 AND e.flags=0 AND r.deleted=0)",
                [upload.entity_id],
                |r| r.get(0),
            )
            .map_err(db)?;
        if !live {
            return Err(AttachmentError::NotFound);
        }
        if index == upload.next_index - 1 {
            let exact: bool = self
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM attachment_upload_chunk WHERE upload_id=?1
                 AND chunk_index=?2 AND content=?3)",
                    params![key, index, content],
                    |r| r.get(0),
                )
                .map_err(db)?;
            if exact {
                tx.commit()?;
                return Ok((upload.next_index, upload.received_bytes));
            }
        }
        if index != upload.next_index || index < 0 {
            return Err(AttachmentError::Order);
        }
        let received_bytes = upload
            .received_bytes
            .checked_add(content.len() as i64)
            .ok_or(AttachmentError::Size)?;
        if received_bytes > upload.expected_bytes {
            return Err(AttachmentError::Size);
        }
        self.conn.execute(
            "INSERT INTO attachment_upload_chunk(upload_id,chunk_index,content) VALUES(?1,?2,?3)",
            params![key, index, content],
        ).map_err(db)?;
        self.conn.execute(
            "UPDATE attachment_upload SET next_index=next_index+1,received_bytes=?2 WHERE upload_id=?1",
            params![upload_id.to_string(), received_bytes],
        ).map_err(db)?;
        tx.commit()?;
        Ok((upload.next_index + 1, received_bytes))
    }

    pub fn finish_upload(
        &self,
        principal_id: &str,
        upload_id: Uuid,
        now_us: i64,
        limits: &AttachmentLimits,
    ) -> AttachmentResult<i64> {
        let tx = TxGuard::begin(self.conn)?;
        let upload = session(self.conn, upload_id, principal_id)?;
        if let Some(id) = upload.attachment_id {
            tx.commit()?;
            return Ok(id);
        }
        if upload.expires_us <= now_us {
            return Err(AttachmentError::ExpiredSession);
        }
        if upload.received_bytes != upload.expected_bytes {
            return Err(AttachmentError::Incomplete);
        }
        let mut reader = ChunkReader {
            conn: self.conn,
            upload_key: upload_key(upload_id),
            next: 0,
            remaining_chunks: upload.next_index,
            content: Vec::new(),
            offset: 0,
        };
        let id = write_attachment(
            self.conn,
            upload.entity_id,
            &upload.filename,
            &upload.mime,
            &mut reader,
            upload.expected_bytes,
            &upload.sha256,
            limits,
            now_us,
            Some(upload_id),
        )?;
        self.conn
            .execute(
                "DELETE FROM attachment_upload_chunk WHERE upload_id=?1",
                [upload_id.to_string()],
            )
            .map_err(db)?;
        self.conn.execute(
            "UPDATE attachment_upload SET attachment_id=?2 WHERE upload_id=?1 AND principal_id=?3",
            params![upload_id.to_string(), id, upload.principal],
        ).map_err(db)?;
        tx.commit()?;
        Ok(id)
    }

    pub fn cancel_upload(
        &self,
        principal_id: &str,
        upload_id: Uuid,
        // Kept for interface stability; the owner may cancel at any time.
        #[allow(unused_variables)] now_us: i64,
    ) -> AttachmentResult<()> {
        let tx = TxGuard::begin(self.conn)?;
        session(self.conn, upload_id, principal_id)?;
        // The owning principal may cancel an expired unfinished session; the
        // reservation must not wait for the optional extractor sweep.
        self.conn
            .execute(
                "DELETE FROM attachment_upload_chunk WHERE upload_id=?1",
                [upload_id.to_string()],
            )
            .map_err(db)?;
        self.conn
            .execute(
                "DELETE FROM attachment_upload WHERE upload_id=?1",
                [upload_id.to_string()],
            )
            .map_err(db)?;
        tx.commit()?;
        Ok(())
    }

    pub fn expire_uploads(&self, now_us: i64) -> AttachmentResult<u64> {
        let tx = TxGuard::begin(self.conn)?;
        self.conn.execute(
            "DELETE FROM attachment_upload_chunk WHERE upload_id IN
             (SELECT upload_id FROM attachment_upload WHERE attachment_id IS NULL AND expires_us<=?1)",
            [now_us],
        ).map_err(db)?;
        let removed = self
            .conn
            .execute(
                "DELETE FROM attachment_upload WHERE attachment_id IS NULL AND expires_us<=?1",
                [now_us],
            )
            .map_err(db)?;
        tx.commit()?;
        Ok(removed as u64)
    }

    // The pinned HTTP spool contract (R1, R16) carries nine values; all are
    // distinct and all cross the boundary at once.
    #[allow(clippy::too_many_arguments)]
    pub fn store_reader(
        &self,
        entity_id: i64,
        filename: &str,
        mime: &str,
        reader: &mut impl Read,
        expected_bytes: i64,
        expected_sha256: &[u8; 32],
        limits: &AttachmentLimits,
        now_us: i64,
    ) -> AttachmentResult<i64> {
        let tx = TxGuard::begin(self.conn)?;
        let id = write_attachment(
            self.conn,
            entity_id,
            filename,
            mime,
            reader,
            expected_bytes,
            expected_sha256,
            limits,
            now_us,
            None,
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn delete_attachment(&self, attachment_id: i64) -> AttachmentResult<()> {
        let tx = TxGuard::begin(self.conn)?;
        delete_attachment_rows(self.conn, attachment_id)?;
        tx.commit()?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct AttachmentJob {
    pub attachment_id: i64,
    pub revision: i64,
    pub lease: Lease,
    pub attempts: i64,
}

pub struct AttachmentJobRepository<'a> {
    conn: &'a Connection,
}

impl<'a> AttachmentJobRepository<'a> {
    pub const fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn claim_due(&self, now_us: i64, lease_us: i64) -> AttachmentResult<Option<AttachmentJob>> {
        let until = lease_until(now_us, lease_us)?;
        let tx = TxGuard::begin(self.conn)?;
        self.conn
            .execute(
                "UPDATE attachment SET status='error',
             error_stage=COALESCE(error_stage,'storage'),
             last_error=COALESCE(last_error,'extraction attempt limit exceeded')
             WHERE id IN (SELECT attachment_id FROM attachment_job
                          WHERE state='leased' AND lease_until_us<=?1 AND attempts>=8)",
                [now_us],
            )
            .map_err(db)?;
        self.conn
            .execute(
                "UPDATE attachment_job SET state='dead',lease_token=NULL,
             last_error=COALESCE(last_error,'extraction attempt limit exceeded')
             WHERE state='leased' AND lease_until_us<=?1 AND attempts>=8",
                [now_us],
            )
            .map_err(db)?;
        let found = self
            .conn
            .query_row(
                "SELECT j.attachment_id,a.revision,j.lease_epoch,j.attempts
             FROM attachment_job j JOIN attachment a ON a.id=j.attachment_id
             JOIN entity e ON e.id=a.entity_id JOIN entity_revision r ON r.entity_id=e.id
             WHERE e.flags=0 AND r.deleted=0 AND a.status IN ('uploaded','extracting')
             AND j.attempts<8
             AND ((j.state='pending' AND j.next_attempt_us<=?1)
                  OR (j.state='leased' AND j.lease_until_us<=?1))
             ORDER BY j.next_attempt_us,j.attachment_id LIMIT 1",
                [now_us],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(db)?;
        let result = if let Some((attachment_id, revision, epoch, attempts)) = found {
            let token = Uuid::new_v4();
            self.conn.execute(
                "UPDATE attachment_job SET state='leased',lease_token=?2,lease_epoch=lease_epoch+1,
                 lease_until_us=?3,attempts=attempts+1 WHERE attachment_id=?1",
                params![attachment_id,token.to_string(),until],
            ).map_err(db)?;
            self.conn
                .execute(
                    "UPDATE attachment SET status='extracting' WHERE id=?1",
                    [attachment_id],
                )
                .map_err(db)?;
            Some(AttachmentJob {
                attachment_id,
                revision,
                lease: Lease {
                    token,
                    epoch: epoch + 1,
                    until_us: until,
                },
                attempts: attempts + 1,
            })
        } else {
            None
        };
        tx.commit()?;
        Ok(result)
    }

    pub fn next_due_us(&self, _now_us: i64) -> AttachmentResult<Option<i64>> {
        self.conn
            .query_row(
                "SELECT MIN(due_us) FROM (
                     SELECT j.next_attempt_us AS due_us
                     FROM attachment_job j
                     JOIN attachment a ON a.id=j.attachment_id
                     JOIN entity e ON e.id=a.entity_id
                     JOIN entity_revision r ON r.entity_id=e.id
                     WHERE e.flags=0 AND r.deleted=0
                       AND a.status IN ('uploaded','extracting')
                       AND j.attempts<8 AND j.state='pending'
                     UNION ALL
                     SELECT j.lease_until_us
                     FROM attachment_job j
                     JOIN attachment a ON a.id=j.attachment_id
                     JOIN entity e ON e.id=a.entity_id
                     JOIN entity_revision r ON r.entity_id=e.id
                     WHERE e.flags=0 AND r.deleted=0
                       AND a.status IN ('uploaded','extracting')
                       AND j.state='leased'
                     UNION ALL
                     SELECT u.expires_us
                     FROM attachment_upload u
                     JOIN entity e ON e.id=u.entity_id
                     JOIN entity_revision r ON r.entity_id=e.id
                     WHERE u.attachment_id IS NULL AND e.flags=0 AND r.deleted=0
                 )",
                [],
                |row| row.get(0),
            )
            .map_err(db)
    }

    fn fenced(&self, job: &AttachmentJob, now_us: i64) -> AttachmentResult<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM attachment_job j
             JOIN attachment a ON a.id=j.attachment_id
             JOIN entity e ON e.id=a.entity_id
             JOIN entity_revision r ON r.entity_id=e.id
             WHERE j.attachment_id=?1 AND j.state='leased' AND j.lease_token=?2
               AND j.lease_epoch=?3 AND j.lease_until_us>?4
               AND a.revision=?5 AND a.status='extracting'
               AND e.flags=0 AND r.deleted=0)",
                params![
                    job.attachment_id,
                    job.lease.token.to_string(),
                    job.lease.epoch,
                    now_us,
                    job.revision
                ],
                |r| r.get(0),
            )
            .map_err(db)
    }

    pub fn renew(&self, job: &AttachmentJob, now_us: i64, lease_us: i64) -> AttachmentResult<bool> {
        let until = lease_until(now_us, lease_us)?;
        let tx = TxGuard::begin(self.conn)?;
        if !self.fenced(job, now_us)? {
            return Ok(false);
        }
        self.conn
            .execute(
                "UPDATE attachment_job SET lease_until_us=?2 WHERE attachment_id=?1",
                params![job.attachment_id, until],
            )
            .map_err(db)?;
        tx.commit()?;
        Ok(true)
    }

    pub fn complete(
        &self,
        job: &AttachmentJob,
        now_us: i64,
        pages: &[(i64, String)],
    ) -> AttachmentResult<bool> {
        let tx = TxGuard::begin(self.conn)?;
        if !self.fenced(job, now_us)? {
            return Ok(false);
        }
        self.conn
            .execute(
                "DELETE FROM attachment_chunk WHERE attachment_id=?1",
                [job.attachment_id],
            )
            .map_err(db)?;
        self.conn
            .execute(
                "DELETE FROM attachment_text WHERE attachment_id=?1",
                [job.attachment_id],
            )
            .map_err(db)?;
        let mut chunk_index = 0_i64;
        for (page, text) in pages {
            let mut chars = 0_i64;
            let mut start = 0;
            let mut segment_index = 0_i64;
            for (end, _) in text.char_indices() {
                if chars > 0 && chars % SEGMENT_CHARS as i64 == 0 {
                    self.conn.execute(
                        "INSERT INTO attachment_chunk(attachment_id,chunk_index,page,segment_index,text)
                         VALUES(?1,?2,?3,?4,?5)",
                        params![job.attachment_id,chunk_index,page,segment_index,&text[start..end]],
                    ).map_err(db)?;
                    segment_index += 1;
                    chunk_index += 1;
                    start = end;
                }
                chars += 1;
            }
            if start < text.len() {
                self.conn.execute(
                    "INSERT INTO attachment_chunk(attachment_id,chunk_index,page,segment_index,text)
                     VALUES(?1,?2,?3,?4,?5)",
                    params![job.attachment_id,chunk_index,page,segment_index,&text[start..]],
                ).map_err(db)?;
                chunk_index += 1;
            }
            self.conn.execute(
                "INSERT INTO attachment_text(attachment_id,page,text,chars) VALUES(?1,?2,?3,?4)",
                params![job.attachment_id,page,text,chars],
            ).map_err(db)?;
        }
        self.conn.execute(
            "UPDATE attachment SET status='ready',revision=revision+1,last_error=NULL,error_stage=NULL WHERE id=?1",
            [job.attachment_id],
        ).map_err(db)?;
        self.conn
            .execute(
                "UPDATE attachment_job SET state='done',lease_token=NULL WHERE attachment_id=?1",
                [job.attachment_id],
            )
            .map_err(db)?;
        enqueue_chunk_change(
            self.conn,
            OwnerKind::Attachment,
            job.attachment_id,
            job.revision + 1,
            false,
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn retry(
        &self,
        job: &AttachmentJob,
        now_us: i64,
        next_attempt_us: i64,
        stage: &str,
        reason: &str,
        dead: bool,
    ) -> AttachmentResult<bool> {
        if !matches!(
            stage,
            "config" | "render" | "provider" | "decode" | "storage"
        ) {
            return Err(AttachmentError::Storage(MCSError::InvalidParams(
                "invalid extraction stage".into(),
            )));
        }
        let tx = TxGuard::begin(self.conn)?;
        if !self.fenced(job, now_us)? {
            return Ok(false);
        }
        let dead = dead || job.attempts >= 8;
        self.conn.execute(
            "UPDATE attachment_job SET state=?2,next_attempt_us=?3,last_error=?4,lease_token=NULL
             WHERE attachment_id=?1",
            params![job.attachment_id, if dead {"dead"} else {"pending"},next_attempt_us,reason],
        ).map_err(db)?;
        self.conn
            .execute(
                "UPDATE attachment SET status=?2,error_stage=?3,last_error=?4 WHERE id=?1",
                params![
                    job.attachment_id,
                    if dead { "error" } else { "extracting" },
                    stage,
                    reason
                ],
            )
            .map_err(db)?;
        tx.commit()?;
        Ok(true)
    }
}

/// True when the entity owns stored attachment files or unfinished upload
/// sessions. The entity-delete cascade discards such rows, so mutation code
/// that deletes the entity must refuse first when the rows cannot move.
pub fn entity_has_attachments(conn: &Connection, entity_id: i64) -> AttachmentResult<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM attachment WHERE entity_id=?1)
              OR EXISTS(SELECT 1 FROM attachment_upload
                        WHERE entity_id=?1 AND attachment_id IS NULL)",
        [entity_id],
        |r| r.get(0),
    )
    .map_err(db)
}

/// Remove an entity's incomplete uploads inside its graph mutation transaction.
pub fn delete_incomplete_uploads_for_entity(
    conn: &Connection,
    entity_id: i64,
) -> AttachmentResult<()> {
    conn.execute(
        "DELETE FROM attachment_upload_chunk WHERE upload_id IN
         (SELECT upload_id FROM attachment_upload WHERE entity_id=?1 AND attachment_id IS NULL)",
        [entity_id],
    )
    .map_err(db)?;
    conn.execute(
        "DELETE FROM attachment_upload WHERE entity_id=?1 AND attachment_id IS NULL",
        [entity_id],
    )
    .map_err(db)?;
    Ok(())
}

/// Remove one attachment and invalidate its indexed profiles in the caller's transaction.
pub fn delete_attachment_rows(conn: &Connection, attachment_id: i64) -> AttachmentResult<()> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM attachment WHERE id=?1)",
            [attachment_id],
            |r| r.get(0),
        )
        .map_err(db)?;
    if !exists {
        return Err(AttachmentError::NotFound);
    }
    conn.execute(
        "UPDATE ann_generation SET durable_generation=durable_generation+1,full_scan_generation=NULL
         WHERE profile_id IN (
             SELECT profile_id FROM chunk_vector WHERE owner_kind='attachment' AND owner_id=?1
             UNION
             SELECT profile_id FROM chunk_index_job WHERE owner_kind='attachment' AND owner_id=?1
         )",
        [attachment_id],
    ).map_err(db)?;
    conn.execute(
        "DELETE FROM chunk_vector WHERE owner_kind='attachment' AND owner_id=?1",
        [attachment_id],
    )
    .map_err(db)?;
    conn.execute(
        "DELETE FROM chunk_index_job WHERE owner_kind='attachment' AND owner_id=?1",
        [attachment_id],
    )
    .map_err(db)?;
    conn.execute("DELETE FROM attachment_upload_chunk WHERE upload_id IN (SELECT upload_id FROM attachment_upload WHERE attachment_id=?1)", [attachment_id]).map_err(db)?;
    conn.execute(
        "DELETE FROM attachment_upload WHERE attachment_id=?1",
        [attachment_id],
    )
    .map_err(db)?;
    conn.execute(
        "DELETE FROM attachment_chunk WHERE attachment_id=?1",
        [attachment_id],
    )
    .map_err(db)?;
    conn.execute(
        "DELETE FROM attachment_text WHERE attachment_id=?1",
        [attachment_id],
    )
    .map_err(db)?;
    conn.execute(
        "DELETE FROM attachment_job WHERE attachment_id=?1",
        [attachment_id],
    )
    .map_err(db)?;
    conn.execute("DELETE FROM attachment WHERE id=?1", [attachment_id])
        .map_err(db)?;
    Ok(())
}

/// Keep attachment vector types equal to their live parent's type without new embeddings.
pub fn refresh_attachment_vector_types(
    conn: &Connection,
    entity_id: i64,
    type_id: i64,
) -> AttachmentResult<()> {
    conn.execute(
        "UPDATE ann_generation SET durable_generation=durable_generation+1,full_scan_generation=NULL
         WHERE profile_id IN (SELECT DISTINCT profile_id FROM chunk_vector
             WHERE owner_kind='attachment' AND owner_id IN
                 (SELECT id FROM attachment WHERE entity_id=?1) AND type_id!=?2)",
        params![entity_id,type_id],
    ).map_err(db)?;
    conn.execute(
        "UPDATE chunk_vector SET type_id=?2 WHERE owner_kind='attachment' AND type_id!=?2
         AND owner_id IN (SELECT id FROM attachment WHERE entity_id=?1)",
        params![entity_id, type_id],
    )
    .map_err(db)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::name_hash;
    use crate::jobs::{
        AnnGenerationRepository, IndexJobRepository, IndexProfileRegistry, OwnerKind,
    };
    use crate::schema::initialize_database;
    use rusqlite::params;
    use sha2::{Digest, Sha256};
    use std::io::{self, Cursor, Read};

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        initialize_database(&conn).unwrap();
        for (id, name) in [(1, "parent"), (2, "other")] {
            conn.execute("INSERT INTO entity(id,name_hash,name,type_id,created_us,updated_us) VALUES(?1,?2,?3,1,1,1)", params![id,name_hash(name),name]).unwrap();
            conn.execute("INSERT INTO entity_revision VALUES(?1,1,0)", [id])
                .unwrap();
        }
        conn
    }

    fn limits(max: i64, budget: i64) -> AttachmentLimits {
        AttachmentLimits {
            max_bytes: max,
            workspace_byte_budget: budget,
            allow_mime: vec!["text/*".into(), "application/pdf".into()],
        }
    }

    fn hash(data: &[u8]) -> [u8; 32] {
        Sha256::digest(data).into()
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn upload(conn: &Connection, entity_id: i64, name: &str, content: &[u8]) -> i64 {
        AttachmentRepository::new(conn)
            .store_reader(
                entity_id,
                name,
                "text/plain",
                &mut Cursor::new(content),
                content.len() as i64,
                &hash(content),
                &limits(50, 100),
                100,
            )
            .unwrap()
    }

    #[test]
    fn reservations_count_completed_bytes_and_release_on_cancel_or_expiry() {
        let conn = fixture();
        let repository = AttachmentRepository::new(&conn);
        upload(&conn, 1, "first.txt", b"123456");
        let reserve = repository
            .begin_upload(
                "alice",
                1,
                "second.txt",
                "text/plain",
                4,
                &hash(b"1234"),
                200,
                &limits(50, 10),
            )
            .unwrap();
        assert!(matches!(
            repository.begin_upload(
                "alice",
                2,
                "third.txt",
                "text/plain",
                1,
                &hash(b"x"),
                200,
                &limits(50, 10)
            ),
            Err(AttachmentError::WorkspaceBudget)
        ));
        repository.cancel_upload("alice", reserve, 100).unwrap();
        let expired = repository
            .begin_upload(
                "alice",
                1,
                "second.txt",
                "text/plain",
                4,
                &hash(b"1234"),
                101,
                &limits(50, 10),
            )
            .unwrap();
        assert!(matches!(
            repository.append_chunk("alice", expired, 0, b"1", 101),
            Err(AttachmentError::ExpiredSession)
        ));
        assert!(matches!(
            repository.finish_upload("alice", expired, 101, &limits(50, 10)),
            Err(AttachmentError::ExpiredSession)
        ));
        repository.cancel_upload("alice", expired, 101).unwrap();
        assert_eq!(repository.expire_uploads(101).unwrap(), 0);
        assert_eq!(count(&conn, "attachment_upload"), 0);
        repository
            .begin_upload(
                "alice",
                2,
                "third.txt",
                "text/plain",
                4,
                &hash(b"1234"),
                200,
                &limits(50, 10),
            )
            .unwrap();
    }

    #[test]
    fn chunks_replay_exactly_and_finish_checks_length_hash_and_principal() {
        let conn = fixture();
        let repo = AttachmentRepository::new(&conn);
        let limits = limits(50, 100);
        let id = repo
            .begin_upload(
                "alice",
                1,
                "memo.txt",
                "text/plain",
                6,
                &hash(b"abcdef"),
                3_600_000_100,
                &limits,
            )
            .unwrap();
        let other_graph = fixture();
        assert!(matches!(
            AttachmentRepository::new(&other_graph).append_chunk("alice", id, 0, b"abc", 100),
            Err(AttachmentError::NotFound),
        ));
        assert!(matches!(
            repo.append_chunk("bob", id, 0, b"abc", 100),
            Err(AttachmentError::WrongPrincipal)
        ));
        assert!(matches!(
            repo.finish_upload("bob", id, 100, &limits),
            Err(AttachmentError::WrongPrincipal)
        ));
        assert!(matches!(
            repo.cancel_upload("bob", id, 100),
            Err(AttachmentError::WrongPrincipal)
        ));
        assert!(matches!(
            repo.append_chunk("alice", id, 1, b"def", 100),
            Err(AttachmentError::Order)
        ));
        assert_eq!(
            repo.append_chunk("alice", id, 0, b"abc", 100).unwrap(),
            (1, 3)
        );
        assert_eq!(
            repo.append_chunk("alice", id, 0, b"abc", 100).unwrap(),
            (1, 3)
        );
        assert!(matches!(
            repo.append_chunk("alice", id, 0, b"abd", 100),
            Err(AttachmentError::Order)
        ));
        assert!(matches!(
            repo.finish_upload("alice", id, 100, &limits),
            Err(AttachmentError::Incomplete)
        ));
        assert_eq!(
            repo.append_chunk("alice", id, 1, b"def", 100).unwrap(),
            (2, 6)
        );
        let attachment = repo.finish_upload("alice", id, 100, &limits).unwrap();
        assert_eq!(
            repo.finish_upload("alice", id, 200, &limits).unwrap(),
            attachment
        );
        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT content FROM attachment WHERE id=?1",
                [attachment],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bytes, b"abcdef");
        assert_eq!(count(&conn, "attachment_job"), 1);
        assert_eq!(conn.query_row(
            "SELECT a.status,j.state FROM attachment a JOIN attachment_job j ON j.attachment_id=a.id WHERE a.id=?1",
            [attachment], |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)),
        ).unwrap(),("uploaded".into(),"pending".into()));
        assert_eq!(count(&conn, "attachment_upload_chunk"), 0);
        assert_eq!(count(&conn, "attachment"), 1);
    }

    #[test]
    fn duplicates_mime_size_hash_and_failed_reader_leave_no_final_rows() {
        let conn = fixture();
        let repo = AttachmentRepository::new(&conn);
        let limits = limits(6, 18);
        assert!(matches!(
            repo.begin_upload("alice", 1, "bad", "image/png", 1, &hash(b"x"), 200, &limits),
            Err(AttachmentError::Mime)
        ));
        assert!(matches!(
            repo.begin_upload(
                "alice",
                1,
                "bad",
                "text/plain",
                7,
                &hash(b"xxxxxxx"),
                200,
                &limits
            ),
            Err(AttachmentError::Size)
        ));
        let first = repo
            .store_reader(
                1,
                "same.txt",
                "text/plain",
                &mut Cursor::new(b"abc"),
                3,
                &hash(b"abc"),
                &limits,
                100,
            )
            .unwrap();
        assert!(matches!(
            repo.store_reader(
                1,
                "same.txt",
                "text/plain",
                &mut Cursor::new(b"def"),
                3,
                &hash(b"def"),
                &limits,
                100
            ),
            Err(AttachmentError::DuplicateFilename)
        ));
        assert!(
            repo.store_reader(
                2,
                "same.txt",
                "text/plain",
                &mut Cursor::new(b"def"),
                3,
                &hash(b"def"),
                &limits,
                100
            )
            .is_ok()
        );
        assert!(matches!(
            repo.store_reader(
                1,
                "wrong.txt",
                "text/plain",
                &mut Cursor::new(b"ghi"),
                3,
                &hash(b"xxx"),
                &limits,
                100
            ),
            Err(AttachmentError::HashMismatch)
        ));
        let broken = BrokenReader {
            data: Cursor::new(b"xyz"),
            read_once: false,
        };
        assert!(matches!(
            repo.store_reader(
                1,
                "broken.txt",
                "text/plain",
                &mut { broken },
                3,
                &hash(b"xyz"),
                &limits,
                100
            ),
            Err(AttachmentError::Storage(_))
        ));
        let bad_session = repo
            .begin_upload(
                "alice",
                1,
                "hash.txt",
                "text/plain",
                3,
                &hash(b"abc"),
                200,
                &limits,
            )
            .unwrap();
        repo.append_chunk("alice", bad_session, 0, b"def", 100)
            .unwrap();
        assert!(matches!(
            repo.finish_upload("alice", bad_session, 100, &limits),
            Err(AttachmentError::HashMismatch)
        ));
        assert_eq!(count(&conn, "attachment"), 2);
        assert_eq!(count(&conn, "attachment_job"), 2);
        assert_eq!(count(&conn, "attachment_upload_chunk"), 1);
        assert_eq!(
            conn.query_row("SELECT content FROM attachment WHERE id=?1", [first], |r| r
                .get::<_, Vec<u8>>(0))
                .unwrap(),
            b"abc"
        );
    }

    struct BrokenReader {
        data: Cursor<&'static [u8]>,
        read_once: bool,
    }
    impl Read for BrokenReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.read_once {
                return Err(io::Error::other("broken spool"));
            }
            self.read_once = true;
            let len = buf.len().min(1);
            self.data.read(&mut buf[..len])
        }
    }

    #[test]
    fn extraction_fence_preserves_pages_and_unicode_exactly() {
        let conn = fixture();
        let id = upload(&conn, 1, "long.txt", b"a");
        let repo = AttachmentJobRepository::new(&conn);
        let first = repo.claim_due(100, 10).unwrap().unwrap();
        assert_eq!(
            (
                first.attachment_id,
                first.revision,
                first.lease.epoch,
                first.attempts
            ),
            (id, 1, 1, 1)
        );
        assert!(!repo.complete(&first, 111, &[(1, "stale".into())]).unwrap());
        let current = repo.claim_due(111, 10).unwrap().unwrap();
        assert!(!repo.renew(&first, 112, 10).unwrap());
        assert!(
            !repo
                .retry(&first, 112, 113, "provider", "lost", false)
                .unwrap()
        );
        assert!(!repo.complete(&first, 112, &[(1, "lost".into())]).unwrap());
        let page = "e\u{301}".repeat(1601);
        let pages = vec![
            (1, page.clone()),
            (2, "二🌐\n".to_owned()),
            (3, String::new()),
        ];
        assert!(repo.renew(&current, 112, 10).unwrap());
        assert!(repo.complete(&current, 113, &pages).unwrap());
        let saved: Vec<(i64, String, i64)> = conn
            .prepare(
                "SELECT page,text,chars FROM attachment_text WHERE attachment_id=?1 ORDER BY page",
            )
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            saved
                .iter()
                .map(|(p, t, c)| (*p, t.clone(), *c))
                .collect::<Vec<_>>(),
            vec![
                (1, page.clone(), 3202),
                (2, "二🌐\n".into(), 3),
                (3, String::new(), 0)
            ]
        );
        let parts: Vec<(i64,i64,i64,String)> = conn.prepare("SELECT chunk_index,page,segment_index,text FROM attachment_chunk WHERE attachment_id=?1 ORDER BY chunk_index").unwrap().query_map([id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(
            parts
                .iter()
                .map(|(_, p, _, text)| (*p, text.chars().count()))
                .collect::<Vec<_>>(),
            vec![(1, 1600), (1, 1600), (1, 2), (2, 3)]
        );
        assert_eq!(
            parts.iter().map(|(i, _, _, _)| *i).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            parts
                .iter()
                .filter(|(_, p, _, _)| *p == 1)
                .map(|(_, _, _, s)| s.as_str())
                .collect::<String>(),
            page
        );
        assert_eq!(count(&conn, "attachment_chunk"), 4);
        assert_eq!(
            conn.query_row(
                "SELECT revision,status,error_stage FROM attachment WHERE id=?1",
                [id],
                |r| Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?
                ))
            )
            .unwrap(),
            (2, "ready".into(), None)
        );
    }

    #[test]
    fn retry_keeps_extraction_error_visible_and_does_not_publish_partial_text() {
        let conn = fixture();
        let id = upload(&conn, 1, "retry.txt", b"a");
        let jobs = AttachmentJobRepository::new(&conn);
        let first = jobs.claim_due(100, 10).unwrap().unwrap();
        assert!(
            jobs.retry(&first, 101, 110, "provider", "timed out", false)
                .unwrap()
        );
        assert!(!jobs.complete(&first, 102, &[(1, "bad".into())]).unwrap());
        assert_eq!(
            conn.query_row(
                "SELECT status,error_stage,last_error FROM attachment WHERE id=?1",
                [id],
                |r| Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?
                ))
            )
            .unwrap(),
            ("extracting".into(), "provider".into(), "timed out".into())
        );
        let next = jobs.claim_due(110, 10).unwrap().unwrap();
        assert!(
            jobs.retry(&next, 111, 120, "render", "missing renderer", true)
                .unwrap()
        );
        assert_eq!(count(&conn, "attachment_text"), 0);
        assert_eq!(
            conn.query_row(
                "SELECT status,error_stage FROM attachment WHERE id=?1",
                [id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            )
            .unwrap(),
            ("error".into(), "render".into())
        );
        assert!(jobs.claim_due(121, 10).unwrap().is_none());
    }

    #[test]
    fn expired_eighth_lease_becomes_terminal_without_a_ninth_extraction() {
        let conn = fixture();
        let attachment = upload(&conn, 1, "bounded.txt", b"a");
        let jobs = AttachmentJobRepository::new(&conn);
        for attempt in 0..7 {
            let job = jobs.claim_due(100 + attempt * 10, 5).unwrap().unwrap();
            assert_eq!(job.attempts, attempt + 1);
            assert!(
                jobs.retry(
                    &job,
                    101 + attempt * 10,
                    110 + attempt * 10,
                    "provider",
                    "timeout",
                    false
                )
                .unwrap()
            );
        }
        let eighth = jobs.claim_due(170, 5).unwrap().unwrap();
        assert_eq!(eighth.attempts, 8);
        assert!(jobs.claim_due(176, 5).unwrap().is_none());
        assert_eq!(
            conn.query_row(
                "SELECT state FROM attachment_job WHERE attachment_id=?1",
                [attachment],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "dead"
        );
        assert_eq!(
            conn.query_row(
                "SELECT status,error_stage FROM attachment WHERE id=?1",
                [attachment],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            )
            .unwrap(),
            ("error".into(), "provider".into())
        );
    }

    #[test]
    fn ready_attachment_enqueues_both_serving_and_candidate_profiles() {
        let conn = fixture();
        let serving = Uuid::new_v4();
        let candidate = Uuid::new_v4();
        conn.execute(
            "UPDATE index_profile_registry SET state='Rebuilding',serving_profile=?1,
             candidate_profile=?2 WHERE store_key='default'",
            params![serving.to_string(), candidate.to_string()],
        )
        .unwrap();
        let attachment = upload(&conn, 1, "both.txt", b"a");
        let job = AttachmentJobRepository::new(&conn)
            .claim_due(100, 10)
            .unwrap()
            .unwrap();
        assert!(
            AttachmentJobRepository::new(&conn)
                .complete(&job, 101, &[(1, "a".into())])
                .unwrap()
        );
        let queued: Vec<(String, String, i64)> = conn
            .prepare(
                "SELECT profile_id,state,owner_revision FROM chunk_index_job
                      WHERE owner_kind='attachment' AND owner_id=?1 ORDER BY profile_id",
            )
            .unwrap()
            .query_map([attachment], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let mut expected = vec![
            (serving.to_string(), "pending".into(), 2),
            (candidate.to_string(), "pending".into(), 2),
        ];
        expected.sort();
        assert_eq!(queued, expected);
    }

    #[test]
    fn zero_segment_has_a_done_index_job_and_needs_no_vector() {
        let conn = fixture();
        let attachment = upload(&conn, 1, "blank.txt", b"");
        let job = AttachmentJobRepository::new(&conn)
            .claim_due(100, 10)
            .unwrap()
            .unwrap();
        AttachmentJobRepository::new(&conn)
            .complete(&job, 101, &[(1, String::new())])
            .unwrap();
        assert_eq!(count(&conn, "attachment_chunk"), 0);
        let state: String = conn
            .query_row(
                "SELECT state FROM chunk_index_job WHERE owner_kind='attachment' AND owner_id=?1",
                [attachment],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "held");
        let candidate = crate::jobs::IndexProfile {
            id: Uuid::new_v4(),
            store_key: "default".into(),
            provider_kind: "test".into(),
            model: "test".into(),
            dimensions: 2,
            representation_version: "entity-v1".into(),
            normalization: crate::jobs::Normalization::None,
            distance_metric: crate::jobs::DistanceMetric::Cosine,
            vector_encoding_version: "f32le-v1".into(),
        };
        IndexProfileRegistry::new(&conn)
            .begin_rebuild(&candidate)
            .unwrap();
        let index = IndexJobRepository::new(&conn);
        let mut found = false;
        while let Some(claim) = index.claim_due(200, 10).unwrap() {
            if claim.owner_kind == OwnerKind::Attachment {
                assert_eq!(claim.owner_id, attachment);
                assert!(index.commit_chunks(&claim, 201, Some(&[]), "test").unwrap());
                found = true;
            } else {
                assert!(
                    index
                        .commit_chunks(
                            &claim,
                            201,
                            Some(&[&(crate::jobs::ChunkKind::Identity, &[1.0f32, 0.0])]),
                            "test"
                        )
                        .unwrap()
                );
            }
        }
        assert!(found);
        AnnGenerationRepository::new(&conn)
            .verify_full_scan(candidate.id)
            .unwrap();
    }

    #[test]
    fn expired_reservations_do_not_block_the_quota_and_the_owner_can_cancel_them() {
        let conn = fixture();
        let repository = AttachmentRepository::new(&conn);
        upload(&conn, 1, "first.txt", b"123456");
        let stale = repository
            .begin_upload_at(
                "alice",
                1,
                "stale.txt",
                "text/plain",
                61,
                &hash(&b"x".repeat(61)),
                100,
                200,
                &limits(70, 70),
            )
            .unwrap();
        // Admission after the stale reservation's expiry must ignore it.
        repository
            .begin_upload_at(
                "alice",
                2,
                "live.txt",
                "text/plain",
                4,
                &hash(b"abcd"),
                250,
                300,
                &limits(70, 70),
            )
            .unwrap();
        assert_eq!(count(&conn, "attachment_upload"), 2);
        // The fence still rejects chunks on an expired session...
        assert!(matches!(
            repository.append_chunk("alice", stale, 0, b"x", 250),
            Err(AttachmentError::ExpiredSession)
        ));
        // ...but the owning principal can cancel the expired session.
        repository.cancel_upload("alice", stale, 250).unwrap();
        assert_eq!(count(&conn, "attachment_upload"), 1);
        assert_eq!(repository.expire_uploads(250).unwrap(), 0);
    }

    #[test]
    fn empty_chunks_are_rejected_and_zero_byte_uploads_finish_without_chunks() {
        let conn = fixture();
        let repo = AttachmentRepository::new(&conn);
        let limits = limits(50, 100);
        let blank = repo
            .begin_upload_at(
                "alice",
                1,
                "blank.txt",
                "text/plain",
                0,
                &hash(b""),
                100,
                3_600_000_100,
                &limits,
            )
            .unwrap();
        assert!(matches!(
            repo.append_chunk("alice", blank, 0, b"", 100),
            Err(AttachmentError::EmptyChunk)
        ));
        assert_eq!(count(&conn, "attachment_upload_chunk"), 0);
        let attachment = repo.finish_upload("alice", blank, 100, &limits).unwrap();
        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT content FROM attachment WHERE id=?1",
                [attachment],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bytes, b"");
        let memo = repo
            .begin_upload_at(
                "alice",
                2,
                "memo.txt",
                "text/plain",
                3,
                &hash(b"abc"),
                100,
                3_600_000_100,
                &limits,
            )
            .unwrap();
        // An empty append must not advance a non-empty upload either.
        assert!(matches!(
            repo.append_chunk("alice", memo, 0, b"", 100),
            Err(AttachmentError::EmptyChunk)
        ));
        assert_eq!(count(&conn, "attachment_upload_chunk"), 0);
        assert_eq!(
            repo.append_chunk("alice", memo, 0, b"abc", 100).unwrap(),
            (1, 3)
        );
    }

    #[test]
    fn next_due_us_returns_the_eighth_attempt_lease_deadline() {
        let conn = fixture();
        let jobs = AttachmentJobRepository::new(&conn);
        let attachment = upload(&conn, 1, "lease.txt", b"a");
        let job = jobs.claim_due(100, 10).unwrap().unwrap();
        assert_eq!(job.attachment_id, attachment);
        conn.execute(
            "UPDATE attachment_job
             SET attempts=8, state='leased', lease_until_us=500
             WHERE attachment_id=?1",
            [attachment],
        )
        .unwrap();

        assert_eq!(jobs.next_due_us(200).unwrap(), Some(500));
    }

    #[test]
    fn next_due_us() {
        let conn = fixture();
        let attachments = AttachmentRepository::new(&conn);
        let jobs = AttachmentJobRepository::new(&conn);
        let retrying = upload(&conn, 1, "retry.txt", b"a");
        let retry_job = jobs.claim_due(100, 10).unwrap().unwrap();
        assert_eq!(retry_job.attachment_id, retrying);
        assert!(
            jobs.retry(&retry_job, 101, 500, "provider", "timeout", false)
                .unwrap()
        );

        let leased = upload(&conn, 1, "lease.txt", b"b");
        let lease_job = jobs.claim_due(100, 10).unwrap().unwrap();
        assert_eq!(lease_job.attachment_id, leased);
        let unfinished = attachments
            .begin_upload_at(
                "alice",
                2,
                "unfinished.txt",
                "text/plain",
                1,
                &hash(b"c"),
                100,
                300,
                &limits(50, 100),
            )
            .unwrap();

        assert_eq!(jobs.next_due_us(200).unwrap(), Some(110));
        conn.execute(
            "UPDATE attachment_job SET state='done' WHERE attachment_id=?1",
            [leased],
        )
        .unwrap();
        assert_eq!(jobs.next_due_us(200).unwrap(), Some(300));
        conn.execute(
            "UPDATE attachment_upload SET attachment_id=?2 WHERE upload_id=?1",
            params![unfinished.to_string(), retrying],
        )
        .unwrap();
        assert_eq!(jobs.next_due_us(200).unwrap(), Some(500));
        conn.execute(
            "UPDATE attachment_job SET state='done' WHERE attachment_id=?1",
            [retrying],
        )
        .unwrap();

        let completed = attachments
            .begin_upload_at(
                "alice",
                2,
                "complete.txt",
                "text/plain",
                0,
                &hash(b""),
                100,
                400,
                &limits(50, 100),
            )
            .unwrap();
        let attachment = attachments
            .finish_upload("alice", completed, 100, &limits(50, 100))
            .unwrap();
        let complete_job = jobs.claim_due(100, 10).unwrap().unwrap();
        assert_eq!(complete_job.attachment_id, attachment);
        assert!(jobs.complete(&complete_job, 101, &[]).unwrap());
        assert_eq!(jobs.next_due_us(200).unwrap(), None);
    }
}
