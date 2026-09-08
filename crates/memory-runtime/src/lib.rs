//! Feature-gated runtime role selection and supervision.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use memory_core::LifecycleState;
use thiserror::Error;
use tokio::task::{JoinError, JoinSet};

pub type RoleFuture = Pin<Box<dyn Future<Output = Result<(), RuntimeError>> + Send + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RuntimeRole {
    Mcp,
    Indexer,
    Webhooks,
}

impl RuntimeRole {
    const fn name(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Indexer => "indexer",
            Self::Webhooks => "webhooks",
        }
    }

    const fn is_compiled(self) -> bool {
        match self {
            Self::Mcp => true,
            Self::Indexer => cfg!(feature = "indexer"),
            Self::Webhooks => cfg!(feature = "webhooks"),
        }
    }
}

impl std::fmt::Display for RuntimeRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ConfigError {
    #[error("at least one runtime role is required")]
    EmptyRoleSet,
    #[error("runtime role names cannot be empty")]
    EmptyRoleName,
    #[error("unknown runtime role '{0}'")]
    UnknownRole(String),
    #[error("runtime role '{0}' was selected more than once")]
    DuplicateRole(RuntimeRole),
    #[error("runtime role '{0}' was selected but its Cargo feature is not compiled")]
    RoleNotCompiled(RuntimeRole),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleSet(Vec<RuntimeRole>);

impl RoleSet {
    pub fn parse_csv(input: &str) -> Result<Self, ConfigError> {
        if input.trim().is_empty() {
            return Err(ConfigError::EmptyRoleSet);
        }

        let roles = input
            .split(',')
            .map(str::trim)
            .map(Self::parse_role)
            .collect::<Result<Vec<_>, _>>()?;

        for (index, role) in roles.iter().enumerate() {
            if roles[..index].contains(role) {
                return Err(ConfigError::DuplicateRole(*role));
            }
            if !role.is_compiled() {
                return Err(ConfigError::RoleNotCompiled(*role));
            }
        }

        Ok(Self(roles))
    }

    pub fn mcp_only() -> Self {
        Self(vec![RuntimeRole::Mcp])
    }

    pub fn roles(&self) -> &[RuntimeRole] {
        &self.0
    }

    fn parse_role(value: &str) -> Result<RuntimeRole, ConfigError> {
        match value {
            "mcp" => Ok(RuntimeRole::Mcp),
            "indexer" => Ok(RuntimeRole::Indexer),
            "webhooks" => Ok(RuntimeRole::Webhooks),
            "" => Err(ConfigError::EmptyRoleName),
            unknown => Err(ConfigError::UnknownRole(unknown.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleLifecycle {
    pub role: RuntimeRole,
    pub state: LifecycleState,
}

pub trait RoleService: Send + Sync {
    fn run(&self) -> RoleFuture;
}

#[derive(Clone)]
pub struct AppServices {
    mcp: Arc<dyn RoleService>,
    #[cfg(feature = "indexer")]
    indexer: Arc<dyn RoleService>,
    #[cfg(feature = "webhooks")]
    webhooks: Arc<dyn RoleService>,
}

impl AppServices {
    pub fn new(mcp: Arc<dyn RoleService>) -> Self {
        Self {
            mcp,
            #[cfg(feature = "indexer")]
            indexer: Arc::new(NoopService),
            #[cfg(feature = "webhooks")]
            webhooks: Arc::new(NoopService),
        }
    }
    #[cfg(feature = "webhooks")]
    pub fn with_webhooks(mut self, webhooks: Arc<dyn RoleService>) -> Self {
        self.webhooks = webhooks;
        self
    }

    #[cfg(feature = "indexer")]
    pub fn with_indexer(mut self, indexer: Arc<dyn RoleService>) -> Self {
        self.indexer = indexer;
        self
    }
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime role '{role}' failed: {message}")]
    RoleFailed { role: RuntimeRole, message: String },
    #[error("runtime role task failed: {source}")]
    TaskFailed {
        #[source]
        source: JoinError,
    },
}

pub struct RuntimeComposition;

impl RuntimeComposition {
    pub fn start(roles: RoleSet, services: Arc<AppServices>) -> Result<RunningRoles, RuntimeError> {
        let RoleSet(roles) = roles;
        let AppServices {
            mcp,
            #[cfg(feature = "indexer")]
            indexer,
            #[cfg(feature = "webhooks")]
            webhooks,
        } = Arc::unwrap_or_clone(services);
        let mut tasks = JoinSet::new();
        let lifecycle = roles
            .into_iter()
            .map(|role| {
                let service: Arc<dyn RoleService> = match role {
                    RuntimeRole::Mcp => mcp.clone(),
                    #[cfg(feature = "indexer")]
                    RuntimeRole::Indexer => indexer.clone(),
                    #[cfg(not(feature = "indexer"))]
                    RuntimeRole::Indexer => Arc::new(NoopService),
                    #[cfg(feature = "webhooks")]
                    RuntimeRole::Webhooks => webhooks.clone(),
                    #[cfg(not(feature = "webhooks"))]
                    RuntimeRole::Webhooks => Arc::new(NoopService),
                };
                tasks.spawn(async move { (role, service.run().await) });
                RoleLifecycle {
                    role,
                    state: LifecycleState::Running,
                }
            })
            .collect();

        Ok(RunningRoles { lifecycle, tasks })
    }
}

struct NoopService;

impl RoleService for NoopService {
    fn run(&self) -> RoleFuture {
        Box::pin(std::future::pending())
    }
}

pub struct RunningRoles {
    lifecycle: Vec<RoleLifecycle>,
    tasks: JoinSet<(RuntimeRole, Result<(), RuntimeError>)>,
}

impl RunningRoles {
    pub fn lifecycle(&self) -> &[RoleLifecycle] {
        &self.lifecycle
    }

    pub async fn wait_for_shutdown(mut self) -> Result<(), RuntimeError> {
        let result = self.tasks.join_next().await;
        self.tasks.abort_all();

        match result {
            Some(Ok((_, Ok(())))) | None => Ok(()),
            Some(Ok((_, Err(error)))) => Err(error),
            Some(Err(source)) => Err(RuntimeError::TaskFailed { source }),
        }
    }
}
