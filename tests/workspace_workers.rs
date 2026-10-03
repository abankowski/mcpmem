#![cfg(feature = "webhooks")]

use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;
use mcpmem::runtime::{RoleService, WebhookService};
use mcpmem::types::EntityInput;
use mcpmem::workspace::{Visibility, WorkspaceRegistry};
use mcpmem_core::subscriptions::{SubscriptionRepository, WebhookSubscription};
use mcpmem_webhook::{
    DeliveryConnector, DeliveryResponse, Resolver, SecretProvider, SignedRequest, SigningKey,
    ValidatedEndpoint, WebhookWorker, WorkerError, WorkerPoll,
};
use parking_lot::Mutex;
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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

fn subscribe(path: &Path, host: &str) {
    let conn = Connection::open(path).unwrap();
    SubscriptionRepository::new(&conn)
        .upsert(WebhookSubscription {
            subscription_id: uuid::Uuid::new_v4(),
            endpoint: format!("https://{host}/receive"),
            event_operations: vec![],
            entity_types: vec![],
            ignored_origins: vec![],
            consumer_origin: host.into(),
            secret_ref: "vault://test/workspaces".into(),
            enabled: true,
        })
        .unwrap();
}

fn create_entity(path: &Path, name: &str) {
    graph(path)
        .create_entities(&[EntityInput {
            name: name.into(),
            entity_type: "note".into(),
            observations: vec![format!("observation for {name}").into()],
            attributes: None,
        }])
        .unwrap();
}

fn count_outbox(path: &Path, state: &str) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM event_outbox WHERE state=?1",
            [state],
            |row| row.get(0),
        )
        .unwrap()
}

#[derive(Clone, Default)]
struct RecordingConnector(Arc<Mutex<Vec<(String, String)>>>);

impl DeliveryConnector for RecordingConnector {
    fn send(
        &self,
        endpoint: &ValidatedEndpoint,
        request: SignedRequest,
    ) -> Result<DeliveryResponse, WorkerError> {
        let envelope: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        self.0.lock().push((
            endpoint.url.host_str().unwrap().to_owned(),
            envelope["entity"]["after"]["name"]
                .as_str()
                .unwrap()
                .to_owned(),
        ));
        Ok(DeliveryResponse {
            status: 204,
            retry_after_us: None,
        })
    }
}

struct TestSecrets;
impl SecretProvider for TestSecrets {
    fn signing_key(&self, _: &str) -> Result<SigningKey, WorkerError> {
        SigningKey::new(b"workspace-test-key".to_vec())
    }
}

struct PublicResolver;
impl Resolver for PublicResolver {
    fn resolve(&self, _: &str) -> Result<Vec<IpAddr>, WorkerError> {
        Ok(vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))])
    }
}

