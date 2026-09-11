//! `Store::revoke_principal`: every live token family that names a principal
//! dies together, each family counted once.

use mcpmem_oauth::new_token;
use mcpmem_oauth::store::{Grant, RefreshOutcome, Store, TokenKind};

/// A store over a fresh database with the 0004 OAuth tables applied. The
/// initializer is the one every runtime role runs, so the test exercises the
/// same schema a server sees.
fn store() -> Store {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    mcpmem_core::schema::initialize_database(&conn).unwrap();
    Store::new(conn)
}

fn grant(family: &str, principal: &str) -> Grant {
    Grant {
        client_id: "c1".into(),
        principal: principal.into(),
        scopes: vec!["graph-read".into()],
        resource: "https://mem.example.com/mcp".into(),
        family: family.into(),
    }
}

/// Put one token and hand back its value, so a test can present it later.
fn token(s: &Store, kind: TokenKind, family: &str, principal: &str) -> String {
    let value = new_token();
    s.put_token(&value, kind, &grant(family, principal), 1, 10_000)
        .unwrap();
    value
}

#[test]
fn revoking_a_principal_revokes_every_live_family_and_counts_it() {
    let s = store();
    // Adam holds two live families; bob holds one. A family carries its
    // refresh token and its access token, and both must go dark.
    let adam_r = token(&s, TokenKind::Refresh, "a1", "adam");
    let adam_a = token(&s, TokenKind::Access, "a1", "adam");
    let adam_2 = token(&s, TokenKind::Refresh, "a2", "adam");
    let bob_r = token(&s, TokenKind::Refresh, "b1", "bob");
    let bob_a = token(&s, TokenKind::Access, "b1", "bob");

    assert_eq!(
        s.revoke_principal("adam").unwrap(),
        2,
        "the count is families, not tokens"
    );

    // Every adam token is dark, through the paths a bearer would use.
    assert!(
        matches!(
            s.take_refresh(&adam_r, 2).unwrap(),
            RefreshOutcome::Unknown
        ),
        "the revoked refresh token answers Unknown"
    );
    assert!(
        s.find_access(&adam_a, 2).unwrap().is_none(),
        "the revoked access token is not found"
    );
    assert!(
        matches!(
            s.take_refresh(&adam_2, 2).unwrap(),
            RefreshOutcome::Unknown
        ),
        "the second family went dark too"
    );

    // Bob's families are untouched.
    assert!(
        matches!(
            s.take_refresh(&bob_r, 2).unwrap(),
            RefreshOutcome::Valid(_)
        ),
        "bob's refresh token still spends"
    );
    assert!(
        s.find_access(&bob_a, 2).unwrap().is_some(),
        "bob's access token is still live"
    );

    // The rows are really revoked, at the column every bearer path reads.
    let flags: Vec<i64> = s
        .connection()
        .prepare("SELECT revoked FROM oauth_token ORDER BY family")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(flags, vec![1, 1, 1, 0, 0]);

    // Revocation is idempotent: nothing live remains to revoke.
    assert_eq!(s.revoke_principal("adam").unwrap(), 0);
    // A principal with no rows at all revokes nothing.
    assert_eq!(s.revoke_principal("carol").unwrap(), 0);
}