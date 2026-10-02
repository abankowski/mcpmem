use std::{num::NonZeroUsize, path::Path};

use mcpmem::workspace::{Visibility, WorkspaceAccess, WorkspaceRegistry};
use mcpmem_core::graph::GraphHandle;
use mcpmem_core::storage::{Durability, SqliteTuning};
use mcpmem_core::types::EntityInput;
use rusqlite::{Connection, params};

fn graph(path: &Path) -> GraphHandle {
    GraphHandle::new(
        path,
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap()
}

fn add_human(memory_path: &Path, subject: &str) -> String {
    let conn = Connection::open(memory_path).unwrap();
    conn.execute(
        "INSERT INTO runtime_principal (iss,sub,name,label,scopes,created_us,updated_us) VALUES (?1,?2,?2,NULL,'[\"graph-read\",\"graph-write\"]',1,1)",
        params!["https://issuer.example", subject],
    )
    .unwrap();
    format!(
        "human:{}",
        mcpmem_oauth::principal_id("https://issuer.example", subject)
    )
}

fn create_graph(registry: &WorkspaceRegistry, name: &str) -> String {
    registry
        .create("machine:local", name, Visibility::Private, |path| {
            drop(graph(path));
            Ok(())
        })
        .unwrap()
        .workspace_id
}

#[test]
fn private_reader_and_writer_grants_take_effect_on_the_next_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    let registry = WorkspaceRegistry::open(&legacy, Some("machine:local")).unwrap();
    let reader = add_human(&legacy, "reader");
    let writer = add_human(&legacy, "writer");
    let outsider = add_human(&legacy, "outsider");
    let id = create_graph(&registry, "private notes");

    assert!(
        registry
            .resolve(&reader, Some(&id), WorkspaceAccess::Read)
            .is_err()
    );
    registry
        .grant("machine:local", &id, &reader, "reader")
        .unwrap();
    registry
        .grant("machine:local", &id, &writer, "writer")
        .unwrap();
    assert!(
        registry
            .resolve(&reader, Some(&id), WorkspaceAccess::Read)
            .is_ok()
    );
    assert!(
        registry
            .resolve(&reader, Some(&id), WorkspaceAccess::Write)
            .is_err()
    );
    assert!(
        registry
            .resolve(&writer, Some(&id), WorkspaceAccess::Read)
            .is_ok()
    );
    assert!(
        registry
            .resolve(&writer, Some(&id), WorkspaceAccess::Write)
            .is_ok()
    );
    assert!(
        registry
            .resolve(&outsider, Some(&id), WorkspaceAccess::Read)
            .is_err()
    );
    assert!(registry.grant(&writer, &id, &outsider, "reader").is_err());
    assert!(
        registry
            .grant("machine:local", &id, "human:unknown", "writer")
            .is_err()
    );
    let visible_to_reader = registry.list(&reader, None, 100).unwrap();
    assert!(
        visible_to_reader
            .workspaces
            .iter()
            .any(|view| view.workspace_id == id)
    );
    let visible_to_writer = registry.list(&writer, None, 100).unwrap();
    assert!(
        visible_to_writer
            .workspaces
            .iter()
            .any(|view| view.workspace_id == id)
    );
    let visible_to_outsider = registry.list(&outsider, None, 100).unwrap();
    assert!(
        visible_to_outsider
            .workspaces
            .iter()
            .all(|view| view.workspace_id != id),
        "a private graph must not expose its name or ID through the list"
    );

    registry.revoke("machine:local", &id, &writer).unwrap();
    assert!(
        registry
            .resolve(&writer, Some(&id), WorkspaceAccess::Read)
            .is_err()
    );
    assert!(
        registry
            .resolve(&writer, Some(&id), WorkspaceAccess::Write)
            .is_err()
    );
}

