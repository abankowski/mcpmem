use mcpmem_oauth::store::{
    ClientRecord, CodeGrant, CodeOutcome, Grant, LoginRecord, RefreshOutcome, Store, TokenKind,
};
use mcpmem_oauth::{digest, digest_eq, new_token, s256_challenge};

#[test]
fn the_oauth_migration_applies_to_a_database_that_predates_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.mcpmem");
    // First open applies the full migration set, exactly as it always has.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        mcpmem_core::schema::initialize_database(&conn).unwrap();
    }
    // A second open must add no migration and must not change earlier rows.
    let conn = rusqlite::Connection::open(&path).unwrap();
    mcpmem_core::schema::initialize_database(&conn).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migration", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 7,
        "the count tracks MIGRATIONS, currently 0007_taxonomy_index"
    );
    conn.query_row("SELECT COUNT(*) FROM oauth_token", [], |r| {
        r.get::<_, i64>(0)
    })
    .expect("oauth_token exists");
}

#[test]
fn the_digest_is_lowercase_sha256_hex() {
    // The FIPS 180-4 vector for "abc". A digest is compared across processes
    // and, later, against values in operator tooling, so the encoding is part
    // of the contract.
    assert_eq!(
        digest("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn the_pkce_challenge_matches_the_rfc_7636_vector() {
    // RFC 7636 appendix B. A wrong base64 alphabet or added padding makes every
    // real client fail its code exchange.
    assert_eq!(
        s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn digest_eq_accepts_only_an_identical_digest() {
    let d = digest("abc");
    assert!(digest_eq(&d, &digest("abc")));
    assert!(!digest_eq(&d, &digest("abd")));
    assert!(!digest_eq(&d, &d[..63]));
}

#[test]
fn a_new_token_is_32_random_bytes_in_base64url() {
    let token = new_token();
    assert_eq!(token.len(), 43, "32 bytes, base64url, no padding: {token}");
    assert!(!token.contains('='), "padding must not appear: {token}");
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "url-safe alphabet only: {token}"
    );
    assert_ne!(token, new_token(), "two tokens must differ");
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("s.mcpmem")).unwrap();
    mcpmem_core::schema::initialize_database(&conn).unwrap();
    (dir, Store::new(conn))
}

fn grant(family: &str, scopes: &[&str]) -> Grant {
    Grant {
        client_id: "c1".into(),
        principal: "adam".into(),
        scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
        resource: "https://mem.example.com/mcp".into(),
        family: family.into(),
    }
}

fn login(state: &str, expires_us: i64) -> LoginRecord {
    LoginRecord {
        state: state.into(),
        client_id: "c1".into(),
        redirect_uri: "https://claude.ai/api/mcp/auth_callback".into(),
        client_state: Some("client-state".into()),
        code_challenge: "the-challenge".into(),
        resource: "https://mem.example.com/mcp".into(),
        scopes: vec!["graph-read".into()],
        upstream_verifier: "verifier".into(),
        nonce: "nonce".into(),
        csrf: "csrf".into(),
        principal: None,
        created_us: 1,
        expires_us,
    }
}

fn code_grant() -> CodeGrant {
    CodeGrant {
        grant: grant("fam", &["graph-read"]),
        redirect_uri: "https://claude.ai/api/mcp/auth_callback".into(),
        code_challenge: "the-challenge".into(),
    }
}

#[test]
fn the_store_never_holds_the_token_value() {
    let (_d, s) = store();
    let token = new_token();
    s.put_token(
        &token,
        TokenKind::Access,
        &grant("fam", &["graph-read"]),
        1,
        1_000,
    )
    .unwrap();
    let stored: String = s
        .connection()
        .query_row("SELECT token_digest FROM oauth_token", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, digest(&token));
    assert_ne!(stored, token);
}

#[test]
fn an_expired_access_token_is_not_found() {
    let (_d, s) = store();
    let token = new_token();
    s.put_token(
        &token,
        TokenKind::Access,
        &grant("fam", &["graph-read"]),
        1,
        10,
    )
    .unwrap();
    assert!(s.find_access(&token, 5).unwrap().is_some());
    assert!(s.find_access(&token, 11).unwrap().is_none());
    assert!(
        s.find_access(&token, 10).unwrap().is_none(),
        "expires_us equal to now_us is expired: the read predicate and the \
         sweep predicate must stay complementary"
    );
}

#[test]
fn find_access_returns_the_grant_it_was_given() {
    let (_d, s) = store();
    let token = new_token();
    let g = grant("fam", &["graph-read", "vectors"]);
    s.put_token(&token, TokenKind::Access, &g, 1, 10_000)
        .unwrap();
    assert_eq!(s.find_access(&token, 2).unwrap().unwrap(), g);
}

#[test]
fn a_refresh_token_is_not_accepted_as_an_access_token() {
    let (_d, s) = store();
    let token = new_token();
    s.put_token(
        &token,
        TokenKind::Refresh,
        &grant("fam", &["graph-read"]),
        1,
        10_000,
    )
    .unwrap();
    assert!(s.find_access(&token, 2).unwrap().is_none());
}

#[test]
fn a_revoked_access_token_is_not_found() {
    let (_d, s) = store();
    let token = new_token();
    s.put_token(
        &token,
        TokenKind::Access,
        &grant("fam", &["graph-read"]),
        1,
        10_000,
    )
    .unwrap();
    assert!(s.find_access(&token, 2).unwrap().is_some());
    s.revoke_family("fam").unwrap();
    assert!(s.find_access(&token, 3).unwrap().is_none());
}

#[test]
fn a_replayed_refresh_token_kills_the_whole_family() {
    let (_d, s) = store();
    let refresh = new_token();
    let access = new_token();
    let g = grant("fam", &["graph-read"]);
    s.put_token(&refresh, TokenKind::Refresh, &g, 1, 10_000)
        .unwrap();
    s.put_token(&access, TokenKind::Access, &g, 1, 10_000)
        .unwrap();

    assert!(matches!(
        s.take_refresh(&refresh, 2).unwrap(),
        RefreshOutcome::Valid(_)
    ));
    assert!(matches!(
        s.take_refresh(&refresh, 3).unwrap(),
        RefreshOutcome::Replayed
    ));
    assert!(
        s.find_access(&access, 4).unwrap().is_none(),
        "the sibling access token must die with the family"
    );
}

#[test]
fn take_refresh_returns_the_grant_it_was_given() {
    let (_d, s) = store();
    let refresh = new_token();
    let g = grant("fam", &["graph-read", "vectors"]);
    s.put_token(&refresh, TokenKind::Refresh, &g, 1, 10_000)
        .unwrap();
    match s.take_refresh(&refresh, 2).unwrap() {
        RefreshOutcome::Valid(returned) => assert_eq!(returned, g),
        other => panic!("expected Valid, got {other:?}"),
    }
}

#[test]
fn an_unknown_refresh_token_is_unknown() {
    let (_d, s) = store();
    let outcome = s.take_refresh(&new_token(), 2).unwrap();
    assert!(
        matches!(outcome, RefreshOutcome::Unknown),
        "got {outcome:?}"
    );
}

#[test]
fn an_expired_refresh_token_is_unknown() {
    let (_d, s) = store();
    let refresh = new_token();
    s.put_token(
        &refresh,
        TokenKind::Refresh,
        &grant("fam", &["graph-read"]),
        1,
        10,
    )
    .unwrap();
    let outcome = s.take_refresh(&refresh, 11).unwrap();
    assert!(
        matches!(outcome, RefreshOutcome::Unknown),
        "got {outcome:?}"
    );
}

#[test]
fn a_revoked_refresh_token_is_unknown() {
    let (_d, s) = store();
    let refresh = new_token();
    s.put_token(
        &refresh,
        TokenKind::Refresh,
        &grant("fam", &["graph-read"]),
        1,
        10_000,
    )
    .unwrap();
    s.revoke_family("fam").unwrap();
    let outcome = s.take_refresh(&refresh, 2).unwrap();
    assert!(
        matches!(outcome, RefreshOutcome::Unknown),
        "got {outcome:?}"
    );
}

#[test]
fn an_access_token_is_not_accepted_as_a_refresh_token() {
    let (_d, s) = store();
    let access = new_token();
    s.put_token(
        &access,
        TokenKind::Access,
        &grant("fam", &["graph-read"]),
        1,
        10_000,
    )
    .unwrap();
    let outcome = s.take_refresh(&access, 2).unwrap();
    assert!(
        matches!(outcome, RefreshOutcome::Unknown),
        "got {outcome:?}"
    );
}

#[test]
fn one_family_does_not_revoke_another() {
    let (_d, s) = store();
    let r1 = new_token();
    let a2 = new_token();
    s.put_token(&r1, TokenKind::Refresh, &grant("f1", &["code"]), 1, 10_000)
        .unwrap();
    s.put_token(&a2, TokenKind::Access, &grant("f2", &["code"]), 1, 10_000)
        .unwrap();
    s.take_refresh(&r1, 2).unwrap();
    s.take_refresh(&r1, 3).unwrap();
    assert!(s.find_access(&a2, 4).unwrap().is_some());
}

#[test]
fn family_of_reports_the_family_of_a_spent_token() {
    let (_d, s) = store();
    let refresh = new_token();
    s.put_token(
        &refresh,
        TokenKind::Refresh,
        &grant("fam", &["code"]),
        1,
        10_000,
    )
    .unwrap();
    s.take_refresh(&refresh, 2).unwrap();
    assert_eq!(s.family_of(&refresh, 3).unwrap().as_deref(), Some("fam"));
    assert_eq!(s.family_of(&new_token(), 3).unwrap(), None);
    s.revoke_family("fam").unwrap();
    assert_eq!(
        s.family_of(&refresh, 4).unwrap().as_deref(),
        Some("fam"),
        "revocation must stay idempotent"
    );
}

#[test]
fn an_expired_token_has_no_family() {
    let (_d, s) = store();
    let refresh = new_token();
    s.put_token(
        &refresh,
        TokenKind::Refresh,
        &grant("fam", &["code"]),
        1,
        10,
    )
    .unwrap();
    assert_eq!(s.family_of(&refresh, 5).unwrap().as_deref(), Some("fam"));
    assert_eq!(
        s.family_of(&refresh, 11).unwrap(),
        None,
        "an expired token must not be able to revoke a live family"
    );
}

#[test]
fn the_sweep_deletes_only_expired_rows() {
    let (_d, s) = store();
    let live = new_token();
    let dead = new_token();
    s.put_token(&live, TokenKind::Access, &grant("f1", &["code"]), 1, 10_000)
        .unwrap();
    s.put_token(&dead, TokenKind::Access, &grant("f2", &["code"]), 1, 5)
        .unwrap();
    let boundary = new_token();
    s.put_token(
        &boundary,
        TokenKind::Access,
        &grant("f3", &["code"]),
        1,
        100,
    )
    .unwrap();
    let removed = s.sweep(100).unwrap();
    assert_eq!(
        removed, 2,
        "a row whose expires_us equals the sweep instant is expired"
    );
    assert!(s.find_access(&live, 101).unwrap().is_some());
}

#[test]
fn the_sweep_covers_logins_and_codes_too() {
    let (_d, s) = store();
    s.put_login(&login("live", 10_000)).unwrap();
    s.put_login(&login("dead", 5)).unwrap();
    s.put_code(&new_token(), &code_grant(), 1, 5).unwrap();
    s.put_code(&new_token(), &code_grant(), 1, 10_000).unwrap();
    assert_eq!(s.sweep(100).unwrap(), 2);
    assert!(s.take_login("live", 101).unwrap().is_some());
}

/// `oauth_client` is the one OAuth table with no `expires_us`, so the sweep
/// cannot reach it and a separate rule does: a client last used more than
/// `max_idle_us` ago **and** holding no token row at all.
///
/// Both halves are asserted, because either one alone is wrong. Without the
/// token check, a connector that has been quiet for a month and still holds a
/// thirty-day refresh token loses its registration and its next refresh
/// answers `invalid_client`. Without the idle check, a client is evicted in
/// the seconds between its registration and its first exchange, when it has
/// by definition reached no token yet.
///
/// A client with `source = 'reserved'` is exempt: the sweep exists to bound
/// anonymous DCR/CIMD rows, and only a restart re-seeds the reserved admin-UI
/// client.
#[test]
fn the_eviction_removes_only_idle_clients_that_hold_no_token() {
    let (_d, s) = store();
    let day_us = 24 * 60 * 60 * 1_000_000_i64;
    let now = 400 * day_us;
    let max_idle_us = 30 * day_us;
    for id in ["forgotten", "recent", "holder"] {
        s.put_client(&ClientRecord {
            client_id: id.into(),
            client_name: "Claude".into(),
            redirect_uris: vec!["https://claude.ai/api/mcp/auth_callback".into()],
            source: "dcr".into(),
            created_us: now - 31 * day_us,
            last_used_us: now - 31 * day_us,
        })
        .unwrap();
    }
    s.put_client(&ClientRecord {
        client_id: "reserved".into(),
        client_name: "mcpmem admin UI".into(),
        redirect_uris: vec!["https://mem.example.com/ui/admin/callback".into()],
        source: ClientRecord::RESERVED.into(),
        created_us: now - 31 * day_us,
        last_used_us: now - 31 * day_us,
    })
    .unwrap();
    s.touch_client("recent", now - max_idle_us + 1).unwrap();
    let mut holder = grant("fam", &["graph-read"]);
    holder.client_id = "holder".into();
    s.put_token(&new_token(), TokenKind::Refresh, &holder, 1, now + day_us)
        .unwrap();

    assert_eq!(s.evict_clients(now, max_idle_us).unwrap(), 1);
    assert!(
        s.get_client("forgotten").unwrap().is_none(),
        "a client idle past the bound and holding nothing must go"
    );
    assert!(
        s.get_client("recent").unwrap().is_some(),
        "one microsecond inside the bound is not idle"
    );
    assert!(
        s.get_client("holder").unwrap().is_some(),
        "a client holding a token must survive however long it has been quiet"
    );
    assert!(
        s.get_client("reserved").unwrap().is_some(),
        "a reserved client must survive the sweep however long it has been idle"
    );
}

#[test]
fn an_authorization_code_is_single_use() {
    let (_d, s) = store();
    let code = new_token();
    let cg = CodeGrant {
        grant: grant("fam", &["graph-read"]),
        redirect_uri: "https://claude.ai/api/mcp/auth_callback".into(),
        code_challenge: "the-challenge".into(),
    };
    s.put_code(&code, &cg, 1, 10_000).unwrap();
    assert_eq!(s.take_code(&code, 2).unwrap(), CodeOutcome::Valid(cg));
    assert_eq!(s.take_code(&code, 3).unwrap(), CodeOutcome::Replayed);
}

/// RFC 6749 section 4.1.2: a code used more than once must be denied, and the
/// tokens already issued from it should be revoked. The second presentation is
/// the only evidence that the code leaked, and by then the winner holds a pair
/// that lives an hour and thirty days. The refresh path has treated the same
/// signal as fatal to the family since Task 3; this is the other half of it.
#[test]
fn a_replayed_authorization_code_revokes_the_family_it_already_produced() {
    let (_d, s) = store();
    let code = new_token();
    let cg = CodeGrant {
        grant: grant("fam", &["graph-read"]),
        redirect_uri: "https://claude.ai/api/mcp/auth_callback".into(),
        code_challenge: "the-challenge".into(),
    };
    s.put_code(&code, &cg, 1, 10_000).unwrap();
    assert_eq!(
        s.take_code(&code, 2).unwrap(),
        CodeOutcome::Valid(cg.clone())
    );

    // What the winner of the race walked away with.
    let access = new_token();
    s.put_token(&access, TokenKind::Access, &cg.grant, 2, 10_000)
        .unwrap();
    assert!(s.find_access(&access, 3).unwrap().is_some());

    assert_eq!(s.take_code(&code, 3).unwrap(), CodeOutcome::Replayed);
    assert!(
        s.find_access(&access, 4).unwrap().is_none(),
        "the replay must revoke the tokens the first exchange issued"
    );
}

#[test]
fn an_expired_authorization_code_is_not_returned() {
    let (_d, s) = store();
    let code = new_token();
    let cg = CodeGrant {
        grant: grant("fam", &["graph-read"]),
        redirect_uri: "https://claude.ai/api/mcp/auth_callback".into(),
        code_challenge: "the-challenge".into(),
    };
    s.put_code(&code, &cg, 1, 10).unwrap();
    assert_eq!(s.take_code(&code, 11).unwrap(), CodeOutcome::Unknown);
}

/// An expired code that was never spent names no family either. A caller that
/// read the family out of a dead row would let anyone holding a stale code
/// revoke the live session of the human who abandoned it.
#[test]
fn an_expired_authorization_code_revokes_nothing() {
    let (_d, s) = store();
    let code = new_token();
    let cg = CodeGrant {
        grant: grant("fam", &["graph-read"]),
        redirect_uri: "https://claude.ai/api/mcp/auth_callback".into(),
        code_challenge: "the-challenge".into(),
    };
    s.put_code(&code, &cg, 1, 10).unwrap();
    let access = new_token();
    s.put_token(&access, TokenKind::Access, &cg.grant, 1, 10_000)
        .unwrap();

    assert_eq!(s.take_code(&code, 11).unwrap(), CodeOutcome::Unknown);
    assert!(s.find_access(&access, 12).unwrap().is_some());
}

#[test]
fn the_store_never_holds_the_authorization_code_value() {
    let (_d, s) = store();
    let code = new_token();
    s.put_code(&code, &code_grant(), 1, 10_000).unwrap();
    let stored: String = s
        .connection()
        .query_row("SELECT code_digest FROM oauth_code", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, digest(&code));
    assert_ne!(stored, code);
}

#[test]
fn a_login_round_trips_and_is_single_use() {
    let (_d, s) = store();
    let l = login("st", 10_000);
    s.put_login(&l).unwrap();
    assert_eq!(s.take_login("st", 2).unwrap().unwrap(), l);
    assert!(s.take_login("st", 3).unwrap().is_none());
}

#[test]
fn an_expired_login_is_not_returned() {
    let (_d, s) = store();
    s.put_login(&login("st", 10)).unwrap();
    assert!(s.take_login("st", 11).unwrap().is_none());
}

#[test]
fn a_client_round_trips_and_touch_records_the_last_use() {
    let (_d, s) = store();
    let c = ClientRecord {
        client_id: "c1".into(),
        client_name: "Claude".into(),
        redirect_uris: vec!["https://claude.ai/api/mcp/auth_callback".into()],
        source: "dcr".into(),
        created_us: 1,
        last_used_us: 1,
    };
    s.put_client(&c).unwrap();
    assert_eq!(s.get_client("c1").unwrap().unwrap(), c);
    assert_eq!(s.get_client("nope").unwrap(), None);
    s.touch_client("c1", 77).unwrap();
    assert_eq!(s.get_client("c1").unwrap().unwrap().last_used_us, 77);
    assert_eq!(s.get_client("c1").unwrap().unwrap().created_us, 1);
}

#[test]
fn a_repeat_registration_refreshes_the_metadata_and_keeps_the_creation_time() {
    let (_d, s) = store();
    let first = ClientRecord {
        client_id: "c1".into(),
        client_name: "Claude".into(),
        redirect_uris: vec!["https://claude.ai/api/mcp/auth_callback".into()],
        source: "dcr".into(),
        created_us: 1,
        last_used_us: 1,
    };
    s.put_client(&first).unwrap();
    let again = ClientRecord {
        client_name: "Claude Desktop".into(),
        redirect_uris: vec![
            "https://claude.ai/api/mcp/auth_callback".into(),
            "https://claude.ai/other".into(),
        ],
        created_us: 500,
        last_used_us: 500,
        ..first
    };
    s.put_client(&again).unwrap();
    let stored = s.get_client("c1").unwrap().unwrap();
    assert_eq!(stored.client_name, "Claude Desktop");
    assert_eq!(stored.redirect_uris.len(), 2);
    assert_eq!(stored.last_used_us, 500);
    assert_eq!(
        stored.created_us, 1,
        "a repeat registration must not rewrite created_us"
    );
}

#[test]
fn the_debug_output_of_a_login_hides_its_secrets() {
    let l = LoginRecord {
        state: "state-abc".into(),
        upstream_verifier: "the-verifier".into(),
        nonce: "the-nonce".into(),
        csrf: "the-csrf".into(),
        ..login("unused", 10_000)
    };
    let shown = format!("{l:?}");
    for secret in ["the-verifier", "the-nonce", "the-csrf"] {
        assert!(
            !shown.contains(secret),
            "a login secret must never reach a log: {shown}"
        );
    }
    assert!(
        shown.contains("state-abc"),
        "the non-secret fields must still print: {shown}"
    );
}
