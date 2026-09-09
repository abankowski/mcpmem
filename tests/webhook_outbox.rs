use memory_core::{
    graph::GraphHandle,
    mutation::{MutationContext, MutationRequest, MutationService, ObservationUpdate},
    storage::{Durability, SqliteTuning},
    subscriptions::{SubscriptionRepository, WebhookSubscription},
    types::Entity,
};
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::{
    net::{IpAddr, Ipv4Addr},
    num::NonZeroUsize,
    path::Path,
};

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
fn entity(name: &str) -> Entity {
    Entity {
        name: name.into(),
        entity_type: "note".into(),
        observations: vec!["secret observation".into()],
    }
}
fn count(conn: &rusqlite::Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn webhook_bootstraps_fresh_graph_and_reopens_without_resetting_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.db");
    let connector = TestConnector::default();
    let worker = webhook_worker::WebhookWorker::new(
        &path,
        &connector,
        TestSecrets,
        BTreeSet::new(),
        TestResolver(vec![]),
    );
    assert_eq!(worker.run_once(1).unwrap().claimed, 0);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(count(&conn, "entity"), 0);
    assert_eq!(count(&conn, "graph_stat"), 5);
    let graph = graph(&path);
    graph.create_entities(&[entity("retained")]).unwrap();
    assert_eq!(worker.run_once(1).unwrap().claimed, 0);
    assert_eq!(
        graph.get_entity("retained").unwrap(),
        Some(entity("retained"))
    );
}

#[test]
fn matching_subscriptions_are_atomically_enqueued_and_same_origin_is_suppressed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let subscription = WebhookSubscription {
        subscription_id: uuid::Uuid::new_v4(),
        endpoint: "https://hooks.example.test/receive".into(),
        event_operations: vec![],
        entity_types: vec!["note".into()],
        ignored_origins: vec![],
        consumer_origin: "consumer-a".into(),
        secret_ref: "vault://webhook/a".into(),
        enabled: true,
    };
    SubscriptionRepository::new(&conn)
        .upsert(subscription.clone())
        .unwrap();
    let mut context = MutationContext::local();
    context.origin = "producer".into();
    MutationService::new(&graph)
        .apply(
            MutationRequest::CreateEntities {
                entities: vec![entity("a")],
            },
            context,
        )
        .unwrap();
    assert_eq!(count(&conn, "change_event"), 1);
    assert_eq!(count(&conn, "event_outbox"), 1);
    let mut same = MutationContext::local();
    same.origin = "consumer-a".into();
    MutationService::new(&graph)
        .apply(
            MutationRequest::CreateEntities {
                entities: vec![entity("b")],
            },
            same,
        )
        .unwrap();
    assert_eq!(count(&conn, "event_outbox"), 1);
    SubscriptionRepository::new(&conn)
        .delete(subscription.subscription_id)
        .unwrap();
}

#[test]
fn failed_mutations_leave_no_webhook_outbox_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    SubscriptionRepository::new(&conn)
        .upsert(WebhookSubscription {
            subscription_id: uuid::Uuid::new_v4(),
            endpoint: "https://hooks.example.test/receive".into(),
            event_operations: vec![],
            entity_types: vec![],
            ignored_origins: vec![],
            consumer_origin: "consumer-a".into(),
            secret_ref: "vault://webhook/a".into(),
            enabled: true,
        })
        .unwrap();
    graph.create_entities(&[entity("ok")]).unwrap();
    let result = MutationService::new(&graph).apply(
        MutationRequest::AddObservations {
            observations: vec![
                ObservationUpdate {
                    entity_name: "ok".into(),
                    contents: vec!["will roll back".into()],
                },
                ObservationUpdate {
                    entity_name: "missing".into(),
                    contents: vec!["fails".into()],
                },
            ],
        },
        MutationContext::local(),
    );
    assert!(result.is_err());
    assert_eq!(count(&conn, "change_event"), 1);
    assert_eq!(count(&conn, "event_outbox"), 1);
}

#[test]
fn egress_policy_rejects_private_and_malformed_endpoints() {
    use webhook_worker::validate_endpoint;
    let resolver = TestResolver(vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))]);
    let allowlist = BTreeSet::from(["example.test".into()]);
    assert!(validate_endpoint("http://example.test", &allowlist, &resolver).is_err());
    assert!(validate_endpoint("https://user@example.test", &allowlist, &resolver).is_err());
    assert!(validate_endpoint("https://example.test#fragment", &allowlist, &resolver).is_err());
    assert!(validate_endpoint("https://127.0.0.1", &allowlist, &resolver).is_err());
    assert!(validate_endpoint("https://not-allowed.test", &allowlist, &resolver).is_err());
    assert!(
        validate_endpoint(
            "https://example.test",
            &allowlist,
            &TestResolver(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        )
        .is_err()
    );
}

