use crate::{CanonicalDocument, EmbeddingProvider, ProviderError};
use mcpmem_core::jobs::IndexProfile;
use std::time::Duration;

pub struct OpenAiCompatibleProvider {
    endpoint: String,
    api_key: String,
    client: reqwest::blocking::Client,
}
impl OpenAiCompatibleProvider {
    pub fn new(
        endpoint: String,
        api_key: String,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            endpoint,
            api_key,
            client: reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| ProviderError::Request(e.to_string()))?,
        })
    }
}
impl EmbeddingProvider for OpenAiCompatibleProvider {
    fn embed(
        &self,
        profile: &IndexProfile,
        documents: &[CanonicalDocument],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        let response: serde_json::Value = self.client.post(&self.endpoint).bearer_auth(&self.api_key).json(&serde_json::json!({"model": profile.model, "input": documents.iter().map(CanonicalDocument::text).collect::<Vec<_>>() })).send().map_err(|e| ProviderError::Request(e.to_string()))?.error_for_status().map_err(|e| ProviderError::Request(e.to_string()))?.json().map_err(|e| ProviderError::Response(e.to_string()))?;
        response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| ProviderError::Response("missing data".into()))?
            .iter()
            .map(|item| {
                item.get("embedding")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| ProviderError::Response("missing embedding".into()))
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
