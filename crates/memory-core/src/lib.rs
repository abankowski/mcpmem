//! Dependency-light types shared by memory runtime roles.
pub mod errors;
pub mod graph;
pub mod mutation;
pub mod storage;
pub mod types;

/// Observable state for a supervised runtime role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    Starting,
    Running,
    Stopped,
}
