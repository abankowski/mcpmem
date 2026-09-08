use std::sync::Mutex;
use std::time::Duration;

use aws_sdk_bedrockruntime::primitives::Blob;
use memory_core::jobs::IndexProfile;

use crate::{CanonicalDocument, EmbeddingProvider, ProviderError};

const TITAN_TEXT_EMBEDDINGS_V2: &str = "amazon.titan-embed-text-v2:";

/// Injectable synchronous boundary around Bedrock's asynchronous SDK. It keeps
/// fixture tests off the network and makes the per-request deadline observable.
pub trait BedrockTransport: Send + Sync {
    fn invoke(
        &self,
        model_id: &str,
        payload: &str,
        timeout: Duration,
    ) -> Result<String, ProviderError>;
}

pub struct BedrockEmbeddingProvider<T = AwsBedrockTransport> {
    transport: T,
    timeout: Duration,
}

impl<T> BedrockEmbeddingProvider<T> {
    pub const fn with_transport(transport: T, timeout: Duration) -> Self {
        Self { transport, timeout }
    }
}

impl BedrockEmbeddingProvider<AwsBedrockTransport> {
    /// Uses only AWS's standard region and credential provider chain.
    pub fn from_standard_chain(timeout: Duration) -> Result<Self, ProviderError> {
        AwsBedrockTransport::from_standard_chain()
            .map(|transport| Self::with_transport(transport, timeout))
    }
}

impl<T: BedrockTransport> EmbeddingProvider for BedrockEmbeddingProvider<T> {
    fn embed(
        &self,
        profile: &IndexProfile,
        documents: &[CanonicalDocument],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        validate_titan_v2(profile)?;
        documents
            .iter()
            .map(|document| {
                let payload = serde_json::json!({
                    "inputText": document.text(),
                    "dimensions": profile.dimensions,
                    "normalize": false,
                });
                let encoded = serde_json::to_string(&payload).map_err(|_| {
                    ProviderError::Request("could not encode Bedrock request".into())
                })?;
                let response = self
                    .transport
                    .invoke(&profile.model, &encoded, self.timeout)?;
                parse_embedding(&response, profile)
            })
            .collect()
    }
}

fn validate_titan_v2(profile: &IndexProfile) -> Result<(), ProviderError> {
    if !profile.model.starts_with(TITAN_TEXT_EMBEDDINGS_V2) {
        return Err(ProviderError::Request(
            "Bedrock V1 supports Titan Text Embeddings V2 models only".into(),
        ));
    }
    if !matches!(profile.dimensions, 256 | 512 | 1024) {
        return Err(ProviderError::Request(
            "Titan Text Embeddings V2 dimensions must be 256, 512, or 1024".into(),
        ));
    }
    Ok(())
}

fn parse_embedding(response: &str, profile: &IndexProfile) -> Result<Vec<f32>, ProviderError> {
    let value: serde_json::Value = serde_json::from_str(response)
        .map_err(|_| ProviderError::Response("Bedrock response is not valid JSON".into()))?;
    let embedding = value
        .get("embedding")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ProviderError::Response("Bedrock response missing embedding".into()))?;
    let vector: Result<Vec<f32>, ProviderError> = embedding
        .iter()
        .map(|value| {
            value.as_f64().map(|number| number as f32).ok_or_else(|| {
                ProviderError::Response("Bedrock embedding contains non-number".into())
            })
        })
        .collect();
    let vector = vector?;
    profile
        .validate_vector(&vector)
        .map_err(|_| ProviderError::Response("Bedrock embedding dimensions are invalid".into()))?;
    Ok(vector)
}

pub struct AwsBedrockTransport {
    client: aws_sdk_bedrockruntime::Client,
    runtime: Mutex<tokio::runtime::Runtime>,
}

impl AwsBedrockTransport {
    pub fn from_standard_chain() -> Result<Self, ProviderError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ProviderError::Request("could not create Bedrock runtime".into()))?;
        let config = runtime.block_on(aws_config::load_defaults(
            aws_config::BehaviorVersion::latest(),
        ));
        Ok(Self {
            client: aws_sdk_bedrockruntime::Client::new(&config),
            runtime: Mutex::new(runtime),
        })
    }
}

impl BedrockTransport for AwsBedrockTransport {
    fn invoke(
        &self,
        model_id: &str,
        payload: &str,
        timeout: Duration,
    ) -> Result<String, ProviderError> {
        let client = self.client.clone();
        let model_id = model_id.to_owned();
        let body = Blob::new(payload.as_bytes());
        let result = self
            .runtime
            .lock()
            .map_err(|_| ProviderError::Request("Bedrock runtime is unavailable".into()))?
            .block_on(async move {
                tokio::time::timeout(
                    timeout,
                    client
                        .invoke_model()
                        .model_id(model_id)
                        .content_type("application/json")
                        .accept("application/json")
                        .body(body)
                        .send(),
                )
                .await
            });
        match result {
            Ok(Ok(response)) => String::from_utf8(response.body.into_inner())
                .map_err(|_| ProviderError::Response("Bedrock response is not UTF-8 JSON".into())),
            Ok(Err(_)) => Err(ProviderError::Request("Bedrock request failed".into())),
            Err(_) => Err(ProviderError::Request("Bedrock request timed out".into())),
        }
    }
}
