use mcpmem_core::jobs::IndexProfile;
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

/// Every provider reduces a document to one string before the request, so
/// **text is the primitive** and `embed` is the convenience over it.
///
/// A query has no entity and no revision. Building a `CanonicalDocument` for
/// one would put an invented `entity_id` and `revision` into the struct whose
/// whole purpose is revision fencing, so a read path calls `embed_texts`.
pub trait EmbeddingProvider: Send + Sync {
    fn embed_texts(
        &self,
        profile: &IndexProfile,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, ProviderError>;

    fn embed(
        &self,
        profile: &IndexProfile,
        documents: &[CanonicalDocument],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        let texts: Vec<String> = documents.iter().map(CanonicalDocument::text).collect();
        self.embed_texts(profile, &texts)
    }
}

impl<T: EmbeddingProvider + ?Sized> EmbeddingProvider for std::sync::Arc<T> {
    fn embed_texts(
        &self,
        profile: &IndexProfile,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        (**self).embed_texts(profile, texts)
    }

    fn embed(
        &self,
        profile: &IndexProfile,
        documents: &[CanonicalDocument],
    ) -> Result<Vec<Vec<f32>>, ProviderError> {
        (**self).embed(profile, documents)
    }
}
