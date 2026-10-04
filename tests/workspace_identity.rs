use mcpmem::authz::PrincipalKind;
use mcpmem::config::Config;
use mcpmem::server::MCPServer;
use mcpmem::workspace::{Visibility, WorkspaceAccess, WorkspaceRegistry};

fn registry() -> (tempfile::TempDir, WorkspaceRegistry) {
    let dir = tempfile::tempdir().unwrap();
    let registry =
        WorkspaceRegistry::open(&dir.path().join("memory.sqlite"), Some("machine:local")).unwrap();
    (dir, registry)
}

/// A relative `-f` memory path must start a server with the legacy graph
/// registered. The registry stores absolute paths; the server's legacy-id
/// lookup compares the memory path against them, so a relative path must be
/// resolved to the same absolute form before the comparison.
/// RED: the comparison is literal, so startup fails with "the registry has
/// no legacy workspace" for every relative path.
#[test]
fn a_relative_memory_path_starts_with_a_registered_legacy_graph() {
    // The test process runs with the crate root as cwd. A uniquely named
    // subdirectory keeps the relative database out of every other test's way.
    let sub = format!(
        ".ws-relative-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .subsec_nanos()
    );
    std::fs::create_dir(&sub).expect("create the relative test directory");
    let relative_db = format!("{sub}/memory.sqlite");

    let config = Config {
        memory_file_path: relative_db.clone(),
        legacy_owner_id: Some("machine:local".into()),
        ..Config::default()
    };
    let server = MCPServer::new_kg(config);

    // The failure mode is a failed startup, so the construction itself is
    // the first assertion; clean the directory before unwrapping so a red
    // run leaves nothing behind.
    std::fs::remove_dir_all(&sub).expect("remove the relative test directory");
    let server = server.expect("a relative -f path must build a server with the legacy graph");

    let registry = server.workspace_registry();
    let legacy = registry
        .resolve("machine:local", None, WorkspaceAccess::Read)
        .expect("the legacy default resolves from a relative -f path");
    let absolute_db = std::env::current_dir()
        .expect("the test cwd")
        .join(&relative_db);
    assert_eq!(
        legacy.graph_path, absolute_db,
        "the legacy record must carry the absolute form of the relative path"
    );

    // And the pinned legacy entry opens from the handle cache.
    server
        .workspace_handles()
        .get(&legacy)
        .expect("the legacy entry opens from the cache");
}

#[test]
fn machine_credentials_have_distinct_ids_scopes_and_saved_defaults() {
    let (_dir, registry) = registry();
    let first_workspace = registry
        .create("machine:local", "first", Visibility::Private, |_| Ok(()))
        .unwrap()
        .workspace_id;
    let second_workspace = registry
        .create("machine:local", "second", Visibility::Private, |_| Ok(()))
        .unwrap()
        .workspace_id;
    let (reader_id, reader_token) = registry
        .create_machine("index reader", &["graph-read".to_owned()])
        .unwrap();
    let (writer_id, writer_token) = registry
        .create_machine(
            "graph writer",
            &["graph-read".to_owned(), "graph-write".to_owned()],
        )
        .unwrap();

    assert_ne!(reader_id, writer_id);
    assert_ne!(reader_token, writer_token);
    assert!(reader_id.starts_with("machine:"));
    assert!(writer_id.starts_with("machine:"));
    let reader = registry
        .authenticate_machine(&reader_token)
        .unwrap()
        .unwrap();
    let writer = registry
        .authenticate_machine(&writer_token)
        .unwrap()
        .unwrap();
    assert_eq!(reader.id, reader_id);
    assert_eq!(writer.id, writer_id);
    assert_eq!(reader.kind, PrincipalKind::Machine);
    assert_eq!(writer.kind, PrincipalKind::Machine);
    assert_eq!(
        reader.scopes.iter().map(String::as_str).collect::<Vec<_>>(),
        ["graph-read"]
    );
    assert_eq!(
        writer.scopes.iter().map(String::as_str).collect::<Vec<_>>(),
        ["graph-read", "graph-write"]
    );
    assert!(
        registry
            .authenticate_machine("not-a-issued-credential")
            .unwrap()
            .is_none()
    );

    registry
        .grant("machine:local", &first_workspace, &reader_id, "reader")
        .unwrap();
    registry
        .grant("machine:local", &second_workspace, &writer_id, "writer")
        .unwrap();
    registry.set_default(&reader.id, &first_workspace).unwrap();
    registry.set_default(&writer.id, &second_workspace).unwrap();
    assert_eq!(
        registry
            .resolve(&reader.id, None, WorkspaceAccess::Read)
            .unwrap()
            .workspace_id,
        first_workspace
    );
    assert_eq!(
        registry
            .resolve(&writer.id, None, WorkspaceAccess::Write)
            .unwrap()
            .workspace_id,
        second_workspace
    );
    assert!(
        registry
            .resolve(&reader.id, None, WorkspaceAccess::Write)
            .is_err()
    );
    assert!(
        registry
            .resolve(&writer.id, Some(&first_workspace), WorkspaceAccess::Read)
            .is_err()
    );
    let accounts = registry.list_machines().unwrap();
    assert!(accounts.iter().any(|account| {
        account.principal_id == reader_id
            && account.name == "index reader"
            && account.scopes == ["graph-read"]
            && !account.revoked
    }));
    assert!(accounts.iter().any(|account| {
        account.principal_id == writer_id
            && account.name == "graph writer"
            && account.scopes == ["graph-read", "graph-write"]
            && !account.revoked
    }));
}

