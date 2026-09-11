//! The consent page, and what a decision on it turns into.
//!
//! Consent is where least privilege is decided. The client asks for a set of
//! scopes, the principal holds a set of scopes, and the human is offered the
//! intersection — never the union, and never what the client asked for on its
//! own. What the human ticks is what the authorization code carries, so a
//! client that asks for everything gets whatever one human agreed to and
//! nothing more.
//!
//! Two rules hold this together, and both are load-bearing:
//!
//! - **The offered set is read from the login row, not recomputed.** The
//!   callback narrows `oauth_login.scopes` to the intersection at the moment it
//!   names the human, so the set [`approve`] validates against is the set the
//!   page showed. A second computation here could disagree with the page, and a
//!   human cannot consent to something they were not shown.
//! - **Every value that reaches the page is escaped.** A client name arrives
//!   from an unauthenticated registration request, so it is hostile input that
//!   this server stores and later renders. [`page`] escapes in one pass and
//!   never substitutes into a value it has already inserted.

use std::collections::BTreeSet;

use thiserror::Error;

use crate::store::{CodeGrant, Grant, LoginRecord, Store, StoreError};

/// How long an authorization code stays redeemable, in microseconds.
///
/// RFC 6749 section 4.1.2 caps a code at ten minutes and asks for less. A code
/// is spent by a token request the client sends the moment it receives the
/// redirect, so a minute is a wide margin over a slow network and a narrow
/// window for a code sitting in a proxy log.
pub const CODE_TTL_US: i64 = 60 * 1_000_000;

/// The page frame. `{{scopes}}` is the one placeholder that takes markup: the
/// rows are built from [`SCOPE_ROW`], whose own value is escaped first.
const PAGE: &str = include_str!("consent.html");
/// One checkbox. Unticked: the human grants, rather than un-grants.
const SCOPE_ROW: &str = include_str!("consent_scope.html");

/// An approved request: the code to hand over, and where to hand it.
#[derive(Clone, Eq, PartialEq)]
pub struct Approval {
    /// The authorization code. The store holds its digest alone.
    pub code: String,
    /// The client's registered redirect URI, taken from the login row.
    pub redirect_uri: String,
    /// The state the client sent, returned unchanged, or `None` when it sent
    /// none.
    pub client_state: Option<String>,
}

/// Redacts the code, so a `tracing::debug!(?approval)` in a handler cannot put
/// a live credential in a log. The store follows the same rule for every other
/// credential it holds.
impl std::fmt::Debug for Approval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Approval")
            .field("code", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("client_state", &self.client_state)
            .finish()
    }
}

/// A refused request: where to say so. It carries no code, because none was
/// minted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Denial {
    pub redirect_uri: String,
    pub client_state: Option<String>,
}