struct TestResolver(Vec<IpAddr>);
impl webhook_worker::Resolver for TestResolver {
    fn resolve(&self, _: &str) -> Result<Vec<IpAddr>, webhook_worker::WorkerError> {
        Ok(self.0.clone())
    }
}

struct TestSecrets;
impl webhook_worker::SecretProvider for TestSecrets {
    fn signing_key(
        &self,
        _: &str,
    ) -> Result<webhook_worker::SigningKey, webhook_worker::WorkerError> {
        webhook_worker::SigningKey::new(b"test-key".to_vec())
    }
}
#[derive(Default)]
struct TestConnector {
    request: Mutex<Option<webhook_worker::SignedRequest>>,
    endpoint: Mutex<Option<webhook_worker::ValidatedEndpoint>>,
}
impl webhook_worker::DeliveryConnector for &TestConnector {
    fn send(
        &self,
        endpoint: &webhook_worker::ValidatedEndpoint,
        request: webhook_worker::SignedRequest,
    ) -> Result<webhook_worker::DeliveryResponse, webhook_worker::WorkerError> {
        *self.request.lock().unwrap() = Some(request);
        *self.endpoint.lock().unwrap() = Some(endpoint.clone());
        Ok(webhook_worker::DeliveryResponse {
            status: 204,
            retry_after_us: None,
        })
    }
}

#[test]
fn worker_signs_a_redacted_envelope_without_observations_or_secret_reference() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    graph.create_entities(&[entity("old")]).unwrap();
    SubscriptionRepository::new(&conn)
        .upsert(WebhookSubscription {
            subscription_id: uuid::Uuid::new_v4(),
            endpoint: "https://hooks.example.test/receive".into(),
            event_operations: vec![],
            entity_types: vec![],
            ignored_origins: vec![],
            consumer_origin: "consumer".into(),
            secret_ref: "vault://not-in-body".into(),
            enabled: true,
        })
        .unwrap();
    let mut context = MutationContext::local();
    context.origin = "producer".into();
    MutationService::new(&graph)
        .apply(
            MutationRequest::RenameEntity {
                old_name: "old".into(),
                new_name: "new".into(),
            },
            context.clone(),
        )
        .unwrap();
    let connector = TestConnector::default();
    let report = webhook_worker::WebhookWorker::new(
        &path,
        &connector,
        TestSecrets,
        BTreeSet::from(["hooks.example.test".into()]),
        TestResolver(vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))]),
    )
    .run_once(i64::MAX / 2)
    .unwrap();
    assert_eq!(report.completed, 1);
    let request = connector.request.lock().unwrap().clone().unwrap();
    let body = String::from_utf8(request.body.clone()).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(envelope["version"], 2);
    assert_eq!(envelope["operation"], "rename");
    assert_eq!(envelope["oldName"], "old");
    assert_eq!(envelope["newName"], "new");
    for field in [
        "eventId",
        "transactionId",
        "entityId",
        "entityRevision",
        "occurredAtUs",
        "origin",
        "correlationId",
        "causationId",
        "hopCount",
    ] {
        assert!(envelope.get(field).is_some(), "preserves {field}");
    }
    assert_eq!(envelope["origin"], "producer");
    assert_eq!(envelope["causationId"], serde_json::Value::Null);
    assert_eq!(envelope["hopCount"], 0);
    assert!(envelope.get("before").is_none(), "no entity snapshot");
    assert!(envelope.get("after").is_none(), "no entity snapshot");
    assert!(
        envelope.get("observations").is_none(),
        "no observation bodies"
    );
    assert!(!body.contains("secret observation"));
    assert!(!body.contains("vault://not-in-body"));
    assert_eq!(request.event_id.len(), 36);
    let endpoint = connector.endpoint.lock().unwrap().clone().unwrap();
    assert_eq!(endpoint.url.host_str(), Some("hooks.example.test"));
    assert_eq!(endpoint.address, "8.8.8.8:443".parse().unwrap());
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(b"test-key").unwrap();
    mac.update(request.timestamp_us.to_string().as_bytes());
    mac.update(b".");
    mac.update(&request.body);
    assert_eq!(
        request.signature,
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );

    MutationService::new(&graph)
        .apply(
            MutationRequest::CreateEntities {
                entities: vec![entity("plain")],
            },
            context,
        )
        .unwrap();
    let report = webhook_worker::WebhookWorker::new(
        &path,
        &connector,
        TestSecrets,
        BTreeSet::from(["hooks.example.test".into()]),
        TestResolver(vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))]),
    )
    .run_once(i64::MAX / 2)
    .unwrap();
    assert_eq!(report.completed, 1);
    let non_rename_request = connector.request.lock().unwrap().clone().unwrap();
    let non_rename: serde_json::Value = serde_json::from_slice(&non_rename_request.body).unwrap();
    assert_eq!(non_rename["operation"], "create");
    assert_eq!(
        non_rename.get("oldName"),
        Some(&serde_json::Value::Null),
        "non-rename envelope explicitly includes oldName: null"
    );
    assert_eq!(
        non_rename.get("newName"),
        Some(&serde_json::Value::Null),
        "non-rename envelope explicitly includes newName: null"
    );
}

