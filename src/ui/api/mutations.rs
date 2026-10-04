//! The browser mutation gateway: `POST /ui/api/mutations`.
//!
//! One adapter serves every graph write the UI offers. The request body is
//! `{workspaceId, operation, payload}`; the operation and the payload form
//! the tagged union the design doc's contract table pins. The adapter
//! validates the payload, checks the `graph-write` scope and the write
//! grant, and dispatches to the shared `MutationService`. The adapter calls
//! the graph services directly; it never invokes its own `/mcp` endpoint.
//!
//! The gates run in a fixed order and the HTTP answers follow the design
//! contract: 401 for an invalid token, 403 for a missing scope or a
//! read-only grant, 404 for an unknown or denied workspace (same body for
//! both), 409 for a name or relation conflict, 400 for an unknown operation
//! or a bad payload. Every error body is `{code,message}`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use mcpmem_core::mutation::{MutationContext, MutationRequest, MutationService, ObservationUpdate};
use mcpmem_core::types::{
    AttributeDelete, AttributeSet, EntityInput, ObservationInput, Relation, RelationInput,
    RelationObservationUpdate,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::error;
use uuid::Uuid;

use crate::errors::MCSError;
use crate::http::{HttpState, principal_of_ui};
use crate::kg::GraphHandle;
use crate::workspace::{WorkspaceAccess, WorkspaceError};

/// The tool the gateway stands for. The scope decision is
/// [`crate::authz::allows_tool`]'s, asked about one write tool by name so
/// the two stay in step: a write tool moved to another scope moves the
/// gateway with it.
const WRITE_TOOL: &str = "create_entities";

/// Register the mutation route. `/ui/api/mutations` is the only write route
/// the UI module owns; every other adapter is read-only or admin-gated.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    router.route("/ui/api/mutations", post(ui_mutations_handler))
}

/// The operations of the spec contract table, parsed from their camelCase
/// wire names.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum Operation {
    CreateEntity,
    RenameEntity,
    MergeEntities,
    DeleteEntity,
    CreateRelation,
    DeleteRelation,
    ReverseRelation,
    ChangeRelationType,
    SetEntityAttributes,
    DeleteEntityAttributes,
    SetRelationAttributes,
    DeleteRelationAttributes,
    AddObservation,
    DeleteObservation,
    EditObservation,
    AddRelationObservation,
    DeleteRelationObservation,
}

/// The request envelope. `operation` names the payload shape; the payload
/// itself is parsed per operation, so the envelope carries it untyped.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MutationEnvelope {
    workspace_id: Option<String>,
    operation: Operation,
    payload: Value,
}