/// Why a decision was not accepted.
///
/// Every variant is a refusal the human or the client caused, except
/// [`ConsentError::Store`]. The caller maps them to statuses; nothing here
/// redirects, because a refusal that cannot be attributed to a validated
/// redirect URI must not be sent to one.
#[derive(Debug, Error)]
pub enum ConsentError {
    /// No login in flight carries this state. A second decision on one login
    /// lands here, because the first consumed the row.
    #[error("no login in flight carries this state")]
    UnknownLogin,
    /// The consent form carried the wrong token.
    #[error("the consent form carries the wrong token")]
    BadCsrf,
    /// The login exists but no provider has vouched for a human on it yet, so
    /// there is nobody whose consent this could be.
    #[error("this login has not been authenticated")]
    NotAuthenticated,
    /// An approved scope was not on the offered set.
    #[error("an approved scope was not offered")]
    NotOffered,
    /// Nothing was approved. An empty grant is not a grant.
    #[error("no scope was approved")]
    NothingApproved,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The scopes a human may be offered: those the client asked for that the
/// principal also holds.
///
/// The order is the client's, and a repeat in the request produces one
/// checkbox: the request is unauthenticated text, and `scope=graph-read
/// graph-read` must not become two boxes or two entries in a grant.
pub fn offered(requested: &[String], held: &BTreeSet<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(requested.len().min(held.len()));
    for scope in requested {
        if held.contains(scope) && !out.contains(scope) {
            out.push(scope.clone());
        }
    }
    out
}

/// The text the page uses to name a client: its registered name when that name
/// names anything, and its identifier when it does not.
///
/// # The property
///
/// *A client name is used only when it carries at least one character that is
/// both in a rendering category and not a default-ignorable code point.*
/// Everything else — absent, empty, whitespace, a format control, or a letter
/// a conforming renderer draws no glyph for — falls back to the identifier,
/// which always names the client.
///
/// # Why the property is spelled with two halves
///
/// This check has been wrong four times, and the first three fixes were
/// deny-lists, each narrower than the last: `is_empty`, defeated by three
/// spaces; `trim().is_empty()`, which follows Unicode `White_Space` and so does
/// not see `U+200B ZERO WIDTH SPACE`. A list of things that draw nothing cannot
/// be finished by enumeration.
///
/// So the first half is inverted: a character must be in a category that
/// carries glyphs before it counts. `is_alphanumeric` is every script's letters
/// and digits, and `is_ascii_graphic` is the printable ASCII punctuation.
///
/// The second half exists because **a category test is not a rendering test**,
/// and the fourth input proved it. The four Hangul fillers — `U+115F`,
/// `U+1160`, `U+3164` and `U+FFA0` — are general category `Lo` and Unicode
/// `Alphabetic`, so `is_alphanumeric` admits all four, and all four are
/// `Default_Ignorable_Code_Point`, so a conforming renderer draws nothing.
/// `U+3164` is what people already use to make a blank display name. They are
/// not obscure new characters either: all four have existed since Unicode 1.1.
///
/// [`DEFAULT_IGNORABLE_LETTERS`] is the whole subtraction, and it is small for
/// a checkable reason rather than a hopeful one: every other
/// `Default_Ignorable_Code_Point` is `Cf`, `Mn` or `Cn`, and none of those
/// passes the first half. Unicode reserves ranges for future
/// default-ignorables, and an unassigned code point in them is `Cn`, so it
/// fails the first half today. **The residual risk is named rather than
/// denied**: if a future Unicode assigns a default-ignorable code point an
/// alphabetic or numeric category, it must be added here. That is the one case
/// this cannot decide on its own, and it is why the round-3 claim that an
/// unknown character can never count was wrong.
///
/// The failure direction is the safe one. A name that renders only as symbols
/// outside ASCII, `"→"` say, is judged to name nothing and the page shows the
/// identifier instead. That is a worse label and a correct page. The opposite
/// mistake — showing a human a prompt that names nobody — is the one this
/// function exists to prevent.
pub fn client_label<'a>(client_name: &'a str, client_id: &'a str) -> &'a str {
    if client_name.chars().any(renders) {
        client_name
    } else {
        client_id
    }
}

/// The `Default_Ignorable_Code_Point` members that a category test admits: the
/// four Hangul fillers, every one of them a letter that draws no glyph.
///
/// See [`client_label`] for why this list is complete against today's Unicode,
/// and for the one future change that would extend it.
const DEFAULT_IGNORABLE_LETTERS: [char; 4] = ['\u{115f}', '\u{1160}', '\u{3164}', '\u{ffa0}'];

/// Whether a browser draws this character as a mark a human can see.
///
/// A category allow-list, minus the letters that are default-ignorable: see
/// [`client_label`]. Neither half is enough on its own.
fn renders(c: char) -> bool {
    (c.is_alphanumeric() || c.is_ascii_graphic()) && !DEFAULT_IGNORABLE_LETTERS.contains(&c)
}

/// The consent page for one login.
///
/// `offered` is the set from [`offered`]; every other argument is text. All
/// five are escaped here, and no substituted value is scanned again, so a
/// client name spelling `{{csrf}}` stays those eight characters.
pub fn page(
    client_name: &str,
    principal_label: &str,
    offered: &[String],
    csrf: &str,
    state: &str,
) -> String {
    let mut rows = String::with_capacity(offered.len() * SCOPE_ROW.len());
    for scope in offered {
        rows.push_str(&render(SCOPE_ROW, &[("scope", &escape_html(scope))]));
    }
    render(
        PAGE,
        &[
            ("client_name", &escape_html(client_name)),
            ("principal_label", &escape_html(principal_label)),
            ("csrf", &escape_html(csrf)),
            ("state", &escape_html(state)),
            // Markup, not text: the rows are built above, and each value in
            // them is escaped there.
            ("scopes", &rows),
        ],
    )
}

/// Grant `approved` on the login named by `state`, and mint one code.
///
/// The login row is consumed, so a second approval finds nothing. A refusal
/// puts it back: a human whose form went stale, or who ticked nothing, must be
/// able to decide again, and a stranger who guesses a state must not be able to
/// end somebody else's login.
///
/// `approved` must be a subset of the offered set, which is the login's own
/// `scopes` column — see the module documentation. Anything else is
/// [`ConsentError::NotOffered`], including a scope the client asked for that the
/// principal does not hold.
pub fn approve(
    store: &Store,
    state: &str,
    csrf: &str,
    approved: &[String],
    now_us: i64,
) -> Result<Approval, ConsentError> {
    let (login, principal) = take_verified(store, state, csrf, now_us)?;
    if approved.iter().any(|s| !login.scopes.contains(s)) {
        return Err(restore(store, &login, ConsentError::NotOffered));
    }
    // Filtered out of the offered set rather than copied from `approved`: the
    // grant is then in the offered order and free of repeats, whatever a
    // crafted form body sent.
    let scopes: Vec<String> = login
        .scopes
        .iter()
        .filter(|s| approved.contains(s))
        .cloned()
        .collect();
    if scopes.is_empty() {
        return Err(restore(store, &login, ConsentError::NothingApproved));
    }

    let code = crate::new_token();
    let grant = CodeGrant {
        grant: Grant {
            client_id: login.client_id,
            principal,
            scopes,
            resource: login.resource,
            // One family per grant. Every token minted from this code, and
            // every refresh that follows, is revoked together.
            family: crate::new_token(),
        },
        redirect_uri: login.redirect_uri,
        code_challenge: login.code_challenge,
    };
    store.put_code(&code, &grant, now_us, now_us + CODE_TTL_US)?;
    Ok(Approval {
        code,
        redirect_uri: grant.redirect_uri,
        client_state: login.client_state,
    })
}

/// Refuse the login named by `state`, and consume it. The human said no, so
/// the login is over and no retry is offered.
///
/// It carries the same token check as [`approve`]: a denial ends a login, and
/// a stranger who could forge one could stop every login on this server.
pub fn deny(store: &Store, state: &str, csrf: &str, now_us: i64) -> Result<Denial, ConsentError> {
    let (login, _) = take_verified(store, state, csrf, now_us)?;
    Ok(Denial {
        redirect_uri: login.redirect_uri,
        client_state: login.client_state,
    })
}

/// Consume the login and check the form's token against it, returning the row
/// and the human it names.
///
/// The take is the mutual exclusion: two decisions arriving together mean one
/// `DELETE ... RETURNING` returns a row and the other returns nothing, so only
/// one can mint a code. A refusal after the take puts the row back.
///
/// The token is checked before the principal, so the answer does not tell a
/// caller holding a state whether the human has come back from the provider
/// yet.
fn take_verified(
    store: &Store,
    state: &str,
    csrf: &str,
    now_us: i64,
) -> Result<(LoginRecord, String), ConsentError> {
    let Some(login) = store.take_login(state, now_us)? else {
        return Err(ConsentError::UnknownLogin);
    };
    if !csrf_matches(csrf, &login.csrf) {
        return Err(restore(store, &login, ConsentError::BadCsrf));
    }
    let Some(principal) = login.principal.clone() else {
        return Err(restore(store, &login, ConsentError::NotAuthenticated));
    };
    Ok((login, principal))
}

/// Put a taken login back, and answer with `e` unless the store refused.
fn restore(store: &Store, login: &LoginRecord, e: ConsentError) -> ConsentError {
    match store.put_login(login) {
        Ok(()) => e,
        Err(put) => ConsentError::Store(put),
    }
}

/// Compare the form's token with the login's, in constant time.
///
/// Both sides are hashed first, so the comparison runs over two 64-byte strings
/// whatever the presented value was: `subtle`'s slice comparison answers early
/// on a length mismatch, and hashing removes the length from the question.
fn csrf_matches(presented: &str, expected: &str) -> bool {
    crate::digest_eq(&crate::digest(presented), &crate::digest(expected))
}

/// The page-safe form of a value chosen by a client.
///
/// Two jobs, and both are about the page being read by a human rather than
/// parsed by a machine.
///
/// It escapes the five characters that would otherwise let the value leave the
/// text of the page and become markup in it. A `client_name` comes from an
/// unauthenticated registration request, and a scope slug from an
/// authorization request, so both are hostile until escaped.
///
/// It also **drops every Unicode bidirectional control**. Those twelve
/// characters reorder the text a browser draws around them: `U+202E` alone
/// makes the run after it render backwards, so a client name can rewrite the
/// sentence beside it on the one page whose job is to tell a human what they
/// are about to grant. Escaping them would not help — they are invisible and
/// act on rendering, not on parsing — so they are removed. This is a deny-list
/// and it is still complete: `Bidi_Control` is a closed Unicode property with
/// exactly these members, and no future character joins it.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            // The `Bidi_Control` set: ALM, LRM, RLM, the four embedding and
            // override codes with their terminator, and the three isolates
            // with theirs.
            '\u{61c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Substitute `{{key}}` in `template` from `values`, in one pass.
///
/// One pass is the point, not an optimization: a second pass over the result
/// would substitute into a value that was just inserted, so a client name
/// spelling `{{csrf}}` would be filled in with the token. Everything written to
/// the output is skipped, and only the template is scanned.
///
/// A `{{key}}` no caller fills stays in the output as text. It is a mismatch
/// between this module and a template compiled into it, which
/// `the_rendered_page_leaves_no_placeholder_behind` catches — and a panic in a
/// request path is a worse answer than visible source text.
fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 256);
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            // An unterminated `{{` is text like any other.
            out.push_str("{{");
            rest = after;
            continue;
        };
        let key = &after[..close];
        match values.iter().find(|(k, _)| *k == key) {
            Some((_, value)) => out.push_str(value),
            None => {
                out.push_str("{{");
                out.push_str(key);
                out.push_str("}}");
            }
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offered set is the intersection, in the client's order, without
    /// repeats. Order matters because it is the order of the grant a token
    /// carries; repeats matter because the request is unauthenticated text.
    #[test]
    fn the_offered_set_is_the_intersection_in_request_order() {
        let held: BTreeSet<String> = ["graph-read", "graph-write"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let requested = [
            "graph-write".to_string(),
            "code".to_string(),
            "graph-read".to_string(),
            "graph-write".to_string(),
        ];
        assert_eq!(offered(&requested, &held), ["graph-write", "graph-read"]);
    }

    /// Nothing in common is an empty offer, which the caller refuses rather
    /// than showing a page with no box on it.
    #[test]
    fn nothing_in_common_is_an_empty_offer() {
        let held: BTreeSet<String> = ["graph-read".to_string()].into_iter().collect();
        assert!(offered(&["code".to_string()], &held).is_empty());
    }

    /// A name in any script names its client. The check must not be a Latin
    /// filter: `is_alphanumeric` is every script's letters and digits, and a
    /// page that fell back for a Japanese name would be a worse page.
    #[test]
    fn a_name_that_renders_is_used_whatever_script_it_is_in() {
        for name in ["Claude Desktop", "日本語クライアント", "Клиент", "7", "***"] {
            assert_eq!(client_label(name, "the-id"), name, "for {name:?}");
        }
    }

    /// Every input that has defeated this check, in the four rounds it took.
    ///
    /// `trim` sees the first four. The next three are `Cf` or `Mn`, so a
    /// category test excludes them. The last four are `Lo` letters, and a
    /// category test admits every one of them: that is the case
    /// `Default_Ignorable_Code_Point` exists to name.
    #[test]
    fn a_name_that_draws_nothing_falls_back_to_the_identifier() {
        for name in [
            "",
            "   ",
            "\t",
            "\u{00a0}",
            "\u{200b}",
            "\u{202e}",
            "\u{feff}",
            "\u{2060}\u{034f}",
            "\u{115f}",
            "\u{1160}",
            "\u{3164}",
            "\u{ffa0}",
            // Two of them together, which is what a display name uses.
            "\u{3164}\u{3164}",
        ] {
            assert_eq!(client_label(name, "the-id"), "the-id", "for {name:?}");
        }
    }

    /// The direction this property fails in, stated as a test so that it is a
    /// decision rather than a surprise: a name that renders only as non-ASCII
    /// symbols is judged to name nothing. A worse label is the safe side; a
    /// page that names nobody is not.
    #[test]
    fn a_name_of_symbols_alone_falls_back_which_is_the_safe_direction() {
        assert_eq!(client_label("→", "the-id"), "the-id");
    }
}