#[test]
fn machine_ingress_derives_actor_and_raw_body_fingerprint() {
    use memory_core::auth::{Principal, PrincipalKind, machine_mutation_context};
    let principal = Principal {
        id: "automation-42".into(),
        kind: PrincipalKind::Machine,
        scopes: BTreeSet::from(["memory:write".into()]),
        allowed_origins: BTreeSet::from(["n8n".into()]),
    };
    let (context, fingerprint) = machine_mutation_context(
        &principal,
        "n8n",
        "request-1".into(),
        uuid::Uuid::new_v4(),
        None,
        0,
        br#"{"operation":"compact"}"#,
    )
    .unwrap();
    assert_eq!(context.actor, "automation-42");
    assert_eq!(
        fingerprint,
        memory_core::events::request_fingerprint(
            "POST",
            "/api/v1/mutations",
            br#"{"operation":"compact"}"#
        )
    );
    assert!(
        machine_mutation_context(
            &principal,
            "other",
            "request-2".into(),
            uuid::Uuid::new_v4(),
            None,
            0,
            b"{}"
        )
        .is_err()
    );
}

#[derive(Clone, Copy)]
struct StatusConnector(u16, Option<i64>);
impl webhook_worker::DeliveryConnector for StatusConnector {
    fn send(
        &self,
        _: &webhook_worker::ValidatedEndpoint,
        _: webhook_worker::SignedRequest,
    ) -> Result<webhook_worker::DeliveryResponse, webhook_worker::WorkerError> {
        Ok(webhook_worker::DeliveryResponse {
            status: self.0,
            retry_after_us: self.1,
        })
    }
}

fn subscription(id: uuid::Uuid) -> WebhookSubscription {
    WebhookSubscription {
        subscription_id: id,
        endpoint: "https://hooks.example.test/receive".into(),
        event_operations: vec![],
        entity_types: vec![],
        ignored_origins: vec![],
        consumer_origin: "consumer".into(),
        secret_ref: "secret".into(),
        enabled: true,
    }
}
fn allowlist() -> BTreeSet<String> {
    BTreeSet::from(["hooks.example.test".into()])
}
fn resolver() -> TestResolver {
    TestResolver(vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))])
}

#[test]
fn retry_after_is_clamped_and_deleted_subscription_dead_letters() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let id = uuid::Uuid::new_v4();
    SubscriptionRepository::new(&conn)
        .upsert(subscription(id))
        .unwrap();
    graph.create_entities(&[entity("a")]).unwrap();
    let worker = webhook_worker::WebhookWorker::new(
        &path,
        StatusConnector(503, Some(99_999_999_999)),
        TestSecrets,
        allowlist(),
        resolver(),
    );
    let now = memory_core::events::now_us();
    assert_eq!(worker.run_once(now).unwrap().retried, 1);
    let next: i64 = conn
        .query_row("SELECT next_attempt_us FROM event_outbox", [], |r| r.get(0))
        .unwrap();
    assert!(next <= now + 3_600_000_000);
    conn.execute(
        "UPDATE event_outbox SET state='pending',next_attempt_us=0",
        [],
    )
    .unwrap();
    SubscriptionRepository::new(&conn).delete(id).unwrap();
    assert_eq!(worker.run_once(now + 1).unwrap().dead, 1);
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT state FROM event_outbox", [], |r| r.get(0))
            .unwrap(),
        "dead"
    );
}

