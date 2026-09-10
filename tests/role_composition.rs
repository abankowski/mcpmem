#[cfg(all(feature = "indexer", feature = "webhooks"))]
use std::sync::Arc;

#[cfg(feature = "indexer")]
use clap::Parser;

#[cfg(all(feature = "indexer", feature = "webhooks"))]
use mcpmem::runtime::{AppServices, RoleFuture, RoleLifecycle, RoleService, RuntimeComposition};
use mcpmem::runtime::{ConfigError, RoleSet, RuntimeRole};

#[test]
fn parses_mcp_role() {
    let roles = RoleSet::parse_csv("mcp").expect("mcp is always compiled");

    assert_eq!(roles.roles(), &[RuntimeRole::Mcp]);
}

#[cfg(all(feature = "indexer", feature = "webhooks"))]
#[test]
fn parses_all_compiled_roles() {
    let roles = RoleSet::parse_csv("mcp,indexer,webhooks")
        .expect("all selected roles are compiled for this test");

    assert_eq!(
        roles.roles(),
        &[
            RuntimeRole::Mcp,
            RuntimeRole::Indexer,
            RuntimeRole::Webhooks
        ]
    );
}

#[cfg(feature = "indexer")]
#[test]
fn parses_indexer_when_the_feature_is_compiled() {
    let roles = RoleSet::parse_csv("indexer").expect("indexer feature is compiled");

    assert_eq!(roles.roles(), &[RuntimeRole::Indexer]);
}

#[cfg(feature = "indexer")]
#[test]
fn rejects_legacy_observations_for_a_worker_only_role() {
    let args =
        mcpmem::Args::try_parse_from(["mcpmem", "--role", "indexer", "--legacy-observations"])
            .expect("CLI syntax is valid");

    let error = mcpmem::config::Config::from_args(&args)
        .expect_err("legacy observation transport requires MCP");
    assert_eq!(
        error.to_string(),
        "Invalid params: --legacy-observations requires the mcp role"
    );
}

#[cfg(feature = "webhooks")]
#[test]
fn parses_webhooks_when_the_feature_is_compiled() {
    let roles = RoleSet::parse_csv("webhooks").expect("webhooks feature is compiled");

    assert_eq!(roles.roles(), &[RuntimeRole::Webhooks]);
}

#[test]
fn rejects_duplicate_roles() {
    let result = RoleSet::parse_csv("mcp,mcp");

    assert_eq!(result, Err(ConfigError::DuplicateRole(RuntimeRole::Mcp)));
}

#[test]
fn rejects_an_empty_role_list() {
    let result = RoleSet::parse_csv("   ");

    assert_eq!(result, Err(ConfigError::EmptyRoleSet));
}

#[cfg(not(feature = "indexer"))]
#[test]
fn rejects_indexer_when_the_feature_is_not_compiled() {
    let result = RoleSet::parse_csv("indexer");

    assert_eq!(
        result,
        Err(ConfigError::RoleNotCompiled(RuntimeRole::Indexer))
    );
}

#[cfg(not(feature = "webhooks"))]
#[test]
fn rejects_webhooks_when_the_feature_is_not_compiled() {
    let result = RoleSet::parse_csv("webhooks");

    assert_eq!(
        result,
        Err(ConfigError::RoleNotCompiled(RuntimeRole::Webhooks))
    );
}

#[cfg(all(feature = "indexer", feature = "webhooks"))]
struct ImmediateMcpService;

#[cfg(all(feature = "indexer", feature = "webhooks"))]
struct ImmediateWebhookService;

#[cfg(all(feature = "indexer", feature = "webhooks"))]
impl RoleService for ImmediateWebhookService {
    fn run(&self) -> RoleFuture {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(all(feature = "indexer", feature = "webhooks"))]
impl RoleService for ImmediateMcpService {
    fn run(&self) -> RoleFuture {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(all(feature = "indexer", feature = "webhooks"))]
#[tokio::test]
async fn supervises_selected_roles_and_stops_with_mcp() {
    let roles = RoleSet::parse_csv("mcp,indexer,webhooks").expect("features are compiled");
    let services = Arc::new(
        AppServices::new(Arc::new(ImmediateMcpService))
            .with_webhooks(Arc::new(ImmediateWebhookService)),
    );

    let running = RuntimeComposition::start(roles, services).expect("roles start");
    assert_eq!(
        running.lifecycle(),
        &[
            RoleLifecycle {
                role: RuntimeRole::Mcp,
                state: mcpmem_core::LifecycleState::Running,
            },
            RoleLifecycle {
                role: RuntimeRole::Indexer,
                state: mcpmem_core::LifecycleState::Running,
            },
            RoleLifecycle {
                role: RuntimeRole::Webhooks,
                state: mcpmem_core::LifecycleState::Running,
            },
        ]
    );

    running
        .wait_for_shutdown()
        .await
        .expect("mcp exits cleanly");
}
