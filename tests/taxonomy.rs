//! Server-level tests of the soft taxonomy suggestion hook.
//!
//! The hook composes an additive per-object `taxonomySuggestions` field into
//! create/upsert responses. The field appears only when the authored type was
//! unknown before the write. Each payload carries `similarTypes`; an unknown
//! entity type also gets up to three `exampleEntities` and an unknown
//! relation type up to three `exampleRelations`. The write itself never
//! fails for an unknown type.

use mcpmem::actions::memory::{
    handle_create_entities, handle_create_relations, handle_upsert_entities,
};
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::kg::GraphHandle;
use mcpmem::vector_store::VectorStore;
use serde_json::{Value, json};
use std::num::NonZeroUsize;

/// A graph over a temporary database. `dir` stays alive for the whole test.
fn graph() -> (tempfile::TempDir, GraphHandle) {
    let dir = tempfile::tempdir().unwrap();
    let kg = GraphHandle::new(
        &dir.path().join("memory.db"),
        Durability::Sync,
        SqliteTuning::default(),
        NonZeroUsize::new(32).unwrap(),
        2,
    )
    .unwrap();
    (dir, kg)
}

/// The inner JSON of a tool response: the parsed `content[0].text`.
fn body(response: Value) -> Value {
    serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn unknown_entity_type_gets_suggestions_known_type_gets_none() {
    let (_dir, graph) = graph();
    // Seed: the type "person" is now established.
    handle_create_entities(
        &graph,
        None,
        Some(&json!({"entities":[{"name":"seed","entityType":"person","observations":[]}]})),
    )
    .unwrap();

    // A misspelled new type must get "person" suggested, and the write succeeds.
    let response = handle_create_entities(
        &graph,
        None,
        Some(&json!({"entities":[{"name":"Ada","entityType":"persn","observations":[]}]})),
    )
    .unwrap();
    let entities = body(response);
    assert_eq!(entities[0]["name"], "Ada");
    let suggestions = &entities[0]["taxonomySuggestions"];
    assert!(suggestions.is_object(), "suggestions must be an object");
    let similar = &suggestions["similarTypes"];
    assert!(similar.is_array(), "similarTypes must be an array");
    assert_eq!(similar[0]["name"], "person");
    assert!(similar[0]["score"].as_f64().unwrap() > 0.0);

    // The authored type row exists after the write, so a post-commit raw
    // existence check could never flag it as unknown. The hook must judge the
    // type against the pre-write state.
    assert!(graph.entity_type_exists("persn"));

    // A call with a known type must not carry the key.
    let response = handle_create_entities(
        &graph,
        None,
        Some(&json!({"entities":[{"name":"Bob","entityType":"person","observations":[]}]})),
    )
    .unwrap();
    let entities = body(response);
    assert_eq!(entities[0]["name"], "Bob");
    assert!(entities[0].get("taxonomySuggestions").is_none());
}

#[test]
fn unknown_relation_type_is_created_without_error_and_gets_suggestions() {
    let (_dir, graph) = graph();
    handle_create_entities(
        &graph,
        None,
        Some(&json!({"entities":[
            {"name":"a","entityType":"person","observations":[]},
            {"name":"b","entityType":"person","observations":[]},
            {"name":"c","entityType":"person","observations":[]}
        ]})),
    )
    .unwrap();

    // The first relation type is unknown; the relation must still be created.
    let response = handle_create_relations(
        &graph,
        None,
        Some(&json!({"relations":[{"from":"a","to":"b","relationType":"knows"}]})),
    )
    .unwrap();
    let relations = body(response);
    assert_eq!(relations[0]["from"], "a");
    assert_eq!(relations[0]["to"], "b");
    assert_eq!(relations[0]["relationType"], "knows");
    assert!(relations[0].get("isError").is_none());
    assert!(relations[0]["taxonomySuggestions"].is_object());

    // A misspelled relation type gets the established type suggested, plus
    // the one example relation of that type.
    let response = handle_create_relations(
        &graph,
        None,
        Some(&json!({"relations":[{"from":"b","to":"a","relationType":"know_"}]})),
    )
    .unwrap();
    let relations = body(response);
    assert!(relations[0].get("isError").is_none());
    let payload = &relations[0]["taxonomySuggestions"];
    assert_eq!(payload["similarTypes"][0]["name"], "knows");
    let examples = payload["exampleRelations"].as_array().unwrap();
    assert_eq!(examples.len(), 1);
    assert_eq!(examples[0]["relationType"], "knows");
    assert!(
        payload.get("exampleEntities").is_none(),
        "a relation-type payload carries no entity examples"
    );

    // A known relation type on a fresh pair must not carry the key.
    let response = handle_create_relations(
        &graph,
        None,
        Some(&json!({"relations":[{"from":"a","to":"c","relationType":"knows"}]})),
    )
    .unwrap();
    let relations = body(response);
    assert_eq!(relations[0]["from"], "a");
    assert_eq!(relations[0]["to"], "c");
    assert!(relations[0].get("isError").is_none());
    assert!(relations[0].get("taxonomySuggestions").is_none());
}

#[test]
fn upsert_suggests_for_a_new_type_and_stays_quiet_for_a_known_one() {
    let (_dir, graph) = graph();
    handle_create_entities(
        &graph,
        None,
        Some(&json!({"entities":[{"name":"seed","entityType":"person","observations":[]}]})),
    )
    .unwrap();

    // A new entity with a misspelled type gets suggestions.
    let response = handle_upsert_entities(
        &graph,
        None,
        Some(&json!({"entities":[{"name":"Ada","entityType":"persn","observations":[]}]})),
    )
    .unwrap();
    let upserted = body(response);
    assert_eq!(upserted["results"][0]["name"], "Ada");
    assert_eq!(
        upserted["results"][0]["taxonomySuggestions"]["similarTypes"][0]["name"],
        "person"
    );

    // An existing entity with a known type must not carry the key.
    let response = handle_upsert_entities(
        &graph,
        None,
        Some(&json!({"entities":[{"name":"seed","entityType":"person","observations":[]}]})),
    )
    .unwrap();
    let upserted = body(response);
    assert_eq!(upserted["results"][0]["name"], "seed");
    assert!(upserted["results"][0].get("taxonomySuggestions").is_none());
}

/// Semantic-tier tests: a fake embeddings endpoint on a loopback port plays
/// the role of the configured provider, exactly as the `src/taxonomy.rs`
/// unit tests do. The store is seeded with a serving profile and per-kind
/// snapshot vectors; the fake server returns all-ones embeddings, so a
/// seeded all-ones vector comes back at distance 0 with score 1.0.
#[cfg(feature = "indexer")]
mod semantic {
    use super::*;
    use mcpmem_core::jobs::{DistanceMetric, IndexProfile, IndexProfileRegistry, Normalization};
    use mcpmem_indexer::OpenAiCompatibleProvider;
    use parking_lot::Mutex;
    use rusqlite::params;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::time::Duration;

    /// One `/v1/embeddings` request the fake server recorded.
    ///
    /// The recorded batch is the input array exactly as the provider sent
    /// it, so an assertion on it proves how many texts one call carried.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedCall {
        texts: Vec<String>,
    }

    /// The shared state of the fake embeddings server.
    struct FakeState {
        calls: Mutex<Vec<RecordedCall>>,
    }

    /// A fake OpenAI-compatible embeddings endpoint on a loopback port.
    ///
    /// [`mcpmem::indexer_provider`] holds one [`ProviderRegistry`], and a
    /// registry can hold only the concrete Ollama and OpenAI providers,
    /// both plain HTTP clients. A trait-level stub can never reach the
    /// semantic tier, so the stub must speak HTTP itself. The server
    /// returns all-ones vectors of the requested dimension and records each
    /// request. One server lives for the whole test binary; every semantic
    /// test shares it through the process-wide registry.
    struct FakeEmbeddings {
        url: String,
        state: Arc<FakeState>,
    }

    /// One loopback embeddings server for the whole test binary.
    static FAKE: std::sync::LazyLock<Arc<FakeEmbeddings>> =
        std::sync::LazyLock::new(|| Arc::new(FakeEmbeddings::start()));

    fn fake_embeddings() -> &'static FakeEmbeddings {
        FAKE.as_ref()
    }

    impl FakeEmbeddings {
        fn start() -> Self {
            let listener =
                TcpListener::bind("127.0.0.1:0").expect("bind the fake embeddings listener");
            let addr = listener
                .local_addr()
                .expect("read back the fake embeddings port");
            let state = Arc::new(FakeState {
                calls: Mutex::new(Vec::new()),
            });
            let thread_state = Arc::clone(&state);
            let _ = std::thread::Builder::new()
                .name("taxonomy-hook-fake".into())
                .spawn(move || {
                    loop {
                        // A dying client is an accept or read failure,
                        // never a server fault; keep serving.
                        let Some((conn, _peer)) = listener.accept().ok() else {
                            continue;
                        };
                        handle_connection(conn, Arc::clone(&thread_state));
                    }
                });
            Self {
                url: format!("http://{addr}/v1/embeddings"),
                state,
            }
        }

        /// Every request the server has seen so far, in arrival order.
        fn recorded(&self) -> Vec<RecordedCall> {
            (self.state.calls.lock()).clone()
        }
    }

    /// Installs the fake server into the process-wide registry cell.
    ///
    /// The cell accepts one value for the lifetime of the process, and
    /// every test registers the same fake server, so whichever call wins
    /// the race the registry is equivalent.
    fn install_fake_provider() {
        let fake = fake_embeddings();
        let provider = OpenAiCompatibleProvider::new(
            fake.url.clone(),
            "test-key".into(),
            Duration::from_secs(5),
        )
        .expect("make an OpenAI provider without a request");
        mcpmem::indexer_provider::init(Arc::new(mcpmem_indexer::ProviderRegistry::new(
            None,
            Some(Arc::new(provider)),
        )));
    }

    /// Serves one connection: read the whole request, record it, answer
    /// with fixed embeddings, then close.
    fn handle_connection(mut conn: TcpStream, state: Arc<FakeState>) {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 8192];
        loop {
            let read = conn.read(&mut chunk);
            if read.is_err() {
                return;
            }
            let n = read.unwrap();
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some((headers_end, body_len)) = request_frame(&buf) {
                // The body follows the blank-line separator.
                let body_start = headers_end + 4;
                if buf.len() >= body_start + body_len {
                    respond(
                        &mut conn,
                        &buf[body_start..body_start + body_len],
                        Arc::clone(&state),
                    );
                    return;
                }
            }
        }
    }

    /// The header-block end and the `Content-Length` of a request.
    ///
    /// Returns None until the full header block is buffered.
    fn request_frame(buf: &[u8]) -> Option<(usize, usize)> {
        let text = String::from_utf8_lossy(buf).to_string();
        let Some(headers_end) = text.find("\r\n\r\n") else {
            return None;
        };
        let mut length: usize = 0;
        for line in text[..headers_end].lines() {
            let Some(colon) = line.find(':') else {
                continue;
            };
            if line[..colon].trim().eq_ignore_ascii_case("content-length") {
                length = line[colon + 1..].trim().parse::<usize>().ok().unwrap_or(0);
            }
        }
        Some((headers_end, length))
    }

    /// Records the request and answers with all-ones embeddings.
    ///
    /// A batch whose first text contains `FAIL_ME` gets HTTP 500 instead, the
    /// provider fault that the hook must swallow. Each request produces the
    /// same answer for all its texts.
    fn respond(conn: &mut TcpStream, body: &[u8], state: Arc<FakeState>) {
        let text = String::from_utf8_lossy(body).to_string();
        let Some(value) = serde_json::from_str::<serde_json::Value>(text.as_str()).ok() else {
            return;
        };
        let input: Vec<String> = value["input"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| row.as_str().expect("an input text").to_owned())
                    .collect()
            })
            .unwrap_or(Vec::new());
        let dimensions: usize = value["dimensions"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(0);
        state.calls.lock().extend_from_slice(&[RecordedCall {
            texts: input.clone(),
        }]);

        if input.first().is_some_and(|text| text.contains("FAIL_ME")) {
            let payload = json!({"error": "the fake provider is broken"}).to_string();
            let head = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(payload.as_bytes());
            return;
        }

        let data: Vec<_> = input
            .iter()
            .map(|_| json!({"embedding": vec![1.0; dimensions]}))
            .collect();
        let payload = json!({"data": data}).to_string();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        let _ = conn.write_all(head.as_bytes());
        let _ = conn.write_all(payload.as_bytes());
    }

    /// A graph and a vector store over the same temporary database, with
    /// the fake provider installed. `dir` stays alive for the whole test.
    struct Env {
        _dir: tempfile::TempDir,
        kg: GraphHandle,
        vs: VectorStore,
    }

    fn env(dims: u32) -> Env {
        install_fake_provider();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.db");
        let kg = GraphHandle::new(
            &db_path,
            Durability::Sync,
            SqliteTuning::default(),
            NonZeroUsize::new(32).unwrap(),
            2,
        )
        .unwrap();
        let vs = VectorStore::new(&db_path, dims).unwrap();
        Env { _dir: dir, kg, vs }
    }

    /// Registers a serving profile for the store and returns its id.
    ///
    /// The registry rows are inserted directly, the way a completed
    /// rebuild would leave them; the entity index is not rebuilt, because
    /// only the taxonomy snapshots matter here.
    fn seed_profile(env: &Env, dims: u32) -> uuid::Uuid {
        let profile = IndexProfile {
            id: uuid::Uuid::new_v4(),
            store_key: "default".into(),
            provider_kind: "openai".into(),
            model: "test-model".into(),
            dimensions: dims,
            representation_version: "v1".into(),
            normalization: Normalization::None,
            distance_metric: DistanceMetric::L2Squared,
            vector_encoding_version: "f32le-v1".into(),
        };
        let conn = rusqlite::Connection::open(&env.vs.db_path).unwrap();
        conn.execute(
            "INSERT INTO index_profile VALUES(?1,'default',?2,?3,'Active')",
            params![
                profile.id.to_string(),
                "taxonomy-hook-fixture",
                serde_json::to_string(&profile).unwrap()
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE index_profile_registry SET state='Active',serving_profile=?1 WHERE store_key='default'",
            [profile.id.to_string()],
        )
        .unwrap();
        profile.id
    }

    /// The type_dict id of a live type row, so seeding never creates a
    /// duplicate type row behind the graph's back.
    fn type_id(env: &Env, kind: i64, name: &str) -> i64 {
        let conn = rusqlite::Connection::open(&env.vs.db_path).unwrap();
        conn.query_row(
            "SELECT id FROM type_dict WHERE kind=?1 AND name=?2",
            params![kind, name],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn seed_generation(env: &Env, profile: uuid::Uuid, kind: i64, durable: i64) {
        let conn = rusqlite::Connection::open(&env.vs.db_path).unwrap();
        conn.execute(
            "INSERT INTO taxonomy_ann_generation(profile_id,subject_kind,durable_generation) VALUES(?1,?2,?3)",
            params![profile.to_string(), kind, durable],
        )
        .unwrap();
    }

    fn seed_vector(env: &Env, profile: uuid::Uuid, kind: i64, id: i64, embedding: &[f32]) {
        let bytes: Vec<u8> = embedding
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let conn = rusqlite::Connection::open(&env.vs.db_path).unwrap();
        conn.execute(
            "INSERT INTO taxonomy_vector VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(profile_id,subject_kind,subject_id) DO UPDATE SET subject_revision=excluded.subject_revision,blob=excluded.blob,created_at_us=excluded.created_at_us,source=excluded.source",
            params![profile.to_string(), kind, id, 1i64, bytes, 1i64, "test"],
        )
        .unwrap();
    }

    /// Builds the per-kind snapshots for the seeded profile, the way the
    /// indexer worker does after a committed batch.
    fn adopt(env: &mut Env) {
        let conn = rusqlite::Connection::open(&env.vs.db_path).unwrap();
        let registry = IndexProfileRegistry::new(&conn);
        env.vs.adopt_taxonomy(&registry, false).unwrap();
    }

    /// Seeds the entity-type snapshot with one all-ones vector for the live
    /// `person` type, so every embedded text hits it at distance 0.
    fn seed_person_snapshot(env: &mut Env, profile: uuid::Uuid) {
        let person_id = type_id(env, 0, "person");
        seed_generation(env, profile, 0, 1);
        seed_vector(env, profile, 0, person_id, &[1.0; 4]);
        adopt(env);
    }

    #[test]
    fn unknown_type_returns_semantic_results_first_then_offline_fill() {
        let mut env = env(4);
        // Both "person" (semantic + offline hit) and "persons" (offline-only
        // hit: no snapshot vector) are real type rows with count > 0.
        handle_create_entities(
            &env.kg,
            None,
            Some(&json!({"entities":[
                {"name":"person one","entityType":"person","observations":[]},
                {"name":"persons alpha","entityType":"persons","observations":[]}
            ]})),
        )
        .unwrap();
        let profile = seed_profile(&env, 4);
        seed_person_snapshot(&mut env, profile);

        let response = handle_create_entities(
            &env.kg,
            Some(&env.vs),
            Some(&json!({"entities":[{"name":"Ada","entityType":"persn","observations":[]}]})),
        )
        .unwrap();
        let entities = body(response);
        let similar = entities[0]["taxonomySuggestions"]["similarTypes"]
            .as_array()
            .unwrap();
        assert_eq!(
            similar[0]["name"], "person",
            "the semantic hit must sort first"
        );
        assert_eq!(similar[0]["score"], 1.0);
        assert!(
            similar.iter().any(|s| s["name"] == "persons"),
            "the offline tier must fill the remaining slots: {similar:?}"
        );
    }

    #[test]
    fn without_a_serving_profile_runs_the_offline_tier_only() {
        let env = env(4);
        handle_create_entities(
            &env.kg,
            None,
            Some(&json!({"entities":[{"name":"person one","entityType":"person","observations":[]}]})),
        )
        .unwrap();
        // No profile row: the store serves nothing, so the hook must use the
        // offline tier, attach the payload, and never call the provider.
        let calls_before = fake_embeddings().recorded().len();
        let response = handle_create_entities(
            &env.kg,
            Some(&env.vs),
            Some(&json!({"entities":[{"name":"Ada","entityType":"persn","observations":[]}]})),
        )
        .unwrap();
        let entities = body(response);
        assert_eq!(entities[0]["name"], "Ada");
        let similar = entities[0]["taxonomySuggestions"]["similarTypes"]
            .as_array()
            .unwrap();
        assert_eq!(similar[0]["name"], "person");
        assert_eq!(
            fake_embeddings().recorded().len(),
            calls_before,
            "no provider call may happen without a serving profile"
        );
    }

    #[test]
    fn example_entities_come_from_the_top_similar_types() {
        let mut env = env(4);
        handle_create_entities(
            &env.kg,
            None,
            Some(&json!({"entities":[
                {"name":"person one","entityType":"person","observations":[]},
                {"name":"person two","entityType":"person","observations":[]},
                {"name":"person three","entityType":"person","observations":[]}
            ]})),
        )
        .unwrap();
        let profile = seed_profile(&env, 4);
        seed_person_snapshot(&mut env, profile);

        let response = handle_create_entities(
            &env.kg,
            Some(&env.vs),
            Some(&json!({"entities":[{"name":"Ada","entityType":"persn","observations":[]}]})),
        )
        .unwrap();
        let payload = &body(response)[0]["taxonomySuggestions"];
        let examples = payload["exampleEntities"].as_array().unwrap();
        assert_eq!(examples.len(), 3);
        for example in examples {
            assert_eq!(example["entityType"], "person");
            assert!(example["name"].as_str().unwrap().starts_with("person "));
        }
        assert!(
            payload.get("exampleRelations").is_none(),
            "an entity-type payload carries no relation examples"
        );
    }

    #[test]
    fn example_relations_come_from_the_top_similar_types() {
        let mut env = env(4);
        handle_create_entities(
            &env.kg,
            None,
            Some(&json!({"entities":[
                {"name":"a","entityType":"person","observations":[]},
                {"name":"b","entityType":"person","observations":[]},
                {"name":"c","entityType":"person","observations":[]}
            ]})),
        )
        .unwrap();
        handle_create_relations(
            &env.kg,
            None,
            Some(&json!({"relations":[
                {"from":"a","to":"b","relationType":"knows"},
                {"from":"b","to":"c","relationType":"knows"},
                {"from":"c","to":"a","relationType":"knows"}
            ]})),
        )
        .unwrap();
        let profile = seed_profile(&env, 4);
        let knows_id = type_id(&env, 1, "knows");
        seed_generation(&env, profile, 1, 1);
        seed_vector(&env, profile, 1, knows_id, &[1.0; 4]);
        adopt(&mut env);

        let response = handle_create_relations(
            &env.kg,
            Some(&env.vs),
            Some(&json!({"relations":[{"from":"a","to":"c","relationType":"know_"}]})),
        )
        .unwrap();
        let relations = body(response);
        assert!(relations[0].get("isError").is_none());
        let payload = &relations[0]["taxonomySuggestions"];
        let similar = payload["similarTypes"].as_array().unwrap();
        assert_eq!(
            similar[0]["name"], "knows",
            "the semantic hit must sort first"
        );
        assert_eq!(similar[0]["score"], 1.0);
        let examples = payload["exampleRelations"].as_array().unwrap();
        assert_eq!(examples.len(), 3);
        for example in examples {
            assert_eq!(example["relationType"], "knows");
        }
        assert!(
            payload.get("exampleEntities").is_none(),
            "a relation-type payload carries no entity examples"
        );
    }

    #[test]
    fn a_failing_provider_falls_back_without_an_error() {
        let mut env = env(4);
        handle_create_entities(
            &env.kg,
            None,
            Some(&json!({"entities":[{"name":"person one","entityType":"person","observations":[]}]})),
        )
        .unwrap();
        let profile = seed_profile(&env, 4);
        seed_person_snapshot(&mut env, profile);

        // The batch's first text is the fake's documented `FAIL_ME` sentinel
        // (respond() answers HTTP 500 for it), so the embedded batch fails
        // and the semantic tier errors. The write must succeed anyway and
        // the offline tier must carry the suggestions (advisory only).
        let response = handle_create_entities(
            &env.kg,
            Some(&env.vs),
            Some(&json!({"entities":[
                {"name":"Ada","entityType":"persn","observations":[]},
                {"name":"Bomb","entityType":"FAIL_ME","observations":[]}
            ]})),
        )
        .unwrap();
        let entities = body(response);
        let entities = entities.as_array().unwrap();
        assert_eq!(entities[0]["name"], "Ada");
        assert_eq!(entities[1]["name"], "Bomb");
        let similar = entities[0]["taxonomySuggestions"]["similarTypes"]
            .as_array()
            .unwrap();
        assert!(
            similar.iter().any(|s| s["name"] == "person"),
            "the offline tier must survive a semantic failure: {similar:?}"
        );
        assert!(env.kg.entity_type_exists("persn"));
        assert!(env.kg.entity_type_exists("FAIL_ME"));
        // The failing batch really was sent: the call is recorded before the
        // endpoint answers 500, so its presence proves the failure path ran.
        let calls = fake_embeddings().recorded();
        assert!(
            calls
                .iter()
                .any(|call| call.texts == vec!["FAIL_ME".to_string(), "persn".to_string()]),
            "the FAIL_ME batch must have reached the provider: {calls:?}"
        );
    }

    #[test]
    fn one_provider_call_carries_the_whole_unknown_type_batch() {
        let mut env = env(4);
        handle_create_entities(
            &env.kg,
            None,
            Some(&json!({"entities":[{"name":"person one","entityType":"person","observations":[]}]})),
        )
        .unwrap();
        let profile = seed_profile(&env, 4);
        seed_person_snapshot(&mut env, profile);
        let calls_before = fake_embeddings().recorded().len();

        // Two unknown types in one write must ride one provider call, and
        // the batch must be the sorted unknown type names.
        let response = handle_create_entities(
            &env.kg,
            Some(&env.vs),
            Some(&json!({"entities":[
                {"name":"Ada","entityType":"persn","observations":[]},
                {"name":"Bob","entityType":"know-ish","observations":[]}
            ]})),
        )
        .unwrap();
        let entities = body(response);
        let entities = entities.as_array().unwrap();
        assert_eq!(entities.len(), 2);
        for entity in entities {
            assert!(
                entity["taxonomySuggestions"]["similarTypes"].is_array(),
                "every unknown authored type must carry suggestions: {entity:?}"
            );
        }
        let calls = fake_embeddings().recorded();
        let new_calls = &calls[calls_before..];
        assert_eq!(new_calls.len(), 1, "one provider call per write: {calls:?}");
        assert_eq!(
            new_calls[0].texts,
            vec!["know-ish".to_string(), "persn".to_string()],
            "the batch is the sorted unknown types"
        );
    }
}