#[test]
fn filters_and_migration_reopen_are_compatible() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut selected = subscription(uuid::Uuid::new_v4());
    selected.event_operations = vec![memory_core::mutation::ChangeOperation::Delete];
    selected.entity_types = vec!["other".into()];
    selected.ignored_origins = vec!["producer".into()];
    SubscriptionRepository::new(&conn).upsert(selected).unwrap();
    let mut context = MutationContext::local();
    context.origin = "producer".into();
    MutationService::new(&graph)
        .apply(
            MutationRequest::CreateEntities {
                entities: vec![entity("a")],
            },
            context,
        )
        .unwrap();
    assert_eq!(count(&conn, "event_outbox"), 0);
    drop(graph);
    memory_core::schema::initialize_database(&conn).unwrap();
    assert_eq!(count(&conn, "schema_migration"), 2);
    assert!(
        conn.query_row::<String, _, _>(
            "SELECT checksum FROM schema_migration WHERE version=1",
            [],
            |r| r.get(0)
        )
        .unwrap()
        .len()
            == 64
    );
}

#[test]
fn migration_from_a_real_0001_database_applies_only_0002() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE schema_migration(version INTEGER PRIMARY KEY, checksum TEXT NOT NULL, applied_at_us INTEGER NOT NULL) STRICT;").unwrap();
    let sql = include_str!("../migrations/0001_change_events.sql");
    conn.execute_batch(sql).unwrap();
    conn.execute(
        "INSERT INTO schema_migration VALUES(1,?1,1)",
        [memory_core::events::sha256(sql.as_bytes())],
    )
    .unwrap();
    memory_core::schema::initialize_database(&conn).unwrap();
    assert_eq!(count(&conn, "schema_migration"), 2);
    assert_eq!(count(&conn, "entity"), 0);
    assert_eq!(count(&conn, "graph_stat"), 5);
    let historical: (String, i64) = conn
        .query_row(
            "SELECT checksum,applied_at_us FROM schema_migration WHERE version=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        historical,
        (
            "a48def8b25e9ecf813d3fa27a785893ba5de346a8b82f012cc543af2fecd5af2".into(),
            1
        )
    );
    assert_eq!(count(&conn, "webhook_subscription"), 0);
}

#[test]
fn operation_type_and_ignored_origin_filters_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("filters.db");
    let graph = graph(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut operation = subscription(uuid::Uuid::new_v4());
    operation.event_operations = vec![memory_core::mutation::ChangeOperation::Create];
    let mut kind = subscription(uuid::Uuid::new_v4());
    kind.entity_types = vec!["note".into()];
    let mut ignored = subscription(uuid::Uuid::new_v4());
    ignored.ignored_origins = vec!["blocked".into()];
    for item in [operation, kind, ignored] {
        SubscriptionRepository::new(&conn).upsert(item).unwrap();
    }
    let mut allowed = MutationContext::local();
    allowed.origin = "allowed".into();
    MutationService::new(&graph)
        .apply(
            MutationRequest::CreateEntities {
                entities: vec![entity("a")],
            },
            allowed,
        )
        .unwrap();
    assert_eq!(count(&conn, "event_outbox"), 3);
    let mut blocked = MutationContext::local();
    blocked.origin = "blocked".into();
    MutationService::new(&graph)
        .apply(
            MutationRequest::CreateEntities {
                entities: vec![entity("b")],
            },
            blocked,
        )
        .unwrap();
    assert_eq!(count(&conn, "event_outbox"), 5);
}

#[test]
fn operation_and_type_filters_each_reject_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("single-filter.db");
    let graph = graph(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut operation = subscription(uuid::Uuid::new_v4());
    operation.event_operations = vec![memory_core::mutation::ChangeOperation::Delete];
    SubscriptionRepository::new(&conn)
        .upsert(operation)
        .unwrap();
    MutationService::new(&graph)
        .apply(
            MutationRequest::CreateEntities {
                entities: vec![entity("a")],
            },
            MutationContext::local(),
        )
        .unwrap();
    assert_eq!(count(&conn, "event_outbox"), 0);
    let mut type_only = subscription(uuid::Uuid::new_v4());
    type_only.entity_types = vec!["other".into()];
    SubscriptionRepository::new(&conn)
        .upsert(type_only)
        .unwrap();
    MutationService::new(&graph)
        .apply(
            MutationRequest::CreateEntities {
                entities: vec![entity("b")],
            },
            MutationContext::local(),
        )
        .unwrap();
    assert_eq!(count(&conn, "event_outbox"), 0);
}

