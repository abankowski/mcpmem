use crate::{CanonicalDocument, EmbeddingProvider, ProviderError};
use mcpmem_core::jobs::IndexProfile;
use std::time::Duration;
use url::Url;

pub struct OllamaProvider {
    endpoint: Url,
    client: reqwest::blocking::Client,
}
impl OllamaProvider {
    pub fn new(endpoint: &str, timeout: Duration) -> Result<Self, ProviderError> {
        let endpoint = Url::parse(endpoint).map_err(|e| ProviderError::Request(e.to_string()))?;
        if !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(ProviderError::Request(
                "Ollama URL must not contain credentials".into(),
            ));
        }
        Ok(Self {
            endpoint,
            client: reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| ProviderError::Request(e.to_string()))?,
        })
    }
}
impl EmbeddingProvider for OllamaProvider {
    fn embed(
        &self,
        profile: &IndexProfile,
        documents: &[CanonicalDocument],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        let url = self
            .endpoint
            .join("api/embed")
            .map_err(|e| ProviderError::Request(e.to_string()))?;
        let response: serde_json::Value = self.client.post(url).json(&serde_json::json!({"model": profile.model, "input": documents.iter().map(CanonicalDocument::text).collect::<Vec<_>>() })).send().map_err(|e| ProviderError::Request(e.to_string()))?.error_for_status().map_err(|e| ProviderError::Request(e.to_string()))?.json().map_err(|e| ProviderError::Response(e.to_string()))?;
        response
            .get("embeddings")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| ProviderError::Response("missing embeddings".into()))?
            .iter()
            .map(|embedding| {
                embedding
                    .as_array()
                    .ok_or_else(|| ProviderError::Response("embedding is not an array".into()))
                    .and_then(|values| {
                        values
                            .iter()
                            .map(|value| {
                                value.as_f64().map(|x| x as f32).ok_or_else(|| {
                                    ProviderError::Response("embedding contains non-number".into())
                                })
                            })
                            .collect()
                    })
            })
            .collect()
    }
}