// One payload struct per contract row. Every one is camelCase on the wire
// and refuses unknown fields, so a payload for the wrong operation is a 400.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NamePayload {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenamePayload {
    old_name: String,
    new_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MergePayload {
    source: String,
    target: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TriplePayload {
    from: String,
    to: String,
    relation_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChangeRelationTypePayload {
    from: String,
    to: String,
    relation_type: String,
    new_relation_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EntityAttributesPayload {
    entity_name: String,
    attributes: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EntityAttributeKeysPayload {
    entity_name: String,
    keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RelationAttributesPayload {
    from: String,
    to: String,
    relation_type: String,
    attributes: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RelationAttributeKeysPayload {
    from: String,
    to: String,
    relation_type: String,
    keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ObservationPayload {
    entity_name: String,
    body: String,
    occurred_at_us: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ObservationIdPayload {
    entity_name: String,
    observation_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditObservationPayload {
    entity_name: String,
    observation_id: i64,
    body: String,
    occurred_at_us: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RelationObservationPayload {
    from: String,
    to: String,
    relation_type: String,
    body: String,
    occurred_at_us: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RelationObservationIdPayload {
    from: String,
    to: String,
    relation_type: String,
    observation_id: i64,
}

impl Operation {
    /// Validate the payload against the contract table and build the shared
    /// mutation request. The shared enum serializes snake_case, so the
    /// camelCase operation name maps onto its snake_case variant here.
    fn into_request(self, payload: Value) -> Result<MutationRequest, String> {
        fn parse<T: DeserializeOwned>(value: Value) -> Result<T, String> {
            serde_json::from_value(value).map_err(|error| format!("invalid payload: {error}"))
        }
        Ok(match self {
            Operation::CreateEntity => MutationRequest::CreateEntities {
                entities: vec![parse::<EntityInput>(payload)?],
            },
            Operation::RenameEntity => {
                let p: RenamePayload = parse(payload)?;
                MutationRequest::RenameEntity {
                    old_name: p.old_name,
                    new_name: p.new_name,
                }
            }
            Operation::MergeEntities => {
                let p: MergePayload = parse(payload)?;
                MutationRequest::MergeEntities {
                    source: p.source,
                    target: p.target,
                }
            }
            Operation::DeleteEntity => {
                let p: NamePayload = parse(payload)?;
                MutationRequest::DeleteEntities {
                    names: vec![p.name],
                }
            }
            Operation::CreateRelation => MutationRequest::CreateRelations {
                relations: vec![parse::<RelationInput>(payload)?],
            },
            Operation::DeleteRelation => {
                let p: TriplePayload = parse(payload)?;
                MutationRequest::DeleteRelations {
                    relations: vec![Relation {
                        from: p.from,
                        to: p.to,
                        relation_type: p.relation_type,
                    }],
                }
            }
            Operation::ReverseRelation => {
                let p: TriplePayload = parse(payload)?;
                MutationRequest::ReverseRelation {
                    from: p.from,
                    to: p.to,
                    relation_type: p.relation_type,
                }
            }
            Operation::ChangeRelationType => {
                let p: ChangeRelationTypePayload = parse(payload)?;
                MutationRequest::ChangeRelationType {
                    from: p.from,
                    to: p.to,
                    relation_type: p.relation_type,
                    new_relation_type: p.new_relation_type,
                }
            }
            Operation::SetEntityAttributes => {
                let p: EntityAttributesPayload = parse(payload)?;
                MutationRequest::SetAttributes {
                    targets: vec![AttributeSet {
                        owner_kind: "entity".into(),
                        entity_name: Some(p.entity_name),
                        from: None,
                        to: None,
                        relation_type: None,
                        attributes: p.attributes,
                    }],
                }
            }
            Operation::DeleteEntityAttributes => {
                let p: EntityAttributeKeysPayload = parse(payload)?;
                MutationRequest::DeleteAttributes {
                    targets: vec![AttributeDelete {
                        owner_kind: "entity".into(),
                        entity_name: Some(p.entity_name),
                        from: None,
                        to: None,
                        relation_type: None,
                        keys: p.keys,
                    }],
                }
            }
            Operation::SetRelationAttributes => {
                let p: RelationAttributesPayload = parse(payload)?;
                MutationRequest::SetAttributes {
                    targets: vec![AttributeSet {
                        owner_kind: "relation".into(),
                        entity_name: None,
                        from: Some(p.from),
                        to: Some(p.to),
                        relation_type: Some(p.relation_type),
                        attributes: p.attributes,
                    }],
                }
            }
            Operation::DeleteRelationAttributes => {
                let p: RelationAttributeKeysPayload = parse(payload)?;
                MutationRequest::DeleteAttributes {
                    targets: vec![AttributeDelete {
                        owner_kind: "relation".into(),
                        entity_name: None,
                        from: Some(p.from),
                        to: Some(p.to),
                        relation_type: Some(p.relation_type),
                        keys: p.keys,
                    }],
                }
            }
            Operation::AddObservation => {
                let p: ObservationPayload = parse(payload)?;
                MutationRequest::AddObservations {
                    observations: vec![ObservationUpdate {
                        entity_name: p.entity_name,
                        contents: vec![ObservationInput {
                            body: p.body,
                            occurred_at_us: p.occurred_at_us,
                        }],
                    }],
                }
            }
            Operation::DeleteObservation => {
                let p: ObservationIdPayload = parse(payload)?;
                MutationRequest::DeleteObservationById {
                    entity_name: p.entity_name,
                    observation_id: p.observation_id,
                }
            }
            Operation::EditObservation => {
                let p: EditObservationPayload = parse(payload)?;
                MutationRequest::EditObservation {
                    entity_name: p.entity_name,
                    observation_id: p.observation_id,
                    body: p.body,
                    occurred_at_us: p.occurred_at_us,
                }
            }
            Operation::AddRelationObservation => {
                let p: RelationObservationPayload = parse(payload)?;
                MutationRequest::AddRelationObservations {
                    relations: vec![RelationObservationUpdate {
                        relation: Relation {
                            from: p.from,
                            to: p.to,
                            relation_type: p.relation_type,
                        },
                        contents: vec![ObservationInput {
                            body: p.body,
                            occurred_at_us: p.occurred_at_us,
                        }],
                    }],
                }
            }
            Operation::DeleteRelationObservation => {
                let p: RelationObservationIdPayload = parse(payload)?;
                MutationRequest::DeleteRelationObservationById {
                    from: p.from,
                    to: p.to,
                    relation_type: p.relation_type,
                    observation_id: p.observation_id,
                }
            }
        })
    }
}

/// What the blocking gate finished with, spelled out so the handler match
/// never has to guess at nested result types.
enum MutateOutcome {
    BadPayload(String),
    Unauthorized,
    CategoryDisabled,
    MissingScope(&'static str),
    WorkspaceInput(WorkspaceError),
    NotFound,
    WriteDenied,
    WorkspaceFault(WorkspaceError),
    Applied,
    Failed(MCSError),
}

/// `POST /ui/api/mutations` — one endpoint for every graph write the UI
/// offers. The body is `{workspaceId, operation, payload}`; the operation
/// and the payload names are fixed in the design contract.
async fn ui_mutations_handler(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let registry = Arc::clone(&state.registry);
    let handles = Arc::clone(&state.handles);
    // The write category, read from the same enabled list the MCP dispatch
    // path publishes its flags from.
    let write_enabled = state
        .enabled_categories
        .contains(&crate::tools::ToolCategory::GraphWrite);
    let gate_state = state.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let Some(principal) = principal_of_ui(&gate_state, &headers, None) else {
            return MutateOutcome::Unauthorized;
        };
        if !write_enabled {
            return MutateOutcome::CategoryDisabled;
        }
        if let Some(missing) = crate::authz::missing_scope(&principal, WRITE_TOOL) {
            return MutateOutcome::MissingScope(missing);
        }
        let envelope: MutationEnvelope = match serde_json::from_slice(&body) {
            Ok(envelope) => envelope,
            Err(error) => return MutateOutcome::BadPayload(error.to_string()),
        };
        let request = match envelope.operation.into_request(envelope.payload) {
            Ok(request) => request,
            Err(message) => return MutateOutcome::BadPayload(message),
        };
        let workspace_id = envelope.workspace_id.as_deref();
        // The read resolve names every workspace the caller may see. An
        // unknown or fully denied id answers 404; a caller who can see the
        // workspace but cannot write it answers 403.
        let record = match registry.resolve(&principal.id, workspace_id, WorkspaceAccess::Read) {
            Ok(record) => record,
            Err(error @ (WorkspaceError::SelectionRequired | WorkspaceError::InvalidInput(_))) => {
                return MutateOutcome::WorkspaceInput(error);
            }
            Err(WorkspaceError::NotFound | WorkspaceError::AccessDenied) => {
                return MutateOutcome::NotFound;
            }
            Err(error) => return MutateOutcome::WorkspaceFault(error),
        };
        if let Err(error) = registry.resolve(&principal.id, workspace_id, WorkspaceAccess::Write) {
            return match error {
                WorkspaceError::SelectionRequired | WorkspaceError::InvalidInput(_) => {
                    MutateOutcome::WorkspaceInput(error)
                }
                _ => MutateOutcome::WriteDenied,
            };
        }
        let handle = match handles.get(&record) {
            Ok(entry) => entry.kg,
            Err(error) => return MutateOutcome::WorkspaceFault(error),
        };
        match apply_with_conflict_check(&handle, &principal.id, request) {
            Ok(()) => MutateOutcome::Applied,
            Err(error) => MutateOutcome::Failed(error),
        }
    })
    .await;
    match outcome {
        Ok(outcome) => respond(&state, outcome),
        Err(join_err) => {
            error!("mutation task join failed: {join_err}");
            mutation_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error",
            )
        }
    }
}

fn respond(state: &HttpState, outcome: MutateOutcome) -> Response {
    match outcome {
        MutateOutcome::Applied => Json(serde_json::json!({ "ok": true })).into_response(),
        MutateOutcome::BadPayload(message) => {
            mutation_error(StatusCode::BAD_REQUEST, "bad_request", message)
        }
        MutateOutcome::Unauthorized => unauthorized_response(state),
        MutateOutcome::CategoryDisabled => mutation_error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "graph-write tools are disabled; start the server with \
             --enable-graph-write (or --enable-all) to edit the graph",
        ),
        MutateOutcome::MissingScope(scope) => insufficient_scope_response(state, scope),
        MutateOutcome::WorkspaceInput(error) => {
            mutation_error(StatusCode::BAD_REQUEST, "bad_request", error.to_string())
        }
        MutateOutcome::NotFound => {
            mutation_error(StatusCode::NOT_FOUND, "not_found", "no such row")
        }
        MutateOutcome::WriteDenied => mutation_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "write access to this workspace is required",
        ),
        MutateOutcome::WorkspaceFault(error) => {
            error!("workspace lookup failed: {error}");
            mutation_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "workspace registry error",
            )
        }
        MutateOutcome::Failed(error) => mutation_failure(error),
    }
}

/// Name conflicts answer 409 before the mutation runs. The shared service
/// silently skips a duplicate create, and the UI must see the conflict, so
/// the gateway checks the exact targets first.
fn apply_with_conflict_check(
    kg: &GraphHandle,
    actor: &str,
    request: MutationRequest,
) -> Result<(), MCSError> {
    if let Some(message) = conflict_message(kg, &request)? {
        return Err(MCSError::ConstraintViolation(message));
    }
    MutationService::new(kg)
        .apply(
            request,
            MutationContext {
                actor: actor.into(),
                origin: "ui".into(),
                correlation_id: Uuid::new_v4(),
                causation_id: None,
                hop_count: 0,
                idempotency_key: None,
            },
        )
        .map(|_| ())
}

/// The safe conflict message when the mutation's target already exists.
fn conflict_message(
    kg: &GraphHandle,
    request: &MutationRequest,
) -> Result<Option<String>, MCSError> {
    match request {
        MutationRequest::CreateEntities { entities } => {
            let name = &entities[0].name;
            if kg.entities_exist(std::slice::from_ref(name))?[0] {
                Ok(Some(format!("Entity '{name}' already exists")))
            } else {
                Ok(None)
            }
        }
        MutationRequest::CreateRelations { relations } => {
            let relation = &relations[0];
            let exists = kg.search_relations(
                Some(&relation.from),
                Some(&relation.to),
                Some(&relation.relation_type),
                None,
                Some(1),
            )?;
            if exists.is_empty() {
                Ok(None)
            } else {
                Ok(Some(format!(
                    "{} -> {} -> {} already exists",
                    relation.from, relation.relation_type, relation.to
                )))
            }
        }
        MutationRequest::RenameEntity { new_name, .. } => {
            if kg.entities_exist(std::slice::from_ref(new_name))?[0] {
                Ok(Some(format!("Entity '{new_name}' already exists")))
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

/// One error body in the `{code,message}` contract the design doc pins.
fn mutation_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

/// Map the shared service error onto the HTTP contract. An invalid request
/// is 400; an avoidable conflict is 409; anything else is logged and hidden
/// behind a safe message.
fn mutation_failure(error: MCSError) -> Response {
    match error {
        MCSError::InvalidParams(message) => {
            mutation_error(StatusCode::BAD_REQUEST, "bad_request", message)
        }
        MCSError::ConstraintViolation(message) => {
            mutation_error(StatusCode::CONFLICT, "conflict", message)
        }
        _ => {
            error!("mutation failed: {error}");
            mutation_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal error",
            )
        }
    }
}

/// The 401 of the MCP endpoint, with the UI's `{code,message}` body in
/// place of the text body. The challenge header is the contract: it names
/// the resource metadata a client needs to discover the authorization
/// server. The header shape here mirrors [`crate::http::unauthorized`].
fn unauthorized_response(state: &HttpState) -> Response {
    let mut value = String::from("Bearer");
    if let Some(oauth) = state.oauth.as_ref() {
        value.push_str(&format!(
            " resource_metadata=\"{}\"",
            oauth.resource_metadata()
        ));
        let scope = state
            .enabled_categories
            .iter()
            .map(|c| c.slug())
            .collect::<Vec<_>>()
            .join(" ");
        if !scope.is_empty() {
            value.push_str(&format!(", scope=\"{scope}\""));
        }
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, value)],
        Json(serde_json::json!({ "code": "unauthorized", "message": "authentication required" })),
    )
        .into_response()
}

/// The 403 of the MCP endpoint, with the UI's `{code,message}` body. The
/// challenge names the scope the caller must ask for, mirroring
/// [`crate::http::insufficient_scope`].
fn insufficient_scope_response(state: &HttpState, scope: &'static str) -> Response {
    let mut value = format!("Bearer error=\"insufficient_scope\", scope=\"{scope}\"");
    if let Some(oauth) = state.oauth.as_ref() {
        value.push_str(&format!(
            ", resource_metadata=\"{}\"",
            oauth.resource_metadata()
        ));
    }
    (
        StatusCode::FORBIDDEN,
        [(header::WWW_AUTHENTICATE, value)],
        Json(serde_json::json!({
            "code": "insufficient_scope",
            "message": "insufficient scope"
        })),
    )
        .into_response()
}