#[test]
fn revoked_machine_credential_is_rejected_on_the_next_lookup_and_drops_its_default() {
    let (dir, registry) = registry();
    let workspace = registry
        .create("machine:local", "private", Visibility::Private, |_| Ok(()))
        .unwrap()
        .workspace_id;
    let (id, token) = registry
        .create_machine("short-lived", &["graph-read".to_owned()])
        .unwrap();
    registry
        .grant("machine:local", &workspace, &id, "reader")
        .unwrap();
    registry.set_default(&id, &workspace).unwrap();
    assert_eq!(
        registry.authenticate_machine(&token).unwrap().unwrap().id,
        id
    );
    assert!(registry.revoke_machine(&id).unwrap());
    assert!(registry.authenticate_machine(&token).unwrap().is_none());
    assert!(
        registry
            .resolve(&id, Some(&workspace), WorkspaceAccess::Read)
            .is_err()
    );
    assert!(registry.set_default(&id, &workspace).is_err());
    assert!(
        registry
            .grant("machine:local", &workspace, &id, "writer")
            .is_err()
    );
    assert!(
        registry
            .list_machines()
            .unwrap()
            .iter()
            .any(|account| { account.principal_id == id && account.revoked })
    );

    let sidecar = format!(
        "{}.workspaces.sqlite",
        dir.path().join("memory.sqlite").display()
    );
    let (default_count, grant_count): (i64, i64) = rusqlite::Connection::open(sidecar)
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM workspace_default WHERE principal_id=?1),
                    (SELECT count(*) FROM workspace_grant WHERE principal_id=?1)",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (default_count, grant_count),
        (0, 0),
        "a revoked credential retains no saved selection or grant"
    );
}

#[test]
fn a_machine_owner_cannot_be_revoked_while_it_owns_a_workspace() {
    let (_dir, registry) = registry();
    let (id, token) = registry
        .create_machine("owner", &["graph-read".into(), "graph-write".into()])
        .unwrap();
    let workspace = registry
        .create(&id, "owned graph", Visibility::Private, |_| Ok(()))
        .unwrap();
    assert_eq!(
        registry
            .resolve(&id, None, WorkspaceAccess::Owner)
            .unwrap()
            .workspace_id,
        workspace.workspace_id
    );
    assert!(registry.revoke_machine(&id).is_err());
    assert_eq!(
        registry.authenticate_machine(&token).unwrap().unwrap().id,
        id
    );
    assert!(
        registry
            .resolve(&id, Some(&workspace.workspace_id), WorkspaceAccess::Owner)
            .is_ok()
    );
}

#[test]
fn revocation_during_graph_initialization_cannot_create_a_machine_owner() {
    use std::sync::Barrier;

    let (dir, registry) = registry();
    let path = dir.path().join("memory.sqlite");
    let (id, token) = registry
        .create_machine("race owner", &["graph-read".into(), "graph-write".into()])
        .unwrap();
    let entered_init = Barrier::new(2);
    let resume_init = Barrier::new(2);
    std::thread::scope(|scope| {
        let creation = scope.spawn(|| {
            registry.create(&id, "must not exist", Visibility::Private, |_| {
                entered_init.wait();
                resume_init.wait();
                Ok(())
            })
        });
        entered_init.wait();
        let revoked = registry.revoke_machine(&id);
        resume_init.wait();
        let created = creation.join().unwrap();
        assert!(revoked.unwrap(), "the machine was active before revocation");
        assert!(created.is_err(), "a revoked machine became a graph owner");
    });
    let conn = rusqlite::Connection::open(format!("{}.workspaces.sqlite", path.display())).unwrap();
    let (workspaces, defaults): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT count(*) FROM workspace WHERE owner_id=?1),
                    (SELECT count(*) FROM workspace_default WHERE principal_id=?1)",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((workspaces, defaults), (0, 0));
    drop(registry);
    let reopened = WorkspaceRegistry::open(&path, Some("machine:local")).unwrap();
    assert!(reopened.authenticate_machine(&token).unwrap().is_none());
    assert!(reopened.resolve(&id, None, WorkspaceAccess::Owner).is_err());
}

