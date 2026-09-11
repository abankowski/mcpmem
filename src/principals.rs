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
    /// The admin scope has no [`ToolCategory`], so it is handled before the
    /// parse: it must survive the set, not vanish as an unknown slug.
    pub fn scope_set(&self) -> BTreeSet<String> {
        self.scopes
            .iter()
            .filter_map(|s| {
                if s == ADMIN_SCOPE {
                    Some(ADMIN_SCOPE.to_owned())
                } else {
                    s.parse::<ToolCategory>().ok().map(|c| c.slug().to_owned())
                }
            })
            .collect()
    }
}

/// The single scope spelling for administration. It grants access to the
/// admin API only; no tool carries it.
pub const ADMIN_SCOPE: &str = "admin";

/// True when `slug` names a scope this server issues: a tool category or
/// the admin scope.
pub fn is_known_scope(slug: &str) -> bool {
    slug == ADMIN_SCOPE || slug.parse::<ToolCategory>().is_ok()
}

/// Canonicalize a scope list: trim, reject unknown slugs, keep input order.
/// Known tool categories are stored as their canonical slug, not the input
/// text: `from_str` accepts `Graph_Read` and ` code `, which would otherwise
/// pass every startup check and then match no tool at request time.
/// The caller decides whether an empty result is allowed.
pub fn canonical_scopes(raw: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(raw.len());
    for slug in raw {
        let slug = slug.trim();
        if slug.is_empty() {
            continue;
        }
        if slug == ADMIN_SCOPE {
            out.push(ADMIN_SCOPE.to_owned());
            continue;
        }
        match slug.parse::<ToolCategory>() {
            Ok(category) => out.push(category.slug().to_owned()),
            Err(_) => {
                return Err(MCSError::InvalidParams(format!("unknown scope '{slug}'")));
            }
        }
    }
    Ok(out)
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
        let scopes = canonical_scopes(&entry.scopes)?;
        if scopes.is_empty() {
            return Err(MCSError::InvalidParams(format!(
                "principal '{}' has no scopes",
                entry.name
            )));
        }
        entry.scopes = scopes;
        if !seen.insert((entry.iss.clone(), entry.sub.clone())) {
            return Err(MCSError::InvalidParams(format!(
                "duplicate principal identity {} {}",
                entry.iss, entry.sub
            )));
        }
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_scope_is_known_and_survives_scope_set() {
        assert!(is_known_scope(ADMIN_SCOPE));
        let entry = PrincipalEntry {
            name: "ops".into(),
            iss: "https://issuer".into(),
            sub: "s1".into(),
            label: None,
            scopes: vec![ADMIN_SCOPE.to_owned()],
        };
        assert!(entry.scope_set().contains(ADMIN_SCOPE));
    }

    #[test]
    fn tool_category_slugs_stay_canonical() {
        assert!(is_known_scope("graph-read"));
        assert!(!is_known_scope("graph-admin"));
        let entry = PrincipalEntry {
            name: "adam".into(),
            iss: "https://issuer".into(),
            sub: "s2".into(),
            label: None,
            scopes: vec!["graph-read".into(), "graph-read".into()],
        };
        // Canonical slugs only, in input order.
        assert_eq!(
            canonical_scopes(&["graph-read".into(), "admin".into()]).unwrap(),
            vec!["graph-read", "admin"]
        );
    }

    #[test]
    fn canonical_scopes_refuses_unknown_scopes() {
        assert!(canonical_scopes(&["graph-read".into(), "everything".into()]).is_err());
    }
}
