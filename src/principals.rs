//! The allowed humans, read from a JSON file at startup.
//!
//! Identity is `iss` plus `sub`. An email address is display text only, because
//! most providers let a human change it.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::errors::{MCSError, Result};
use crate::tools::ToolCategory;

#[derive(Debug, Clone, Deserialize)]
pub struct PrincipalEntry {
    pub name: String,
    pub iss: String,
    pub sub: String,
    #[serde(default)]
    pub label: Option<String>,
    pub scopes: Vec<String>,
}

impl PrincipalEntry {
    /// The identity key used to match an upstream identity token.
    pub const fn key(&self) -> (&str, &str) {
        (self.iss.as_str(), self.sub.as_str())
    }

    pub fn scope_set(&self) -> BTreeSet<String> {
        self.scopes.iter().cloned().collect()
    }
}

/// Read and validate the principals file. Fails closed: an unreadable, empty or
/// invalid file stops the server.
pub fn load(path: &str) -> Result<Vec<PrincipalEntry>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        MCSError::InvalidParams(format!("failed to read --principals-file '{path}': {e}"))
    })?;
    let list: Vec<PrincipalEntry> = serde_json::from_str(&text).map_err(|e| {
        MCSError::InvalidParams(format!("--principals-file '{path}' is not valid JSON: {e}"))
    })?;
    if list.is_empty() {
        return Err(MCSError::InvalidParams(format!(
            "--principals-file '{path}' is empty; refusing to start with nobody allowed"
        )));
    }
    let mut seen = BTreeSet::new();
    for entry in &list {
        if entry.name.trim().is_empty()
            || entry.iss.trim().is_empty()
            || entry.sub.trim().is_empty()
        {
            return Err(MCSError::InvalidParams(
                "every principal needs a non-empty name, iss and sub".into(),
            ));
        }
        if entry.scopes.is_empty() {
            return Err(MCSError::InvalidParams(format!(
                "principal '{}' has no scopes",
                entry.name
            )));
        }
        for scope in &entry.scopes {
            if scope.parse::<ToolCategory>().is_err() {
                return Err(MCSError::InvalidParams(format!(
                    "principal '{}' names unknown scope '{scope}'",
                    entry.name
                )));
            }
        }
        if !seen.insert((entry.iss.clone(), entry.sub.clone())) {
            return Err(MCSError::InvalidParams(format!(
                "duplicate principal identity {} {}",
                entry.iss, entry.sub
            )));
        }
    }
    Ok(list)
}