#[test]
fn two_graphs_with_the_same_entity_name_keep_their_rows_separate() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    let registry = WorkspaceRegistry::open(&legacy, Some("machine:local")).unwrap();
    let first = create_graph(&registry, "first");
    let second = create_graph(&registry, "second");
    let first_path = registry
        .resolve("machine:local", Some(&first), WorkspaceAccess::Write)
        .unwrap()
        .graph_path;
    let second_path = registry
        .resolve("machine:local", Some(&second), WorkspaceAccess::Write)
        .unwrap()
        .graph_path;
    assert_ne!(first_path, second_path);

    graph(&first_path)
        .create_entities(&[EntityInput {
            name: "same-name".into(),
            entity_type: "test".into(),
            observations: vec!["only-first".into()],
            attributes: None,
        }])
        .unwrap();
    graph(&second_path)
        .create_entities(&[EntityInput {
            name: "same-name".into(),
            entity_type: "test".into(),
            observations: vec!["only-second".into()],
            attributes: None,
        }])
        .unwrap();

    let first_entity = graph(&first_path).get_entity("same-name").unwrap().unwrap();
    let second_entity = graph(&second_path)
        .get_entity("same-name")
        .unwrap()
        .unwrap();
    assert_eq!(first_entity.observations[0].body, "only-first");
    assert_eq!(second_entity.observations[0].body, "only-second");
    assert_eq!(
        Connection::open(&first_path)
            .unwrap()
            .query_row("SELECT count(*) FROM entity WHERE flags=0", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        1
    );
    assert_eq!(
        Connection::open(&second_path)
            .unwrap()
            .query_row("SELECT count(*) FROM entity WHERE flags=0", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        1
    );
}

#[test]
fn an_explicit_workspace_overrides_selection_without_changing_the_saved_default() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    let registry = WorkspaceRegistry::open(&legacy, Some("machine:local")).unwrap();
    let first = create_graph(&registry, "first");
    let second = create_graph(&registry, "second");
    registry.set_default("machine:local", &first).unwrap();

    assert_eq!(
        registry
            .resolve("machine:local", None, WorkspaceAccess::Read)
            .unwrap()
            .workspace_id,
        first
    );
    assert_eq!(
        registry
            .resolve("machine:local", Some(&second), WorkspaceAccess::Read)
            .unwrap()
            .workspace_id,
        second
    );
    assert_eq!(
        registry
            .resolve("machine:local", None, WorkspaceAccess::Read)
            .unwrap()
            .workspace_id,
        first,
        "a per-call override must not update the saved default"
    );

    let other = add_human(&legacy, "other");
    assert!(
        registry
            .resolve(&other, None, WorkspaceAccess::Read)
            .is_err()
    );
    registry
        .grant("machine:local", &first, &other, "reader")
        .unwrap();
    assert!(
        registry
            .resolve(&other, None, WorkspaceAccess::Read)
            .is_err()
    );
    registry.set_default(&other, &first).unwrap();
    assert_eq!(
        registry
            .resolve(&other, None, WorkspaceAccess::Read)
            .unwrap()
            .workspace_id,
        first
    );
    registry.revoke("machine:local", &first, &other).unwrap();
    assert!(
        registry
            .resolve(&other, None, WorkspaceAccess::Read)
            .is_err()
    );
}

#[test]
fn a_public_graph_allows_read_but_never_implies_write() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    let registry = WorkspaceRegistry::open(&legacy, Some("machine:local")).unwrap();
    let other = add_human(&legacy, "public-reader");
    let id = registry
        .create("machine:local", "shared", Visibility::Public, |path| {
            drop(graph(path));
            Ok(())
        })
        .unwrap()
        .workspace_id;
    assert!(
        registry
            .resolve(&other, Some(&id), WorkspaceAccess::Read)
            .is_ok()
    );
    assert!(
        registry
            .resolve(&other, Some(&id), WorkspaceAccess::Write)
            .is_err()
    );
    registry
        .set_visibility("machine:local", &id, Visibility::Private)
        .unwrap();
    assert!(
        registry
            .resolve(&other, Some(&id), WorkspaceAccess::Read)
            .is_err()
    );
}

#[test]
fn file_backed_human_owner_must_still_be_registered_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    let principal = mcpmem::principals::PrincipalEntry {
        name: "owner".into(),
        iss: "https://issuer.example".into(),
        sub: "owner-1".into(),
        label: None,
        scopes: vec!["graph-read".into(), "graph-write".into()],
    };
    let id = format!(
        "human:{}",
        mcpmem_oauth::principal_id(&principal.iss, &principal.sub)
    );
    let registry = WorkspaceRegistry::open_with_principals(
        &legacy,
        Some(&id),
        std::slice::from_ref(&principal),
        false,
    )
    .unwrap();
    assert!(registry.resolve(&id, None, WorkspaceAccess::Write).is_ok());
    drop(registry);
    assert!(
        WorkspaceRegistry::open_with_principals(&legacy, None, &[], false).is_err(),
        "removing a file-backed owner must refuse startup"
    );
    let registry = WorkspaceRegistry::open_with_principals(&legacy, None, &[principal], false)
        .expect("a valid registry no longer needs legacy-owner-id");
    assert!(registry.resolve(&id, None, WorkspaceAccess::Read).is_ok());
}

#[test]
fn an_unconfigured_static_machine_cannot_own_the_legacy_graph() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    assert!(
        WorkspaceRegistry::open_with_principals(&legacy, Some("machine:static"), &[], false)
            .is_err()
    );
    let registry =
        WorkspaceRegistry::open_with_principals(&legacy, Some("machine:static"), &[], true)
            .unwrap();
    assert!(
        registry
            .resolve("machine:static", None, WorkspaceAccess::Read)
            .is_ok()
    );
}

#[test]
fn a_newer_registry_version_refuses_startup_without_replacing_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    drop(WorkspaceRegistry::open(&legacy, Some("machine:local")).unwrap());
    let sidecar = std::path::PathBuf::from(format!("{}.workspaces.sqlite", legacy.display()));
    Connection::open(&sidecar)
        .unwrap()
        .execute("INSERT INTO workspace_registry_version VALUES(2)", [])
        .unwrap();
    assert!(WorkspaceRegistry::open(&legacy, None).is_err());
    let owner: String = Connection::open(&sidecar)
        .unwrap()
        .query_row("SELECT owner_id FROM workspace", [], |row| row.get(0))
        .unwrap();
    assert_eq!(owner, "machine:local");
}

