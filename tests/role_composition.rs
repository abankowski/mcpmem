#[cfg(all(feature = "indexer", feature = "webhooks"))]
use std::sync::Arc;

#[cfg(all(feature = "indexer", feature = "webhooks"))]
use mcp_memory::runtime::{
    AppServices, RoleFuture, RoleLifecycle, RoleService, RuntimeComposition,
};
use mcp_memory::runtime::{ConfigError, RoleSet, RuntimeRole};

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
impl RoleService for ImmediateMcpService {
    fn run(&self) -> RoleFuture {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(all(feature = "indexer", feature = "webhooks"))]
#[tokio::test]
async fn supervises_selected_roles_and_stops_with_mcp() {
    let roles = RoleSet::parse_csv("mcp,indexer,webhooks").expect("features are compiled");
    let services = Arc::new(AppServices::new(Arc::new(ImmediateMcpService)));

    let running = RuntimeComposition::start(roles, services).expect("roles start");
    assert_eq!(
        running.lifecycle(),
        &[
            RoleLifecycle {
                role: RuntimeRole::Mcp,
                state: memory_core::LifecycleState::Running,
            },
            RoleLifecycle {
                role: RuntimeRole::Indexer,
                state: memory_core::LifecycleState::Running,
            },
            RoleLifecycle {
                role: RuntimeRole::Webhooks,
                state: memory_core::LifecycleState::Running,
            },
        ]
    );

    running
        .wait_for_shutdown()
        .await
        .expect("mcp exits cleanly");
}
