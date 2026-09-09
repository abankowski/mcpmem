//! Dependency-light types shared by memory runtime roles.
pub mod auth;
pub mod errors;
pub mod events;
pub mod graph;
pub mod jobs;
pub mod mutation;
pub mod schema;
pub mod storage;
pub mod subscriptions;
pub mod types;

/// Observable state for a supervised runtime role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    Starting,
    Running,
    Stopped,
}