#[tokio::test]
async fn a_revoked_machine_bearer_fails_on_the_next_http_request() {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use mcpmem::http::{HttpState, TestSetup};
    use mcpmem::tools::ToolCategory;
    use tower::ServiceExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.sqlite");
    let state = HttpState::for_test(TestSetup {
        db_path: path.clone(),
        oauth: None,
        auth_token: Some(Arc::<str>::from("configured-static-token")),
        metadata_fetch: None,
        bearer_scopes: ToolCategory::ALL.to_vec(),
        enabled_categories: ToolCategory::ALL.to_vec(),
        now_us: None,
        ui_enabled: true,
    });
    let registry = WorkspaceRegistry::open(&path, None).unwrap();
    let (id, credential) = registry
        .create_machine("http reader", &["graph-read".to_owned()])
        .unwrap();
    let app = mcpmem::http::router(state);
    let request = || {
        Request::post("/mcp")
            .header("authorization", format!("Bearer {credential}"))
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .unwrap()
    };
    let first = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let bytes = first.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let tools = body["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "read_graph"));
    assert!(tools.iter().all(|tool| tool["name"] != "delete_entities"));
    let admin = app
        .clone()
        .oneshot(
            Request::get("/ui/api/principals")
                .header("authorization", format!("Bearer {credential}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admin.status(), StatusCode::FORBIDDEN);
    assert!(registry.revoke_machine(&id).unwrap());
    let next = app.oneshot(request()).await.unwrap();
    assert_eq!(next.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn machine_credentials_cannot_hold_admin_scope() {
    let (_dir, registry) = registry();
    assert!(
        registry
            .create_machine("attempted admin", &["graph-read".into(), "admin".into()])
            .is_err()
    );
    assert!(registry.list_machines().unwrap().is_empty());
    let (id, token) = registry
        .create_machine("ordinary reader", &["graph-read".to_owned()])
        .unwrap();
    let workspace = registry
        .create("machine:local", "private", Visibility::Private, |_| Ok(()))
        .unwrap();
    registry
        .grant("machine:local", &workspace.workspace_id, &id, "writer")
        .unwrap();
    let principal = registry.authenticate_machine(&token).unwrap().unwrap();
    assert!(!principal.scopes.contains("admin"));
}

#[test]
fn only_an_admin_human_or_trusted_local_can_manage_machine_accounts() {
    use std::collections::BTreeSet;

    use mcpmem::authz::{bearer_principal, local_principal, may_manage_machines, oauth_principal};
    use mcpmem::tools::ToolCategory;

    let admin = oauth_principal("human:admin", BTreeSet::from(["admin".into()]));
    let reader = oauth_principal("human:reader", BTreeSet::from(["graph-read".into()]));
    let mut static_bearer = bearer_principal(ToolCategory::ALL);
    assert!(may_manage_machines(&admin));
    assert!(may_manage_machines(&local_principal()));
    assert!(!may_manage_machines(&reader));
    assert!(!may_manage_machines(&static_bearer));
    static_bearer.scopes.insert("admin".into());
    assert!(
        !may_manage_machines(&static_bearer),
        "a machine cannot become an administrator through a scope"
    );
    static_bearer.id = "machine:issued".into();
    assert!(!may_manage_machines(&static_bearer));
    let forged_human = oauth_principal("machine:issued", BTreeSet::from(["admin".into()]));
    assert!(!may_manage_machines(&forged_human));
}

#[cfg(feature = "oauth")]
mod support;

#[cfg(feature = "oauth")]
mod migration {
    use std::sync::Arc;

    use super::support;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use mcpmem::http::{HttpState, TestSetup};
    use mcpmem_oauth::store::{Grant, Store, TokenKind};
    use rusqlite::Connection;
    use tower::ServiceExt;

    const NOW_US: i64 = 1_700_000_000_000_000;

    fn legacy_graph(path: &std::path::Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE entity(id INTEGER PRIMARY KEY, name_hash INTEGER NOT NULL, name TEXT NOT NULL, type_id INTEGER NOT NULL,
                obs_count INTEGER NOT NULL DEFAULT 0, out_deg INTEGER NOT NULL DEFAULT 0, in_deg INTEGER NOT NULL DEFAULT 0,
                created_us INTEGER NOT NULL, updated_us INTEGER NOT NULL, flags INTEGER NOT NULL DEFAULT 0) STRICT;
             CREATE TABLE observation(id INTEGER PRIMARY KEY, entity_id INTEGER NOT NULL, idx INTEGER NOT NULL, body TEXT NOT NULL, created_us INTEGER NOT NULL) STRICT;
             CREATE TABLE relation(from_id INTEGER NOT NULL, to_id INTEGER NOT NULL, type_id INTEGER NOT NULL, created_us INTEGER NOT NULL) STRICT;
             CREATE TABLE type_dict(id INTEGER PRIMARY KEY, kind INTEGER NOT NULL, name TEXT NOT NULL, count INTEGER NOT NULL DEFAULT 0) STRICT;
             CREATE TABLE graph_stat(key TEXT NOT NULL PRIMARY KEY, value INTEGER NOT NULL) STRICT, WITHOUT ROWID;
             INSERT INTO graph_stat VALUES('entities',0),('relations',0),('observations',0),('entity_seq',0),('obs_seq',0);
             CREATE VIRTUAL TABLE obs_fts USING fts5(body, content='observation', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
             CREATE TRIGGER obs_fts_bd BEFORE DELETE ON observation BEGIN
               INSERT INTO obs_fts(obs_fts, rowid, body) VALUES ('delete', old.id, old.body);
             END;
             CREATE TABLE schema_migration(version INTEGER PRIMARY KEY, checksum TEXT NOT NULL, applied_at_us INTEGER NOT NULL) STRICT;",
        )
        .unwrap();
        for &(version, sql) in mcpmem_core::events::MIGRATIONS
            .iter()
            .filter(|(version, _)| *version <= 13)
        {
            conn.execute_batch(sql).unwrap();
            conn.execute(
                "INSERT INTO schema_migration VALUES(?1,?2,1)",
                rusqlite::params![version, mcpmem_core::events::sha256(sql.as_bytes())],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn unauthenticated_http_refuses_before_it_marks_a_legacy_graph() {
        use std::fs::File;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let conn = legacy_graph(&path);
        let binary =
            std::env::var("CARGO_BIN_EXE_mcpmem").unwrap_or_else(|_| "target/debug/mcpmem".into());
        let log_path = path.to_string_lossy() + ".refusal.log";
        let log = File::create(&*log_path).expect("create server log");
        let log_err = log.try_clone().expect("clone server log handle");
        let mut child = Command::new(&binary)
            .arg("-f")
            .arg(&path)
            .arg("--transport")
            .arg("http")
            .arg("--legacy-owner-id")
            .arg("machine:local")
            .arg("--enable-all")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("an unauthenticated HTTP server stayed active");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        assert!(!status.success(), "the HTTP listener must refuse startup");
        let output = std::fs::read_to_string(&*log_path)
            .unwrap_or_else(|e| format!("<log unreadable: {e}>"));
        let reason = output.to_ascii_lowercase();
        assert!(
            reason.contains("oauth") && reason.contains("bearer"),
            "the refusal must name both credential options: {output}"
        );
        let latest: i64 = conn
            .query_row("SELECT max(version) FROM schema_migration", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(latest, 13, "refused startup must not migrate the graph");
        assert!(
            !std::path::PathBuf::from(format!("{}.workspaces.sqlite", path.display())).exists(),
            "refused startup must not create the workspace registry"
        );
    }

    #[tokio::test]
    async fn a_live_legacy_oauth_token_is_refused_on_the_first_http_request_after_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let conn = legacy_graph(&path);
        let token = mcpmem_oauth::new_token();
        let store = Store::new(Connection::open(&path).unwrap());
        store
            .put_token(
                &token,
                TokenKind::Access,
                &Grant {
                    client_id: "old-client".into(),
                    principal: "Old display name".into(),
                    scopes: vec!["graph-read".into()],
                    resource: format!("{}/mcp", support::PUBLIC_URL),
                    family: "legacy-family".into(),
                },
                NOW_US - 1_000_000,
                NOW_US + 3_600_000_000,
            )
            .unwrap();
        assert!(store.find_access(&token, NOW_US).unwrap().is_some());
        let old_version: i64 = conn
            .query_row("SELECT max(version) FROM schema_migration", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(old_version, 13);
        drop(store);

        let scopes = support::Scopes::all();
        let state = HttpState::for_test(TestSetup {
            db_path: path,
            oauth: Some(support::oauth_config("https://issuer.invalid")),
            auth_token: None,
            metadata_fetch: None,
            bearer_scopes: scopes.bearer,
            enabled_categories: scopes.enabled,
            now_us: Some(Arc::new(|| NOW_US)),
            ui_enabled: true,
        });
        let response = mcpmem::http::router(state)
            .oneshot(
                Request::post("/mcp")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            conn.query_row("SELECT max(version) FROM schema_migration", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            15
        );
        assert!(
            Store::new(Connection::open(dir.path().join("legacy.sqlite")).unwrap())
                .find_access(&token, NOW_US)
                .unwrap()
                .is_none()
        );
    }
}
