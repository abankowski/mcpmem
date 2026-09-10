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

    /// The scopes as canonical slugs, which is the form
    /// [`crate::authz::missing_scope`] compares against
    /// [`crate::tools::scope_of`]. [`load`] already stores them this way; the
    /// parse here also covers an entry built by another deserializer.
    pub fn scope_set(&self) -> BTreeSet<String> {
        self.scopes
            .iter()
            .filter_map(|s| s.parse::<ToolCategory>().ok())
            .map(|c| c.slug().to_owned())
            .collect()
    }
}

/// Read and validate the principals file. Fails closed: an unreadable, empty or
/// invalid file stops the server. The entries come back canonical — identity
/// fields trimmed, scopes as [`ToolCategory`] slugs — so every later
/// comparison is plain string equality.
pub fn load(path: &str) -> Result<Vec<PrincipalEntry>> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        MCSError::InvalidParams(format!("failed to read --principals-file '{path}': {e}"))
    })?;
    let mut list: Vec<PrincipalEntry> = serde_json::from_str(&text).map_err(|e| {
        // Not "invalid JSON": a well-formed file that omits a field lands here
        // too, and an operator told to look for a syntax error will not find one.
        MCSError::InvalidParams(format!(
            "--principals-file '{path}' is not a valid principals file: {e}"
        ))
    })?;
    if list.is_empty() {
        return Err(MCSError::InvalidParams(format!(
            "--principals-file '{path}' is empty; refusing to start with nobody allowed"
        )));
    }
    let mut seen = BTreeSet::new();
    for entry in &mut list {
        // Trim into the stored entry, not just for the check below: an
        // untrimmed `sub` can never equal a provider claim, and two entries
        // differing only by whitespace would slip past the duplicate guard.
        entry.name = entry.name.trim().to_owned();
        entry.iss = entry.iss.trim().to_owned();
        entry.sub = entry.sub.trim().to_owned();
        if entry.name.is_empty() || entry.iss.is_empty() || entry.sub.is_empty() {
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
        for scope in &mut entry.scopes {
            match scope.parse::<ToolCategory>() {
                // Store the slug, not the input text: `from_str` accepts
                // `Graph_Read` and ` code `, which would otherwise pass every
                // startup check and then match no tool at request time.
                Ok(category) => *scope = category.slug().to_owned(),
                Err(_) => {
                    return Err(MCSError::InvalidParams(format!(
                        "principal '{}' names unknown scope '{scope}'",
                        entry.name
                    )));
                }
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
