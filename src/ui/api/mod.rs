//! The `/ui/api/*` JSON adapters. Each group lives in its own file and
//! registers its own routes, so the module root can mix and match them under
//! the runtime switch.

pub mod admin;
pub mod attachments;
pub mod graph;
pub mod mutations;
pub mod search;