#[test]
fn list_cursor_pages_accessible_graphs_without_repeating_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    let registry = WorkspaceRegistry::open(&legacy, Some("machine:local")).unwrap();
    create_graph(&registry, "one");
    create_graph(&registry, "two");
    let expected = registry.list("machine:local", None, 100).unwrap();
    assert_eq!(expected.workspaces.len(), 3);
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = registry
            .list("machine:local", cursor.as_deref(), 1)
            .unwrap();
        seen.extend(page.workspaces.into_iter().map(|view| view.workspace_id));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(
        seen,
        expected
            .workspaces
            .into_iter()
            .map(|view| view.workspace_id)
            .collect::<Vec<_>>()
    );
    assert!(
        registry
            .list("machine:local", Some("bad cursor"), 1)
            .is_err()
    );
}

#[test]
fn restart_refuses_a_missing_registered_graph_without_recreating_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    let registry = WorkspaceRegistry::open(&legacy, Some("machine:local")).unwrap();
    let id = create_graph(&registry, "keep");
    let path = registry
        .resolve("machine:local", Some(&id), WorkspaceAccess::Read)
        .unwrap()
        .graph_path;
    assert!(path.exists());
    drop(registry);
    std::fs::remove_file(&path).unwrap();
    assert!(WorkspaceRegistry::open(&legacy, None).is_err());
    assert!(
        !path.exists(),
        "startup must not silently replace a missing graph"
    );
}

#[test]
fn restart_refuses_a_removed_owner_of_a_nonlegacy_graph() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    let registry = WorkspaceRegistry::open(&legacy, Some("machine:local")).unwrap();
    let owner = add_human(&legacy, "second-owner");
    let workspace = registry
        .create(&owner, "private", Visibility::Private, |path| {
            drop(graph(path));
            Ok(())
        })
        .unwrap();
    let path = registry
        .resolve(&owner, Some(&workspace.workspace_id), WorkspaceAccess::Read)
        .unwrap()
        .graph_path;
    drop(registry);
    Connection::open(&legacy)
        .unwrap()
        .execute(
            "DELETE FROM runtime_principal WHERE iss=?1 AND sub=?2",
            params!["https://issuer.example", "second-owner"],
        )
        .unwrap();
    assert!(WorkspaceRegistry::open(&legacy, None).is_err());
    assert!(
        path.exists(),
        "a refused restart must not discard the graph"
    );
}

fn uppercase_graph() -> (tempfile::TempDir, WorkspaceRegistry, String) {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("memory.sqlite");
    let registry = WorkspaceRegistry::open(&legacy, Some("machine:local")).unwrap();
    let id = create_graph(&registry, "case-insensitive-id");
    (dir, registry, id)
}

#[test]
fn grant_accepts_an_uppercase_workspace_uuid() {
    let (dir, registry, id) = uppercase_graph();
    let reader = add_human(&dir.path().join("memory.sqlite"), "upper-grant");
    registry
        .grant("machine:local", &id.to_ascii_uppercase(), &reader, "reader")
        .unwrap();
    assert!(
        registry
            .resolve(&reader, Some(&id), WorkspaceAccess::Read)
            .is_ok()
    );
}

#[test]
fn revoke_accepts_an_uppercase_workspace_uuid() {
    let (dir, registry, id) = uppercase_graph();
    let reader = add_human(&dir.path().join("memory.sqlite"), "upper-revoke");
    registry
        .grant("machine:local", &id, &reader, "reader")
        .unwrap();
    assert!(
        registry
            .revoke("machine:local", &id.to_ascii_uppercase(), &reader)
            .unwrap()
    );
    assert!(
        registry
            .resolve(&reader, Some(&id), WorkspaceAccess::Read)
            .is_err()
    );
}

#[test]
fn visibility_accepts_an_uppercase_workspace_uuid() {
    let (dir, registry, id) = uppercase_graph();
    let reader = add_human(&dir.path().join("memory.sqlite"), "upper-visibility");
    registry
        .set_visibility(
            "machine:local",
            &id.to_ascii_uppercase(),
            Visibility::Public,
        )
        .unwrap();
    assert!(
        registry
            .resolve(&reader, Some(&id), WorkspaceAccess::Read)
            .is_ok()
    );
}

#[test]
fn grants_accepts_an_uppercase_workspace_uuid() {
    let (dir, registry, id) = uppercase_graph();
    let reader = add_human(&dir.path().join("memory.sqlite"), "upper-list");
    registry
        .grant("machine:local", &id, &reader, "writer")
        .unwrap();
    let grants = registry
        .grants("machine:local", &id.to_ascii_uppercase())
        .unwrap();
    assert!(
        grants
            .iter()
            .any(|grant| grant.principal_id == reader && grant.role == "writer")
    );
}
