#![cfg(feature = "bedrock")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mcpmem_core::jobs::{DistanceMetric, IndexProfile, Normalization};
use mcpmem_indexer::{
    BedrockEmbeddingProvider, BedrockTransport, CanonicalDocument, EmbeddingProvider, ProviderError,
};
use uuid::Uuid;

#[derive(Clone)]
struct FixtureTransport {
    response: String,
    delay: Duration,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    invocation_threads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
}

impl FixtureTransport {
    fn response(body: impl Into<String>) -> Self {
        Self {
            response: body.into(),
            delay: Duration::ZERO,
            requests: Arc::new(Mutex::new(Vec::new())),
            invocation_threads: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl BedrockTransport for FixtureTransport {
    fn invoke(
        &self,
        model_id: &str,
        payload: &str,
        timeout: Duration,
    ) -> Result<String, ProviderError> {
        self.invocation_threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        if self.delay > timeout {
            return Err(ProviderError::Request("Bedrock request timed out".into()));
        }
        self.requests
            .lock()
            .unwrap()
            .push((model_id.into(), payload.into()));
        Ok(self.response.clone())
    }
}

fn profile(dimensions: u32) -> IndexProfile {
    IndexProfile {
        id: Uuid::new_v4(),
        store_key: "default".into(),
        provider_kind: "bedrock".into(),
        model: "amazon.titan-embed-text-v2:0".into(),
        dimensions,
        representation_version: "v1".into(),
        normalization: Normalization::None,
        distance_metric: DistanceMetric::Cosine,
        vector_encoding_version: "f32le-v1".into(),
    }
}

fn document() -> CanonicalDocument {
    CanonicalDocument {
        entity_id: 7,
        revision: 3,
        name: "Ada".into(),
        entity_type: "Person".into(),
        observations: vec!["first programmer".into()],
    }
}

#[test]
fn titan_payload_pins_model_input_and_dimensions_without_credentials() {
    let transport =
        FixtureTransport::response(serde_json::json!({"embedding": vec![1.0; 256]}).to_string());
    let requests = Arc::clone(&transport.requests);
    let provider = BedrockEmbeddingProvider::with_transport(transport, Duration::from_millis(10));

    assert_eq!(
        provider.embed(&profile(256), &[document()]).unwrap(),
        vec![vec![1.0; 256]]
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "amazon.titan-embed-text-v2:0");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&requests[0].1).unwrap(),
        serde_json::json!({"inputText":"Ada\nPerson\nfirst programmer","dimensions":256,"normalize":false})
    );
}

#[test]
fn provider_rejects_non_titan_model_before_transport() {
    let transport =
        FixtureTransport::response(serde_json::json!({"embedding": vec![1.0; 256]}).to_string());
    let requests = Arc::clone(&transport.requests);
    let provider = BedrockEmbeddingProvider::with_transport(transport, Duration::from_millis(10));
    let mut unsupported = profile(256);
    unsupported.model = "cohere.embed-english-v3".into();

    let error = provider.embed(&unsupported, &[document()]).unwrap_err();
    assert!(error.to_string().contains("Titan Text Embeddings V2"));
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn transport_timeout_is_bounded_and_redacts_transport_detail() {
    let mut transport =
        FixtureTransport::response(serde_json::json!({"embedding": vec![1.0; 256]}).to_string());
    transport.delay = Duration::from_millis(20);
    let invocation_threads = Arc::clone(&transport.invocation_threads);
    let provider = BedrockEmbeddingProvider::with_transport(transport, Duration::from_millis(1));
    let caller_thread = std::thread::current().id();
    let started = std::time::Instant::now();
    for _ in 0..32 {
        let error = provider.embed(&profile(256), &[document()]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider request failed: Bedrock request timed out"
        );
    }
    assert!(started.elapsed() < Duration::from_millis(10));
    let invocation_threads = invocation_threads.lock().unwrap();
    assert_eq!(invocation_threads.len(), 32);
    assert!(invocation_threads.iter().all(|id| *id == caller_thread));
}

#[test]
fn malformed_missing_non_numeric_and_wrong_dimension_responses_fail_closed() {
    for body in [
        "not-json",
        r#"{}"#,
        r#"{"embedding":[1.0,"not-a-number"]}"#,
        r#"{"embedding":[1.0]}"#,
    ] {
        let provider = BedrockEmbeddingProvider::with_transport(
            FixtureTransport::response(body),
            Duration::from_millis(10),
        );
        assert!(matches!(
            provider.embed(&profile(256), &[document()]),
            Err(ProviderError::Response(_))
        ));
    }
}

#[test]
fn provider_rejects_an_unsupported_titan_dimension_before_transport() {
    let transport = FixtureTransport::response(r#"{"embedding":[1.0]}"#);
    let requests = Arc::clone(&transport.requests);
    let provider = BedrockEmbeddingProvider::with_transport(transport, Duration::from_millis(10));
    let error = provider.embed(&profile(2), &[document()]).unwrap_err();
    assert!(error.to_string().contains("256, 512, or 1024"));
    assert!(requests.lock().unwrap().is_empty());
}