#[test]
fn resolver_rebinding_mixed_private_answer_is_rejected() {
    assert!(
        webhook_worker::validate_endpoint(
            "https://hooks.example.test",
            &allowlist(),
            &TestResolver(vec![
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            ])
        )
        .is_err()
    );
}

#[test]
fn nonretryable_status_and_attempt_cap_dead_letter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dead.db");
    let graph = graph(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    SubscriptionRepository::new(&conn)
        .upsert(subscription(uuid::Uuid::new_v4()))
        .unwrap();
    graph.create_entities(&[entity("a")]).unwrap();
    let now = memory_core::events::now_us();
    let worker = webhook_worker::WebhookWorker::new(
        &path,
        StatusConnector(400, None),
        TestSecrets,
        allowlist(),
        resolver(),
    );
    assert_eq!(worker.run_once(now).unwrap().dead, 1);
    conn.execute(
        "UPDATE event_outbox SET state='pending', attempts=7, next_attempt_us=0",
        [],
    )
    .unwrap();
    let worker = webhook_worker::WebhookWorker::new(
        &path,
        StatusConnector(503, None),
        TestSecrets,
        allowlist(),
        resolver(),
    );
    assert_eq!(worker.run_once(now + 1).unwrap().dead, 1);
}

#[derive(Default)]
struct RecordingHttpsTransport(Mutex<Option<webhook_worker::ValidatedEndpoint>>);
impl webhook_worker::HttpsTransport for RecordingHttpsTransport {
    fn post(
        &self,
        endpoint: &webhook_worker::ValidatedEndpoint,
        _: webhook_worker::SignedRequest,
    ) -> Result<webhook_worker::DeliveryResponse, webhook_worker::WorkerError> {
        *self.0.lock().unwrap() = Some(endpoint.clone());
        Ok(webhook_worker::DeliveryResponse {
            status: 302,
            retry_after_us: None,
        })
    }
}
#[test]
fn https_connector_hands_pinned_address_to_transport_and_does_not_follow_redirect() {
    use webhook_worker::DeliveryConnector;
    let endpoint = webhook_worker::validate_endpoint(
        "https://hooks.example.test/path",
        &allowlist(),
        &resolver(),
    )
    .unwrap();
    let transport = RecordingHttpsTransport::default();
    let connector = webhook_worker::HttpsConnector(transport);
    let response = connector
        .send(
            &endpoint,
            webhook_worker::SignedRequest {
                body: b"x".to_vec(),
                event_id: uuid::Uuid::new_v4().to_string(),
                timestamp_us: 1,
                signature: "sig".into(),
            },
        )
        .unwrap();
    assert_eq!(response.status, 302);
    let passed = connector.0.0.lock().unwrap().clone().unwrap();
    assert_eq!(passed.address, "8.8.8.8:443".parse().unwrap());
    assert_eq!(passed.url.host_str(), Some("hooks.example.test"));
}

#[test]
fn production_reqwest_plan_pins_dns_disables_redirects_and_sets_headers() {
    let endpoint = webhook_worker::validate_endpoint(
        "https://hooks.example.test/path",
        &allowlist(),
        &resolver(),
    )
    .unwrap();
    let plan = webhook_worker::request_plan(
        &endpoint,
        &webhook_worker::SignedRequest {
            body: vec![],
            event_id: "event-1".into(),
            timestamp_us: 42,
            signature: "signature".into(),
        },
    )
    .unwrap();
    assert_eq!(plan.host, "hooks.example.test");
    assert_eq!(plan.address, "8.8.8.8:443".parse().unwrap());
    assert!(!plan.follow_redirects);
    assert_eq!(plan.idempotency_key, "event-1");
    assert_eq!(plan.timestamp, "42");
    assert_eq!(plan.signature, "signature");
}

#[test]
fn lease_reclaim_fences_stale_completion_for_webhook_delivery() {
    use memory_core::events::EventRepository;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let graph = graph(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let other = rusqlite::Connection::open(&path).unwrap();
    let id = uuid::Uuid::new_v4();
    SubscriptionRepository::new(&conn)
        .upsert(subscription(id))
        .unwrap();
    graph.create_entities(&[entity("a")]).unwrap();
    let first = EventRepository::new(&conn)
        .claim_due(10, 10)
        .unwrap()
        .unwrap();
    let second = EventRepository::new(&other)
        .claim_due(21, 10)
        .unwrap()
        .unwrap();
    assert!(!EventRepository::new(&conn).complete(&first, 22).unwrap());
    assert!(EventRepository::new(&other).complete(&second, 22).unwrap());
}
