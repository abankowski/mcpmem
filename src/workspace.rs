//! One registry holds graph paths and access grants. Graph rows stay in separate files.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::authz::{Principal, PrincipalKind};
use crate::config::{Durability, SqliteTuning};
use crate::kg::GraphHandle;
use crate::principals::{self, PrincipalEntry};
use crate::vector_store::{VectorConfig, VectorStore};

const REGISTRY_VERSION: i64 = 1;
const LOCAL_ID: &str = "machine:local";
const STATIC_ID: &str = "machine:static";

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace not found")]
    NotFound,
    #[error("workspace selection required")]
    SelectionRequired,
    #[error("workspace access denied")]
    AccessDenied,
    #[error("invalid workspace input: {0}")]
    InvalidInput(String),
    #[error("workspace registry error: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("workspace file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("graph setup failed: {0}")]
    Graph(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Private,
    Public,
}

impl Visibility {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAccess {
    Read,
    Write,
    Owner,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub workspace_id: String,
    pub name: String,
    pub visibility: Visibility,
    pub owner_id: String,
    pub graph_path: PathBuf,
    pub created_us: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceView {
    pub workspace_id: String,
    pub name: String,
    pub visibility: Visibility,
    pub role: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePage {
    pub workspaces: Vec<WorkspaceView>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceGrant {
    pub principal_id: String,
    pub role: String,
}

/// Public metadata for a machine account. It never carries a credential.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineView {
    pub principal_id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub revoked: bool,
}

pub struct WorkspaceRegistry {
    conn: Mutex<Connection>,
    legacy_path: PathBuf,
    graph_dir: PathBuf,
    file_humans: HashSet<String>,
    static_enabled: bool,
}

impl WorkspaceRegistry {
    /// Use this form only for a local process with no file-backed humans or static token.
    pub fn open(memory_path: &Path, legacy_owner: Option<&str>) -> Result<Self, WorkspaceError> {
        Self::open_with_principals(memory_path, legacy_owner, &[], false)
    }

    /// The caller supplies the same verified identities and static-token setting as startup.
    /// An owner is checked before any graph schema change or migration marker.
    pub fn open_with_principals(
        memory_path: &Path,
        legacy_owner: Option<&str>,
        principals: &[PrincipalEntry],
        static_enabled: bool,
    ) -> Result<Self, WorkspaceError> {
        let absolute_path = if memory_path.is_absolute() {
            memory_path.to_path_buf()
        } else {
            std::env::current_dir()?.join(memory_path)
        };
        let memory_path = absolute_path.as_path();
        let file_humans = principals
            .iter()
            .map(|principal| principals::human_id(&principal.iss, &principal.sub))
            .collect();
        let registry_path = appended_path(memory_path, ".workspaces.sqlite");
        let graph_dir = appended_path(memory_path, ".workspaces");
        let exists = registry_path.exists();
        let conn = Connection::open(&registry_path)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let registry = Self {
            conn: Mutex::new(conn),
            legacy_path: memory_path.to_path_buf(),
            graph_dir,
            file_humans,
            static_enabled,
        };
        let conn = registry
            .conn
            .lock()
            .expect("workspace registry lock poisoned");
        let has_version: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='workspace_registry_version')",
            [],
            |row| row.get(0),
        )?;
        if has_version {
            let version: Option<i64> = conn.query_row(
                "SELECT max(version) FROM workspace_registry_version",
                [],
                |row| row.get(0),
            )?;
            if version.is_some_and(|version| version > REGISTRY_VERSION) {
                return Err(WorkspaceError::Graph(
                    "registry schema is newer than this binary".into(),
                ));
            }
            if version.is_some_and(|version| version != REGISTRY_VERSION) {
                return Err(WorkspaceError::Graph(
                    "unsupported registry schema version".into(),
                ));
            }
            if version == Some(REGISTRY_VERSION) {
                let mut stmt = conn.prepare("SELECT owner_id,graph_path FROM workspace")?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        PathBuf::from(row.get::<_, String>(1)?),
                    ))
                })?;
                let saved = rows.collect::<rusqlite::Result<Vec<_>>>()?;
                drop(stmt);
                if !saved.iter().any(|(_, path)| path == memory_path) {
                    return Err(WorkspaceError::Graph(
                        "registry has no legacy workspace".into(),
                    ));
                }
                drop(conn);
                for (owner, path) in &saved {
                    if !registry.registered(owner)? {
                        return Err(WorkspaceError::InvalidInput(format!(
                            "the saved owner of '{}' is not registered",
                            path.display()
                        )));
                    }
                    if !path.is_file() {
                        return Err(WorkspaceError::Graph(format!(
                            "registered graph file is missing: {}",
                            path.display()
                        )));
                    }
                    let graph =
                        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    let marker: Option<i64> = graph.query_row(
                        "SELECT max(version) FROM schema_migration",
                        [],
                        |row| row.get(0),
                    )?;
                    if marker != Some(14) {
                        return Err(WorkspaceError::Graph(format!(
                            "registered graph has no workspace marker: {}",
                            path.display()
                        )));
                    }
                }
                let graph =
                    Connection::open_with_flags(memory_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
                mcpmem_core::schema::initialize_database(&graph)
                    .map_err(|error| WorkspaceError::Graph(error.to_string()))?;
                return Ok(registry);
            }
        } else if exists {
            let table_count: i64 = conn.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='workspace'",
                [],
                |row| row.get(0),
            )?;
            if table_count != 0 {
                return Err(WorkspaceError::Graph("incomplete registry schema".into()));
            }
        }
        drop(conn);

        let owner = legacy_owner.ok_or_else(|| {
            WorkspaceError::InvalidInput(
                "[workspaces] legacy-owner-id is required for first migration".into(),
            )
        })?;
        if !registry.registered(owner)? {
            return Err(WorkspaceError::InvalidInput(
                "legacy-owner-id is not a registered identity".into(),
            ));
        }
        // No graph initializer has run before this point. A failed registry write
        // leaves a marked file, not a graph a previous binary can open.
        let graph = Connection::open(memory_path)?;
        mcpmem_core::schema::initialize_database(&graph)
            .map_err(|error| WorkspaceError::Graph(error.to_string()))?;
        drop(graph);

        let mut conn = registry
            .conn
            .lock()
            .expect("workspace registry lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS workspace (
                workspace_id TEXT PRIMARY KEY, name TEXT NOT NULL,
                visibility TEXT NOT NULL CHECK (visibility IN ('private','public')),
                owner_id TEXT NOT NULL, graph_path TEXT NOT NULL UNIQUE,
                created_us INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE IF NOT EXISTS workspace_grant (
                workspace_id TEXT NOT NULL REFERENCES workspace(workspace_id),
                principal_id TEXT NOT NULL,
                role TEXT NOT NULL CHECK (role IN ('reader','writer')),
                PRIMARY KEY(workspace_id,principal_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS workspace_default (
                principal_id TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL REFERENCES workspace(workspace_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS machine_account (
                principal_id TEXT PRIMARY KEY, name TEXT NOT NULL, scopes TEXT NOT NULL,
                token_digest BLOB NOT NULL UNIQUE,
                revoked INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0,1))
            ) STRICT;
            CREATE TABLE IF NOT EXISTS workspace_registry_version (version INTEGER PRIMARY KEY) STRICT;",
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO workspace_registry_version VALUES(?1)",
            [REGISTRY_VERSION],
        )?;
        let prior: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace)", [], |row| {
            row.get(0)
        })?;
        if !prior {
            let id = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO workspace VALUES(?1,'Legacy','private',?2,?3,?4)",
                params![
                    id,
                    owner,
                    memory_path.to_string_lossy(),
                    mcpmem_core::events::now_us()
                ],
            )?;
            tx.execute(
                "INSERT INTO workspace_default VALUES(?1,?2)",
                params![owner, id],
            )?;
        }
        tx.commit()?;
        drop(conn);
        Ok(registry)
    }

    /// Check a stable human ID against the file and runtime principals.
    pub fn registered_human(&self, id: &str) -> Result<bool, WorkspaceError> {
        let Some((iss, sub)) = principals::human_key(id) else {
            return Ok(false);
        };
        if principals::human_id(&iss, &sub) != id {
            return Ok(false);
        }
        principals::resolve_human(
            &iss,
            &sub,
            |_, _| self.file_humans.contains(id).then_some(()),
            |iss, sub| {
                if !self.legacy_path.exists() {
                    return Ok(None);
                }
                let conn = Connection::open_with_flags(
                    &self.legacy_path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                let has_table: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='runtime_principal')",
                    [],
                    |row| row.get(0),
                )?;
                if !has_table {
                    return Ok(None);
                }
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_principal WHERE iss=?1 AND sub=?2)",
                    params![iss, sub],
                    |row| row.get(0),
                )?;
                Ok(exists.then_some(()))
            },
        )
        .map(|principal| principal.is_some())
    }

    fn registered(&self, principal: &str) -> Result<bool, WorkspaceError> {
        if principal == LOCAL_ID {
            return Ok(true);
        }
        if principal == STATIC_ID {
            return Ok(self.static_enabled);
        }
        if principal.starts_with("human:") {
            return self.registered_human(principal);
        }
        if principal.starts_with("machine:") {
            let conn = self.conn.lock().expect("workspace registry lock poisoned");
            let has_table: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='machine_account')",
                [], |row| row.get(0),
            )?;
            if !has_table {
                return Ok(false);
            }
            return Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM machine_account WHERE principal_id=?1 AND revoked=0)",
                [principal],
                |row| row.get(0),
            )?);
        }
        Ok(false)
    }

    /// Keep a machine active until a registry mutation commits.
    fn machine_active_in(conn: &Connection, principal_id: &str) -> Result<bool, WorkspaceError> {
        if !principal_id.starts_with("machine:")
            || principal_id == LOCAL_ID
            || principal_id == STATIC_ID
        {
            return Ok(true);
        }
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM machine_account WHERE principal_id=?1 AND revoked=0)",
            [principal_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Recheck identity while the registry write transaction is held.
    /// The human lookup opens the principals database without taking this mutex.
    fn registered_for_write_in(
        &self,
        conn: &Connection,
        principal_id: &str,
    ) -> Result<bool, WorkspaceError> {
        if principal_id.starts_with("human:") {
            self.registered_human(principal_id)
        } else {
            Self::machine_active_in(conn, principal_id)
        }
    }

    pub fn create(
        &self,
        principal_id: &str,
        name: &str,
        visibility: Visibility,
        init: impl FnOnce(&Path) -> Result<(), WorkspaceError>,
    ) -> Result<WorkspaceView, WorkspaceError> {
        if !self.registered(principal_id)? || name.trim().is_empty() {
            return Err(WorkspaceError::InvalidInput(
                "a registered owner and a non-empty name are required".into(),
            ));
        }
        let id = Uuid::new_v4().to_string();
        std::fs::create_dir_all(&self.graph_dir)?;
        let path = self.graph_dir.join(format!("{id}.sqlite"));
        init(&path)?;
        let graph = Connection::open(&path)?;
        mcpmem_core::schema::initialize_database(&graph)
            .map_err(|error| WorkspaceError::Graph(error.to_string()))?;
        drop(graph);
        let mut conn = self.conn.lock().expect("workspace registry lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !self.registered_for_write_in(&tx, principal_id)? {
            return Err(WorkspaceError::InvalidInput(
                "a registered owner and a non-empty name are required".into(),
            ));
        }
        tx.execute(
            "INSERT INTO workspace VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                id,
                name,
                visibility.as_str(),
                principal_id,
                path.to_string_lossy(),
                mcpmem_core::events::now_us()
            ],
        )?;
        let has_default: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workspace_default WHERE principal_id=?1)",
            [principal_id],
            |row| row.get(0),
        )?;
        if !has_default {
            tx.execute(
                "INSERT INTO workspace_default VALUES(?1,?2)",
                params![principal_id, id],
            )?;
        }
        tx.commit()?;
        Ok(WorkspaceView {
            workspace_id: id,
            name: name.to_owned(),
            visibility,
            role: "owner".into(),
            is_default: !has_default,
        })
    }

    fn record(conn: &Connection, id: &str) -> Result<Option<WorkspaceRecord>, WorkspaceError> {
        conn.query_row(
            "SELECT workspace_id,name,visibility,owner_id,graph_path,created_us FROM workspace WHERE workspace_id=?1",
            [id],
            |row| {
                let visibility: String = row.get(2)?;
                Ok(WorkspaceRecord {
                    workspace_id: row.get(0)?,
                    name: row.get(1)?,
                    visibility: if visibility == "public" { Visibility::Public } else { Visibility::Private },
                    owner_id: row.get(3)?,
                    graph_path: PathBuf::from(row.get::<_, String>(4)?),
                    created_us: row.get(5)?,
                })
            },
        ).optional().map_err(Into::into)
    }

    fn role(
        conn: &Connection,
        record: &WorkspaceRecord,
        principal: &str,
    ) -> Result<Option<&'static str>, WorkspaceError> {
        if record.owner_id == principal {
            return Ok(Some("owner"));
        }
        let grant: Option<String> = conn
            .query_row(
                "SELECT role FROM workspace_grant WHERE workspace_id=?1 AND principal_id=?2",
                params![record.workspace_id, principal],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match grant.as_deref() {
            Some("writer") => Some("writer"),
            Some("reader") => Some("reader"),
            _ if record.visibility == Visibility::Public => Some("public"),
            _ => None,
        })
    }

    pub fn resolve(
        &self,
        principal_id: &str,
        requested: Option<&str>,
        access: WorkspaceAccess,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        if !self.registered(principal_id)? {
            return Err(WorkspaceError::NotFound);
        }
        let conn = self.conn.lock().expect("workspace registry lock poisoned");
        let id = if let Some(requested) = requested {
            Uuid::parse_str(requested)
                .map_err(|_| WorkspaceError::InvalidInput("workspaceId must be a UUID".into()))?
                .to_string()
        } else {
            conn.query_row(
                "SELECT workspace_id FROM workspace_default WHERE principal_id=?1",
                [principal_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(WorkspaceError::SelectionRequired)?
        };
        let record = Self::record(&conn, &id)?.ok_or_else(|| {
            if requested.is_none() {
                WorkspaceError::SelectionRequired
            } else {
                WorkspaceError::NotFound
            }
        })?;
        let role = Self::role(&conn, &record, principal_id)?.ok_or_else(|| {
            if requested.is_none() {
                WorkspaceError::SelectionRequired
            } else {
                WorkspaceError::NotFound
            }
        })?;
        if (access == WorkspaceAccess::Write && !matches!(role, "owner" | "writer"))
            || (access == WorkspaceAccess::Owner && role != "owner")
        {
            return Err(WorkspaceError::AccessDenied);
        }
        Ok(record)
    }

    pub fn list(
        &self,
        principal_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<WorkspacePage, WorkspaceError> {
        if !self.registered(principal_id)? {
            return Err(WorkspaceError::AccessDenied);
        }
        let after = match cursor {
            None => String::new(),
            Some(cursor) => {
                let raw = cursor
                    .strip_prefix("v1.")
                    .ok_or_else(|| WorkspaceError::InvalidInput("invalid list cursor".into()))?;
                let id = Uuid::parse_str(raw)
                    .map_err(|_| WorkspaceError::InvalidInput("invalid list cursor".into()))?;
                if id.simple().to_string() != raw {
                    return Err(WorkspaceError::InvalidInput("invalid list cursor".into()));
                }
                id.to_string()
            }
        };
        let conn = self.conn.lock().expect("workspace registry lock poisoned");
        let default: Option<String> = conn
            .query_row(
                "SELECT workspace_id FROM workspace_default WHERE principal_id=?1",
                [principal_id],
                |row| row.get(0),
            )
            .optional()?;
        let mut stmt = conn.prepare(
            "SELECT workspace_id,name,visibility,owner_id,graph_path,created_us FROM workspace
             WHERE workspace_id>?1 AND (
                owner_id=?2 OR visibility='public' OR EXISTS (
                    SELECT 1 FROM workspace_grant WHERE workspace_grant.workspace_id=workspace.workspace_id AND principal_id=?2
                )
             ) ORDER BY workspace_id LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![after, principal_id, (limit.clamp(1, 100) + 1) as i64],
            |row| {
                let visibility: String = row.get(2)?;
                Ok(WorkspaceRecord {
                    workspace_id: row.get(0)?,
                    name: row.get(1)?,
                    visibility: if visibility == "public" {
                        Visibility::Public
                    } else {
                        Visibility::Private
                    },
                    owner_id: row.get(3)?,
                    graph_path: PathBuf::from(row.get::<_, String>(4)?),
                    created_us: row.get(5)?,
                })
            },
        )?;
        let mut workspaces = Vec::new();
        for row in rows {
            let record = row?;
            let role =
                Self::role(&conn, &record, principal_id)?.expect("list query enforces access");
            workspaces.push(WorkspaceView {
                is_default: default.as_deref() == Some(record.workspace_id.as_str()),
                workspace_id: record.workspace_id,
                name: record.name,
                visibility: record.visibility,
                role: role.into(),
            });
        }
        let next_cursor = if workspaces.len() > limit.clamp(1, 100) {
            workspaces.pop();
            workspaces.last().map(|view| {
                let id = Uuid::parse_str(&view.workspace_id).expect("stored IDs are UUIDs");
                format!("v1.{}", id.simple())
            })
        } else {
            None
        };
        Ok(WorkspacePage {
            workspaces,
            next_cursor,
        })
    }

    pub fn all_paths(&self) -> Result<Vec<(String, PathBuf)>, WorkspaceError> {
        let conn = self.conn.lock().expect("workspace registry lock poisoned");
        let mut stmt =
            conn.prepare("SELECT workspace_id,graph_path FROM workspace ORDER BY workspace_id")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get(0)?, PathBuf::from(row.get::<_, String>(1)?)))
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Select the registered graph at `cursor`, wrapping around without
    /// materializing the path list.
    ///
    /// Rows serve in insertion (rowid) order, so a graph registered mid-cycle
    /// joins the schedule; `cursor` advances to the next offset. Workspace
    /// rows are never deleted, so `MAX(rowid)` is the cycle length. A broken
    /// registry query surfaces as an error instead of a silent empty schedule.
    #[cfg(any(feature = "indexer", feature = "webhooks"))]
    pub(crate) fn next_path(
        &self,
        cursor: &mut usize,
    ) -> Result<Option<(String, PathBuf)>, WorkspaceError> {
        let conn = self.conn.lock().expect("workspace registry lock poisoned");
        let last: i64 =
            conn.query_row("SELECT coalesce(MAX(rowid),0) FROM workspace", [], |row| {
                row.get(0)
            })?;
        let count = last as usize;
        if count == 0 {
            return Ok(None);
        }
        let offset = if *cursor >= count { 0 } else { *cursor };
        let (id, path): (String, String) = conn.query_row(
            "SELECT workspace_id, graph_path FROM workspace ORDER BY rowid LIMIT 1 OFFSET ?1",
            [offset as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        *cursor = offset + 1;
        Ok(Some((id, PathBuf::from(path))))
    }

    pub fn set_default(
        &self,
        principal_id: &str,
        workspace_id: &str,
    ) -> Result<(), WorkspaceError> {
        if !self.registered(principal_id)? {
            return Err(WorkspaceError::NotFound);
        }
        let id = Uuid::parse_str(workspace_id)
            .map_err(|_| WorkspaceError::InvalidInput("workspaceId must be a UUID".into()))?
            .to_string();
        let mut conn = self.conn.lock().expect("workspace registry lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !self.registered_for_write_in(&tx, principal_id)? {
            return Err(WorkspaceError::NotFound);
        }
        let record = Self::record(&tx, &id)?.ok_or(WorkspaceError::NotFound)?;
        if Self::role(&tx, &record, principal_id)?.is_none() {
            return Err(WorkspaceError::NotFound);
        }
        tx.execute(
            "INSERT INTO workspace_default VALUES(?1,?2) ON CONFLICT(principal_id) DO UPDATE SET workspace_id=excluded.workspace_id",
            params![principal_id, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The caller's view of one workspace it can access, plus the record.
    /// The record carries the owner identity for owner-only result fields;
    /// the view carries the caller's current role and default flag.
    pub fn view(
        &self,
        principal_id: &str,
        workspace_id: &str,
    ) -> Result<(WorkspaceRecord, WorkspaceView), WorkspaceError> {
        let record = self.resolve(principal_id, Some(workspace_id), WorkspaceAccess::Read)?;
        let conn = self.conn.lock().expect("workspace registry lock poisoned");
        let role = Self::role(&conn, &record, principal_id)?.ok_or(WorkspaceError::NotFound)?;
        let is_default: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM workspace_default WHERE principal_id=?1 AND workspace_id=?2)",
            params![principal_id, record.workspace_id],
            |row| row.get(0),
        )?;
        let view = WorkspaceView {
            workspace_id: record.workspace_id.clone(),
            name: record.name.clone(),
            visibility: record.visibility,
            role: role.into(),
            is_default,
        };
        Ok((record, view))
    }

    pub fn grant(
        &self,
        owner: &str,
        workspace_id: &str,
        principal_id: &str,
        role: &str,
    ) -> Result<(), WorkspaceError> {
        let record = self.resolve(owner, Some(workspace_id), WorkspaceAccess::Owner)?;
        if !matches!(role, "reader" | "writer") || !self.registered(principal_id)? {
            return Err(WorkspaceError::InvalidInput(
                "grant needs a registered identity and reader or writer role".into(),
            ));
        }
        if owner == principal_id {
            return Err(WorkspaceError::InvalidInput(
                "owner access cannot be replaced by a grant".into(),
            ));
        }
        let mut conn = self.conn.lock().expect("workspace registry lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !self.registered_for_write_in(&tx, principal_id)? {
            return Err(WorkspaceError::InvalidInput(
                "grant needs a registered identity and reader or writer role".into(),
            ));
        }
        tx.execute(
            "INSERT INTO workspace_grant VALUES(?1,?2,?3) ON CONFLICT(workspace_id,principal_id) DO UPDATE SET role=excluded.role",
            params![record.workspace_id, principal_id, role],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn revoke(
        &self,
        owner: &str,
        workspace_id: &str,
        principal_id: &str,
    ) -> Result<bool, WorkspaceError> {
        let record = self.resolve(owner, Some(workspace_id), WorkspaceAccess::Owner)?;
        if owner == principal_id {
            return Err(WorkspaceError::InvalidInput(
                "owner cannot revoke ownership".into(),
            ));
        }
        let mut conn = self.conn.lock().expect("workspace registry lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revoked = tx.execute(
            "DELETE FROM workspace_grant WHERE workspace_id=?1 AND principal_id=?2",
            params![record.workspace_id, principal_id],
        )? != 0;
        if revoked {
            tx.execute(
                "DELETE FROM workspace_default WHERE principal_id=?1 AND workspace_id=?2",
                params![principal_id, record.workspace_id],
            )?;
        }
        tx.commit()?;
        Ok(revoked)
    }

    pub fn set_visibility(
        &self,
        owner: &str,
        workspace_id: &str,
        visibility: Visibility,
    ) -> Result<WorkspaceView, WorkspaceError> {
        let record = self.resolve(owner, Some(workspace_id), WorkspaceAccess::Owner)?;
        let mut conn = self.conn.lock().expect("workspace registry lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE workspace SET visibility=?1 WHERE workspace_id=?2",
            params![visibility.as_str(), record.workspace_id],
        )?;
        if visibility == Visibility::Private {
            tx.execute(
                "DELETE FROM workspace_default WHERE workspace_id=?1 AND principal_id<>?2 AND NOT EXISTS (
                    SELECT 1 FROM workspace_grant WHERE workspace_grant.workspace_id=workspace_default.workspace_id
                    AND workspace_grant.principal_id=workspace_default.principal_id
                )",
                params![record.workspace_id, owner],
            )?;
        }
        tx.commit()?;
        let is_default = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM workspace_default WHERE principal_id=?1 AND workspace_id=?2)",
            params![owner, record.workspace_id], |row| row.get(0),
        )?;
        Ok(WorkspaceView {
            workspace_id: record.workspace_id,
            name: record.name,
            visibility,
            role: "owner".into(),
            is_default,
        })
    }

    pub fn grants(
        &self,
        owner: &str,
        workspace_id: &str,
    ) -> Result<Vec<WorkspaceGrant>, WorkspaceError> {
        let record = self.resolve(owner, Some(workspace_id), WorkspaceAccess::Owner)?;
        let conn = self.conn.lock().expect("workspace registry lock poisoned");
        let mut stmt = conn.prepare("SELECT principal_id,role FROM workspace_grant WHERE workspace_id=?1 ORDER BY principal_id")?;
        let rows = stmt.query_map([record.workspace_id], |row| {
            Ok(WorkspaceGrant {
                principal_id: row.get(0)?,
                role: row.get(1)?,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Check ownership and clear a human's access while `action` runs.
    /// The callback must not acquire this registry mutex. If it fails, the
    /// access changes roll back; otherwise the transaction commits after it.
    pub(crate) fn with_human_access_cleanup<T, E>(
        &self,
        principal_id: &str,
        action: impl FnOnce() -> Result<T, E>,
    ) -> Result<Result<T, E>, WorkspaceError> {
        let mut conn = self.conn.lock().expect("workspace registry lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let is_owner: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workspace WHERE owner_id=?1)",
            [principal_id],
            |row| row.get(0),
        )?;
        if is_owner {
            return Err(WorkspaceError::AccessDenied);
        }
        tx.execute(
            "DELETE FROM workspace_default WHERE principal_id=?1",
            [principal_id],
        )?;
        tx.execute(
            "DELETE FROM workspace_grant WHERE principal_id=?1",
            [principal_id],
        )?;
        let value = match action() {
            Ok(value) => value,
            Err(error) => return Ok(Err(error)),
        };
        tx.commit()?;
        Ok(Ok(value))
    }

    /// Issue a random credential once. The registry retains only its digest.
    pub fn create_machine(
        &self,
        name: &str,
        scopes: &[String],
    ) -> Result<(String, String), WorkspaceError> {
        let name = name.trim();
        let scopes = principals::canonical_scopes(scopes)
            .map_err(|error| WorkspaceError::InvalidInput(error.to_string()))?;
        if name.is_empty() || scopes.is_empty() || scopes.iter().any(|scope| scope == "admin") {
            return Err(WorkspaceError::InvalidInput(
                "a machine needs a name and tool-category scopes, not admin".into(),
            ));
        }
        let id = format!("machine:{}", Uuid::new_v4());
        let token = mcpmem_oauth::new_token();
        let digest = mcpmem_oauth::digest(&token);
        let scopes_json = serde_json::to_string(&scopes)
            .map_err(|error| WorkspaceError::Graph(error.to_string()))?;
        let conn = self.conn.lock().expect("workspace registry lock poisoned");
        conn.execute(
            "INSERT INTO machine_account(principal_id,name,scopes,token_digest) VALUES(?1,?2,?3,?4)",
            params![id, name, scopes_json, digest.as_bytes()],
        )?;
        Ok((id, token))
    }

    /// List metadata without reading or returning a machine credential.
    pub fn list_machines(&self) -> Result<Vec<MachineView>, WorkspaceError> {
        let conn = self.conn.lock().expect("workspace registry lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT principal_id,name,scopes,revoked FROM machine_account ORDER BY principal_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?;
        let mut accounts = Vec::new();
        for row in rows {
            let (principal_id, name, scopes, revoked) = row?;
            accounts.push(MachineView {
                principal_id,
                name,
                scopes: serde_json::from_str(&scopes)
                    .map_err(|error| WorkspaceError::Graph(error.to_string()))?,
                revoked,
            });
        }
        Ok(accounts)
    }

    /// Refuse to disable an owner. Remove its grants and default together.
    pub fn revoke_machine(&self, principal_id: &str) -> Result<bool, WorkspaceError> {
        if principal_id == LOCAL_ID
            || principal_id == STATIC_ID
            || !principal_id.starts_with("machine:")
        {
            return Err(WorkspaceError::InvalidInput(
                "only created machine accounts can be revoked".into(),
            ));
        }
        let mut conn = self.conn.lock().expect("workspace registry lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let is_owner: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workspace WHERE owner_id=?1)",
            [principal_id],
            |row| row.get(0),
        )?;
        if is_owner {
            return Err(WorkspaceError::AccessDenied);
        }
        let revoked = tx.execute(
            "UPDATE machine_account SET revoked=1 WHERE principal_id=?1 AND revoked=0",
            [principal_id],
        )? != 0;
        if revoked {
            tx.execute(
                "DELETE FROM workspace_default WHERE principal_id=?1",
                [principal_id],
            )?;
            tx.execute(
                "DELETE FROM workspace_grant WHERE principal_id=?1",
                [principal_id],
            )?;
        }
        tx.commit()?;
        Ok(revoked)
    }

    /// Resolve a machine bearer for this request, after a fresh revocation check.
    pub fn authenticate_machine(&self, token: &str) -> Result<Option<Principal>, WorkspaceError> {
        if token.is_empty() {
            return Ok(None);
        }
        let digest = mcpmem_oauth::digest(token);
        let conn = self.conn.lock().expect("workspace registry lock poisoned");
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT principal_id,scopes FROM machine_account WHERE token_digest=?1 AND revoked=0",
                [digest.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((id, scopes)) = row else {
            return Ok(None);
        };
        let scopes: BTreeSet<String> = serde_json::from_str(&scopes)
            .map_err(|error| WorkspaceError::Graph(error.to_string()))?;
        if scopes.is_empty()
            || scopes
                .iter()
                .any(|scope| scope.parse::<crate::tools::ToolCategory>().is_err())
        {
            return Err(WorkspaceError::Graph(
                "machine account has invalid tool-category scopes".into(),
            ));
        }
        Ok(Some(Principal {
            id,
            kind: PrincipalKind::Machine,
            scopes,
            allowed_origins: BTreeSet::new(),
        }))
    }
}

/// Open handles for the graph and vector store of one workspace.
pub struct WorkspaceEntry {
    pub kg: Arc<GraphHandle>,
    pub vs: Option<Arc<VectorStore>>,
}

/// The open parameters every workspace handle is built with, shared so the
/// constructor and the first-open path cannot drift apart.
#[derive(Clone, Copy)]
pub struct HandleSpec {
    pub durability: Durability,
    pub tuning: SqliteTuning,
    pub lru_cache_size: NonZeroUsize,
    pub read_pool_size: usize,
    pub vector_dims: Option<u32>,
}

/// LRU bookkeeping for the entry cache. `recency` holds workspace IDs, most
/// recently used at the back. The pinned legacy workspace (whose entry holds
/// the server's original handles) is never evicted.
struct HandleCache {
    entries: HashMap<String, WorkspaceEntry>,
    recency: VecDeque<String>,
    pinned: HashSet<String>,
    legacy_id: String,
}

/// Bounded cache of graph handles, one entry per workspace ID.
///
/// Each entry holds the [`GraphHandle`] and, when vector support is on, the
/// [`VectorStore`] for that workspace's registered file. The cache is bounded
/// so a process with many workspaces does not hold one reader pool per
/// workspace; an evicted entry has no in-process state left behind, so the
/// next call to that workspace re-opens from its file. The legacy workspace
/// entry is pinned: it carries the original handles the server was built with.
pub struct WorkspaceHandles {
    inner: Mutex<HandleCache>,
    spec: HandleSpec,
}

impl WorkspaceHandles {
    /// The cache bound, in open workspace entries. Chosen so a deployment
    /// with hundreds of workspaces stays within the process's file-descriptor
    /// and connection budget; each entry holds a graph handle plus its reader
    /// pool, and optionally one vector store.
    pub const BOUND: usize = 32;

    /// A cache seeded with the server's own legacy handles.
    pub fn new(
        legacy_id: &str,
        legacy_kg: Arc<GraphHandle>,
        legacy_vs: Option<Arc<VectorStore>>,
        spec: HandleSpec,
    ) -> Self {
        let mut entries = HashMap::new();
        entries.insert(
            legacy_id.to_owned(),
            WorkspaceEntry {
                kg: legacy_kg,
                vs: legacy_vs,
            },
        );
        Self {
            inner: Mutex::new(HandleCache {
                entries,
                recency: VecDeque::from([legacy_id.to_owned()]),
                pinned: HashSet::from([legacy_id.to_owned()]),
                legacy_id: legacy_id.to_owned(),
            }),
            spec,
        }
    }

    /// The open handles for a resolved workspace, opening them on first use
    /// and evicting the least-recently-used non-pinned entry when at capacity.
    ///
    /// A request path hydrates an already published vector snapshot from the
    /// file so the first search after a reopen serves durable vectors. That
    /// load reads only; it never publishes or changes profile state.
    pub fn get(&self, record: &WorkspaceRecord) -> Result<WorkspaceEntry, WorkspaceError> {
        self.open_entry(&record.workspace_id, &record.graph_path, true)
    }

    /// Open a graph that the registry returned to a worker. Workers do not
    /// have a request principal, so they cannot resolve a record by ACL, and
    /// they must not block on snapshot hydration: an in-flight rebuild has no
    /// published snapshot, and the worker itself publishes it after its turn.
    #[cfg(feature = "indexer")]
    pub(crate) fn get_by_registered_path(
        &self,
        workspace_id: &str,
        graph_path: &Path,
    ) -> Result<WorkspaceEntry, WorkspaceError> {
        self.open_entry(workspace_id, graph_path, false)
    }

    fn open_entry(
        &self,
        workspace_id: &str,
        graph_path: &Path,
        hydrate_snapshot: bool,
    ) -> Result<WorkspaceEntry, WorkspaceError> {
        let mut cache = self.inner.lock().expect("workspace handle cache poisoned");
        if let Some(entry) = cache.entries.get(workspace_id) {
            let kg = Arc::clone(&entry.kg);
            let vs = entry.vs.clone();
            touch(&mut cache, workspace_id);
            return Ok(WorkspaceEntry { kg, vs });
        }
        let kg = Arc::new(
            GraphHandle::new(
                graph_path,
                self.spec.durability,
                self.spec.tuning,
                self.spec.lru_cache_size,
                self.spec.read_pool_size,
            )
            .map_err(|error| WorkspaceError::Graph(error.to_string()))?,
        );
        let vs = match self.spec.vector_dims {
            Some(dims) => {
                let store = Arc::new(
                    VectorStore::with_config(graph_path, &VectorConfig::new(dims))
                        .map_err(|error| WorkspaceError::Graph(error.to_string()))?,
                );
                #[cfg(feature = "indexer")]
                if hydrate_snapshot {
                    self.load_managed_snapshot(&store)?;
                }
                let _ = hydrate_snapshot;
                Some(store)
            }
            None => None,
        };
        // Evict least-recently-used non-pinned entries until under the bound.
        // The pinned legacy entry must not block eviction: skip past it and
        // keep looking, so the cache can never exceed `BOUND` entries.
        while cache.entries.len() >= Self::BOUND {
            match cache.recency.pop_front() {
                Some(id) if cache.pinned.contains(&id) => {
                    cache.recency.push_back(id);
                }
                Some(id) => {
                    cache.entries.remove(&id);
                }
                None => break,
            }
        }
        let entry = WorkspaceEntry {
            kg: Arc::clone(&kg),
            vs: vs.clone(),
        };
        cache.entries.insert(workspace_id.to_owned(), entry);
        cache.recency.push_back(workspace_id.to_owned());
        Ok(WorkspaceEntry { kg, vs })
    }

    /// A bounded snapshot of open vector handles for the MCP refresh loop.
    pub(crate) fn cached_vectors(&self) -> Vec<Arc<VectorStore>> {
        let cache = self.inner.lock().expect("workspace handle cache poisoned");
        cache
            .entries
            .values()
            .filter_map(|entry| entry.vs.clone())
            .collect()
    }

    /// The vector store of the pinned legacy entry, when vector support is
    /// on. `tools/list` and `initialize` use it to decide whether the vector
    /// tools can run, the same way the previous single-store dispatch did.
    pub fn legacy_vs(&self) -> Option<Arc<VectorStore>> {
        let cache = self.inner.lock().expect("workspace handle cache poisoned");
        cache.entries.get(&cache.legacy_id)?.vs.clone()
    }

    #[cfg(feature = "indexer")]
    fn load_managed_snapshot(&self, store: &VectorStore) -> Result<(), WorkspaceError> {
        store
            .load_managed_snapshot()
            .map_err(|error| WorkspaceError::Graph(error.to_string()))
    }

    /// Create and initialize a fresh graph file for `registry.create`, with
    /// the same storage settings as every other workspace.
    pub fn initialize_graph(&self, path: &Path) -> Result<(), WorkspaceError> {
        GraphHandle::new(
            path,
            self.spec.durability,
            self.spec.tuning,
            self.spec.lru_cache_size,
            self.spec.read_pool_size,
        )
        .map(|_| ())
        .map_err(|error| WorkspaceError::Graph(error.to_string()))
    }
}

/// Move `id` to the back of the recency queue.
fn touch(cache: &mut HandleCache, id: &str) {
    if let Some(position) = cache.recency.iter().position(|cached| cached == id) {
        let id = cache.recency.remove(position).expect("position is valid");
        cache.recency.push_back(id);
    }
}

fn appended_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handle cache is bounded: after more workspaces than the bound are
    /// opened, the pinned legacy entry survives and the entry count stays at
    /// most [`WorkspaceHandles::BOUND`].
    #[test]
    fn handle_cache_stays_within_bound_and_keeps_the_legacy_entry() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().join("memory.sqlite");
        let registry =
            WorkspaceRegistry::open(&memory, Some("machine:local")).expect("registry opens");
        let (legacy_id, _) = registry
            .all_paths()
            .expect("registered paths")
            .into_iter()
            .find(|(_, graph_path)| graph_path == &memory)
            .expect("the legacy workspace");
        let legacy_kg = Arc::new(
            GraphHandle::new(
                &memory,
                Durability::Sync,
                SqliteTuning::default(),
                NonZeroUsize::new(32).unwrap(),
                2,
            )
            .expect("legacy graph opens"),
        );
        let handles = WorkspaceHandles::new(
            &legacy_id,
            Arc::clone(&legacy_kg),
            None,
            HandleSpec {
                durability: Durability::Sync,
                tuning: SqliteTuning::default(),
                lru_cache_size: NonZeroUsize::new(32).unwrap(),
                read_pool_size: 2,
                vector_dims: None,
            },
        );

        for index in 0..40 {
            let view = registry
                .create(
                    "machine:local",
                    &format!("ws-{index}"),
                    Visibility::Private,
                    |_| Ok(()),
                )
                .expect("workspace creation succeeds");
            let record = registry
                .resolve(
                    "machine:local",
                    Some(&view.workspace_id),
                    WorkspaceAccess::Read,
                )
                .expect("the new workspace resolves");
            handles.get(&record).expect("the entry opens");
        }

        let cache = handles.inner.lock().expect("cache lock");
        assert!(
            cache.entries.len() <= WorkspaceHandles::BOUND,
            "the cache exceeded its bound: {} entries",
            cache.entries.len()
        );
        assert!(
            cache.entries.contains_key(&legacy_id),
            "the pinned legacy entry must survive every eviction"
        );
        let recency_len = cache.recency.len();
        drop(cache);
        assert!(
            recency_len <= WorkspaceHandles::BOUND,
            "the recency queue must stay bounded too: {recency_len}"
        );
        assert!(
            Arc::ptr_eq(
                &legacy_kg,
                &handles
                    .inner
                    .lock()
                    .expect("cache lock")
                    .entries
                    .get(&legacy_id)
                    .expect("legacy entry")
                    .kg
                    .clone()
            ),
            "the cached legacy handle must be the server's own handle, not a re-open"
        );
    }
}
