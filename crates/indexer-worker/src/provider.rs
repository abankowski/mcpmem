use memory_core::jobs::IndexProfile;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalDocument {
    pub entity_id: i64,
    pub revision: i64,
    pub name: String,
    pub entity_type: String,
    pub observations: Vec<String>,
}

impl CanonicalDocument {
    pub fn text(&self) -> String {
        std::iter::once(self.name.as_str())
            .chain(std::iter::once(self.entity_type.as_str()))
            .chain(self.observations.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider request failed: {0}")]
    Request(String),
    #[error("provider response malformed: {0}")]
    Response(String),
}

pub trait EmbeddingProvider: Send + Sync {
    fn embed(
        &self,
        profile: &IndexProfile,
        documents: &[CanonicalDocument],
    ) -> Result<Vec<Vec<f32>>, ProviderError>;
}

impl<T: EmbeddingProvider + ?Sized> EmbeddingProvider for std::sync::Arc<T> {
    fn embed(
        &self,
        profile: &IndexProfile,
        documents: &[CanonicalDocument],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        (**self).embed(profile, documents)
    }
}
