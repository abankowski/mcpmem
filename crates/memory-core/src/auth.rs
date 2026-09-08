//! Typed machine-ingress policy. HTTP parsing remains an adapter concern.
use crate::errors::{MCSError, Result};
use crate::events::request_fingerprint;
use crate::mutation::MutationContext;
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrincipalKind {
    Machine,
    Human,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    pub id: String,
    pub kind: PrincipalKind,
    pub scopes: BTreeSet<String>,
    pub allowed_origins: BTreeSet<String>,
}
pub trait CredentialAuthenticator: Send + Sync {
    fn authenticate(&self, credential: &str) -> Result<Principal>;
}

pub fn machine_mutation_context(
    principal: &Principal,
    origin: &str,
    idempotency_key: String,
    correlation_id: Uuid,
    causation_id: Option<Uuid>,
    hop_count: u8,
    raw_body: &[u8],
) -> Result<(MutationContext, String)> {
    if principal.id.trim().is_empty()
        || principal.id.len() > 256
        || principal.id.chars().any(char::is_control)
        || principal.kind != PrincipalKind::Machine
        || !principal.scopes.contains("memory:write")
        || !principal.allowed_origins.contains(origin)
    {
        return Err(MCSError::InvalidParams(
            "machine principal is not authorized for this origin".into(),
        ));
    }
    let context = MutationContext {
        actor: principal.id.clone(),
        origin: origin.to_owned(),
        correlation_id,
        causation_id,
        hop_count,
        idempotency_key: Some(idempotency_key),
    }
    .validate()?;
    Ok((
        context,
        request_fingerprint("POST", "/api/v1/mutations", raw_body),
    ))
}
