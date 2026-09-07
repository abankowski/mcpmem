//! Dependency-light types shared by memory runtime roles.

/// Observable state for a supervised runtime role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    Starting,
    Running,
    Stopped,
}