#[test]
fn webhook_turns_deliver_each_graphs_own_event_and_preserve_the_legacy_subscription() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_path = dir.path().join("legacy.sqlite");
    drop(graph(&legacy_path));
    subscribe(&legacy_path, "legacy.example.test");

    // Remove migrations 14 and 15 to reproduce a version-13 graph.
    // Migration 14 changes OAuth rows but does not alter webhook tables.
    let historical = Connection::open(&legacy_path).unwrap();
    historical
        .execute_batch(
            "BEGIN IMMEDIATE;
             DROP TABLE attachment_upload_chunk;
             DROP TABLE attachment_upload;
             DROP TABLE attachment_job;
             DROP TABLE attachment_chunk;
             DROP TABLE attachment_text;
             DROP TABLE attachment;
             DROP TABLE chunk_vector;
             DROP TABLE chunk_index_job;
             COMMIT;",
        )
        .unwrap();
    historical
        .execute_batch(mcpmem_core::events::MIGRATIONS[8].1)
        .unwrap();
    historical
        .execute("DELETE FROM schema_migration WHERE version IN (14,15)", [])
        .unwrap();
    assert_eq!(
        historical
            .query_row("SELECT max(version) FROM schema_migration", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        13
    );
    let registry = Arc::new(WorkspaceRegistry::open(&legacy_path, Some("machine:local")).unwrap());
    assert_eq!(
        historical
            .query_row("SELECT max(version) FROM schema_migration", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        15
    );
    assert_eq!(
        historical
            .query_row(
                "SELECT count(*) FROM webhook_subscription WHERE enabled=1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1,
        "the registry migration must retain the active legacy endpoint"
    );

    let second_id = registry
        .create("machine:local", "second", Visibility::Private, |path| {
            drop(graph(path));
            Ok(())
        })
        .unwrap()
        .workspace_id;
    let second_path = registry
        .all_paths()
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == &second_id)
        .unwrap()
        .1;
    assert_ne!(second_path, legacy_path);
    assert_eq!(registry.all_paths().unwrap().len(), 2);
    subscribe(&second_path, "second.example.test");
    create_entity(&legacy_path, "legacy-only");
    create_entity(&second_path, "second-only");
    assert_eq!(count_outbox(&legacy_path, "pending"), 1);
    assert_eq!(count_outbox(&second_path, "pending"), 1);

    let connector = RecordingConnector::default();
    let factory_connector = connector.clone();
    let worker_factory = Arc::new(move |path: &Path| -> Arc<dyn WorkerPoll> {
        Arc::new(WebhookWorker::new(
            path,
            factory_connector.clone(),
            TestSecrets,
            BTreeSet::from(["legacy.example.test".into(), "second.example.test".into()]),
            PublicResolver,
        ))
    });
    let service = WebhookService::with_workspaces(Arc::clone(&registry), worker_factory);
    let now = mcpmem_core::events::now_us();
    assert_eq!(service.run_once(now).unwrap().completed, 1);
    assert_eq!(
        count_outbox(&legacy_path, "done") + count_outbox(&second_path, "done"),
        1,
        "one service turn must poll at most one graph"
    );
    assert_eq!(service.run_once(now + 1).unwrap().completed, 1);
    assert_eq!(count_outbox(&legacy_path, "done"), 1);
    assert_eq!(count_outbox(&second_path, "done"), 1);
    let mut delivered = connector.0.lock().clone();
    delivered.sort();
    assert_eq!(
        delivered,
        [
            ("legacy.example.test".into(), "legacy-only".into()),
            ("second.example.test".into(), "second-only".into()),
        ],
        "each endpoint must receive only the event from its own graph"
    );
}

struct AlwaysFails;

impl WorkerPoll for AlwaysFails {
    fn poll(&self, _: i64) -> Result<mcpmem_webhook::DeliveryReport, WorkerError> {
        Err(WorkerError::Delivery("injected graph failure".into()))
    }
}

#[tokio::test]
async fn a_failed_webhook_graph_does_not_stop_delivery_from_the_next_graph() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_path = dir.path().join("legacy.sqlite");
    let registry = Arc::new(WorkspaceRegistry::open(&legacy_path, Some("machine:local")).unwrap());
    registry
        .create("machine:local", "next", Visibility::Private, |path| {
            drop(graph(path));
            Ok(())
        })
        .unwrap();
    let paths = registry.all_paths().unwrap();
    assert_eq!(paths.len(), 2);
    // Legacy is the first scheduled row (rowid 1), so its failing turn runs
    // before the healthy graph is ever polled: delivery succeeds only if the
    // role survives the per-graph error.
    let failed_path = legacy_path.clone();
    let healthy_path = paths
        .iter()
        .find(|(_, path)| path != &failed_path)
        .unwrap()
        .1
        .clone();
    subscribe(&healthy_path, "healthy.example.test");
    create_entity(&healthy_path, "healthy-only");
    assert_eq!(count_outbox(&healthy_path, "pending"), 1);

    let connector = RecordingConnector::default();
    let factory_connector = connector.clone();
    let worker_factory = Arc::new(move |path: &Path| -> Arc<dyn WorkerPoll> {
        if path == failed_path {
            Arc::new(AlwaysFails)
        } else {
            Arc::new(WebhookWorker::new(
                path,
                factory_connector.clone(),
                TestSecrets,
                BTreeSet::from(["healthy.example.test".into()]),
                PublicResolver,
            ))
        }
    });
    let service = WebhookService::with_workspaces(registry, worker_factory);
    let mut role = tokio::spawn(service.run());
    tokio::select! {
        outcome = &mut role => panic!("one failed graph stopped the webhook role: {outcome:?}"),
        delivered = tokio::time::timeout(Duration::from_secs(2), async {
            while count_outbox(&healthy_path, "done") != 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }) => delivered.expect("the healthy graph must deliver after the failed graph"),
    }
    role.abort();
    assert_eq!(
        connector.0.lock().as_slice(),
        &[("healthy.example.test".into(), "healthy-only".into())]
    );
}
