//! The two OAuth discovery documents, and the RFC 6750 challenge that points a
//! client at them.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::Arc;

mod support;

use support::PUBLIC_URL;

fn tools_list() -> Request<Body> {
    Request::post("/mcp")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        ))
        .unwrap()
}

fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn an_unauthenticated_mcp_post_names_the_resource_metadata() {
    let server = support::oauth_server().await;
    let res = server.request(tools_list()).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let header = support::header(&res, "www-authenticate");
    assert_eq!(
        header,
        format!(
            "Bearer resource_metadata=\"{PUBLIC_URL}/.well-known/oauth-protected-resource\", \
             scope=\"graph-read graph-write vectors code\""
        )
    );
}

/// RFC 6749 section 3.3 admits no empty scope list, and RFC 6750 makes the
/// parameter optional, so a server with no category enabled must leave it out
/// rather than send `scope=""`. A parser that rejects the malformed auth-param
/// discards the whole header, and with it the only discovery pointer.
#[tokio::test]
async fn the_challenge_omits_the_scope_parameter_when_no_category_is_enabled() {
    let server = support::oauth_server_without_categories().await;
    let res = server.request(tools_list()).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let header = support::header(&res, "www-authenticate");
    assert_eq!(
        header,
        format!("Bearer resource_metadata=\"{PUBLIC_URL}/.well-known/oauth-protected-resource\"")
    );
}

/// One server must not answer with two shapes of 401. The viewer's data
/// endpoints refuse with the same challenge as `/mcp`, so a scripted client
/// finds the authorization server from either.
#[tokio::test]
async fn the_ui_gate_sends_the_same_challenge_as_the_mcp_endpoint() {
    let server = support::oauth_server().await;
    let mcp = server.request(tools_list()).await;
    let ui = server.request(get("/ui/graph")).await;
    assert_eq!(ui.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        support::header(&ui, "www-authenticate"),
        support::header(&mcp, "www-authenticate")
    );
}

#[tokio::test]
async fn the_protected_resource_document_names_this_server() {
    let server = support::oauth_server().await;
    let res = server
        .request(get("/.well-known/oauth-protected-resource"))
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(
        body,
        json!({
            "resource": format!("{PUBLIC_URL}/mcp"),
            "authorization_servers": [PUBLIC_URL],
            "scopes_supported": ["graph-read", "graph-write", "vectors", "code", "admin"],
            "bearer_methods_supported": ["header"]
        })
    );
}

/// RFC 9728 section 3.1 inserts the well-known suffix between the host and the
/// path of the resource identifier, so a client holding
/// `https://mem.example.com/mcp` fetches `…/oauth-protected-resource/mcp`. That
/// URL must answer, and section 3.3 requires the `resource` value it returns to
/// be the identifier the client started from.
#[tokio::test]
async fn the_path_suffixed_document_names_the_resource_the_client_asked_about() {
    let server = support::oauth_server().await;
    let res = server
        .request(get("/.well-known/oauth-protected-resource/mcp"))
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(body["resource"], format!("{PUBLIC_URL}/mcp"));
    assert_eq!(body["authorization_servers"][0], PUBLIC_URL);
}

/// One rule covers every suffix: it names the resource identically, or it is
/// 404. A trailing slash makes a different identifier, so a client holding
/// `…/mcp/` must not be answered with a document about `…/mcp` — section 3.3
/// requires the two to be identical, and such a client aborts on the 200.
#[tokio::test]
async fn a_suffix_that_is_not_identical_is_not_found() {
    let server = support::oauth_server().await;
    for path in [
        "/.well-known/oauth-protected-resource/mcp/",
        "/.well-known/oauth-protected-resource/mcp///",
        "/.well-known/oauth-protected-resource/somewhere/else",
        "/.well-known/oauth-authorization-server/nope",
    ] {
        let res = server.request(get(path)).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "path was: {path}");
    }
}

#[tokio::test]
async fn the_authorization_server_document_advertises_pkce_and_cimd() {
    let server = support::oauth_server().await;
    let res = server
        .request(get("/.well-known/oauth-authorization-server"))
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(
        body,
        json!({
            "issuer": PUBLIC_URL,
            "authorization_endpoint": format!("{PUBLIC_URL}/oauth/authorize"),
            "token_endpoint": format!("{PUBLIC_URL}/oauth/token"),
            "registration_endpoint": format!("{PUBLIC_URL}/oauth/register"),
            "revocation_endpoint": format!("{PUBLIC_URL}/oauth/revoke"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "client_id_metadata_document_supported": true,
            "authorization_response_iss_parameter_supported": true,
            "scopes_supported": ["graph-read", "graph-write", "vectors", "code", "admin"]
        })
    );
}

/// RFC 8414 section 3.1 inserts the well-known suffix between the host and the
/// path of the issuer identifier, exactly as RFC 9728 does for a resource. A
/// client that read `authorization_servers: ["https://host/base"]` fetches
/// `https://host/.well-known/oauth-authorization-server/base`, so that URL must
/// answer or the flow dead-ends one hop after discovery succeeded.
#[tokio::test]
async fn a_server_under_a_path_prefix_answers_both_suffixed_paths() {
    let public_url = format!("{PUBLIC_URL}/base");
    let server = support::oauth_server_at(&public_url).await;

    let res = server
        .request(get("/.well-known/oauth-authorization-server/base"))
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(body["issuer"], public_url);

    let res = server
        .request(get("/.well-known/oauth-protected-resource/base/mcp"))
        .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(body["resource"], format!("{public_url}/mcp"));
    assert_eq!(body["authorization_servers"][0], public_url);
}

/// The suffix is relative to the origin, not to the public URL. A client that
/// appends the path a second time names a resource this server does not
/// protect.
#[tokio::test]
async fn a_prefixed_server_does_not_answer_the_unprefixed_suffix() {
    let server = support::oauth_server_at(&format!("{PUBLIC_URL}/base")).await;
    for path in [
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-authorization-server/base/base",
    ] {
        let res = server.request(get(path)).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "path was: {path}");
    }
}

/// With neither OAuth nor a static token, the server is open: a request with no
/// credential is dispatched, not refused.
#[tokio::test]
async fn a_server_with_no_auth_configured_stays_open() {
    let server = support::open_server().await;
    let res = server.request(tools_list()).await;
    assert_eq!(res.status(), StatusCode::OK);
}

/// A test router must reach the tools, not just the transport. The category
/// flags the dispatcher consults are process-wide and are published when the
/// server is built, so a router whose state says `graph-read` is enabled has to
/// advertise the `graph-read` tools.
#[tokio::test]
async fn a_test_router_advertises_the_tools_of_its_enabled_categories() {
    let server = support::open_server().await;
    let res = server.request(tools_list()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = support::json(res).await;
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array")
        .iter()
        .map(|t| t["name"].as_str().expect("every tool has a name"))
        .collect();
    assert!(names.contains(&"read_graph"), "tools were: {names:?}");
    assert!(names.contains(&"delete_entities"), "tools were: {names:?}");
}

/// A server with OAuth off advertises no authorization server, so no discovery
/// document exists at either shape of either path.
#[tokio::test]
async fn the_discovery_documents_are_absent_when_oauth_is_off() {
    let server = support::open_server().await;
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-authorization-server/base",
    ] {
        let res = server.request(get(path)).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "path was: {path}");
    }
}

/// The store opened beside the graph is usable from a test, and its connection
/// carries the busy timeout `Store::new` states as its precondition. Both
/// failures are silent: a store built before the schema is migrated only breaks
/// on its first write, and a missing busy timeout only shows up as a lost
/// refresh-token replay under concurrency. Tasks 5 to 9 reach the store exactly
/// this way.
#[tokio::test]
async fn the_store_is_reachable_and_opens_on_a_migrated_schema() {
    let server = support::oauth_server().await;

    let record = mcpmem_oauth::store::ClientRecord {
        client_id: "c-1".into(),
        client_name: "probe".into(),
        redirect_uris: vec!["https://claude.ai/callback".into()],
        source: "dcr".into(),
        created_us: 1,
        last_used_us: 1,
    };
    // One call, one lock: `with_store` hands out the store for the length of
    // the closure and never the guard itself.
    let (round_trip, timeout) = server.oauth().with_store(|store| {
        store.put_client(&record).expect("the oauth tables exist");
        let round_trip = store.get_client("c-1").expect("read back");
        let timeout: i64 = store
            .connection()
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        (round_trip, timeout)
    });
    assert_eq!(
        round_trip,
        Some(record),
        "the store must round-trip through the migrated schema"
    );
    assert!(timeout > 0, "busy_timeout was {timeout}");
}

/// OAuth startup seeds the reserved admin-UI client: one row, source
/// `reserved`, its redirect named after the public URL, and both timestamps
/// stamped with the server's own clock reading.
#[tokio::test]
async fn startup_seeds_the_reserved_admin_ui_client() {
    const FIXED: i64 = 1_700_000_000_000_000;
    let server = support::oauth_server_with_clock(support::Clock::at(FIXED)).await;
    let record = server.oauth().with_store(|store| {
        store
            .get_client(mcpmem_oauth::ADMIN_CLIENT_ID)
            .expect("the store answers")
            .expect("startup seeded the admin UI client")
    });
    assert_eq!(record.client_name, "mcpmem admin UI");
    assert_eq!(record.source, "reserved");
    assert_eq!(
        record.redirect_uris,
        vec![format!("{PUBLIC_URL}/ui/admin/callback")]
    );
    assert_eq!(record.created_us, FIXED);
    assert_eq!(record.last_used_us, FIXED);
}

/// OAuth startup seeds the reserved graph-viewer client beside the admin UI:
/// one row, source `reserved`, its redirect the viewer shell's own path (the
/// page the provider sends `?code=` back to), stamped with the server clock.
#[tokio::test]
async fn startup_seeds_the_reserved_graph_client() {
    const FIXED: i64 = 1_700_000_000_000_000;
    let server = support::oauth_server_with_clock(support::Clock::at(FIXED)).await;
    let record = server.oauth().with_store(|store| {
        store
            .get_client(mcpmem_oauth::GRAPH_CLIENT_ID)
            .expect("the store answers")
            .expect("startup seeded the graph viewer client")
    });
    assert_eq!(record.client_name, "mcpmem graph viewer");
    assert_eq!(record.source, "reserved");
    assert_eq!(record.redirect_uris, vec![format!("{PUBLIC_URL}/ui")]);
    assert_eq!(record.created_us, FIXED);
    assert_eq!(record.last_used_us, FIXED);
}

/// A repeat start refreshes the seeded row instead of duplicating it:
/// `put_client` upserts, and the second open rewrites `last_used_us` but
/// never `created_us`.
#[tokio::test]
async fn reopening_the_store_upserts_the_reserved_client() {
    const FIXED: i64 = 1_700_000_000_000_000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.mcpmem");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        mcpmem_core::schema::initialize_database(&conn).unwrap();
    }
    let clock: std::sync::Arc<dyn Fn() -> i64 + Send + Sync> = Arc::new(move || FIXED);
    let open = || {
        mcpmem::oauth_routes::OauthState::open_with_clock(
            support::oauth_config("https://idp.invalid"),
            &path,
            5000,
            Arc::clone(&clock),
        )
        .expect("the store opens on the migrated schema")
    };
    let created_us = open().with_store(|store| {
        store
            .get_client(mcpmem_oauth::ADMIN_CLIENT_ID)
            .expect("the store answers")
            .expect("the first open seeded the admin client")
            .created_us
    });
    let second = open();
    let count: i64 = second.with_store(|store| {
        store
            .connection()
            .query_row("SELECT COUNT(*) FROM oauth_client", [], |r| r.get(0))
            .expect("the store counts its clients")
    });
    assert_eq!(count, 2, "a repeat start must upsert, not duplicate");
    let after_admin = second.with_store(|store| {
        store
            .get_client(mcpmem_oauth::ADMIN_CLIENT_ID)
            .expect("the store answers")
            .expect("the admin client survives the second open")
    });
    assert_eq!(after_admin.created_us, created_us);
    assert_eq!(after_admin.last_used_us, FIXED);
    let after_graph = second.with_store(|store| {
        store
            .get_client(mcpmem_oauth::GRAPH_CLIENT_ID)
            .expect("the store answers")
            .expect("the graph client survives the second open")
    });
    assert_eq!(after_graph.created_us, FIXED);
    assert_eq!(after_graph.last_used_us, FIXED);
}

/// `OauthState::revoke_principal` reaches the store through the same lock a
/// handler uses, so the admin API can revoke by principal without holding the
/// guard itself.
#[tokio::test]
async fn oauth_state_revokes_by_principal_through_the_store() {
    use mcpmem_oauth::new_token;
    use mcpmem_oauth::store::{Grant, TokenKind};
    let server = support::oauth_server().await;
    let token = new_token();
    server
        .oauth()
        .with_store(|store| {
            store.put_token(
                &token,
                TokenKind::Refresh,
                &Grant {
                    client_id: "c1".into(),
                    principal: "adam".into(),
                    scopes: vec!["graph-read".into()],
                    resource: format!("{PUBLIC_URL}/mcp"),
                    family: "f1".into(),
                },
                1,
                10_000,
            )
        })
        .expect("the store writes the token");

    let count = server
        .oauth()
        .revoke_principal("adam")
        .expect("the store answers");
    assert_eq!(count, 1, "one live family names adam");

    let outcome = server
        .oauth()
        .with_store(|store| store.take_refresh(&token, 2))
        .expect("the store answers");
    assert!(
        matches!(outcome, mcpmem_oauth::store::RefreshOutcome::Unknown),
        "the wrapper revoked the family the bearer path reads"
    );
}

/// The clock a test injects is the clock the state reads, and moving it moves
/// what the state sees. Tasks 7 and 8 observe an expiry this way; no test can
/// wait out a token lifetime.
#[tokio::test]
async fn the_injected_clock_is_the_one_the_state_reads_and_it_moves() {
    const FIXED: i64 = 1_700_000_000_000_000;
    let server = support::oauth_server_with_clock(support::Clock::at(FIXED)).await;
    let now_us = &server.oauth().now_us;
    assert_eq!(now_us(), FIXED);

    server.clock().advance_seconds(3600);
    assert_eq!(now_us(), FIXED + 3_600_000_000);
    assert_eq!(server.clock().now_us(), FIXED + 3_600_000_000);
}

/// `Scopes::bearer_holds` must narrow the credential and leave the server's
/// categories alone. Swap its two fields and the document advertises
/// `["graph-read"]`, which is the only place that swap is observable: the
/// bearer list itself is private, and with OAuth on and no static token no
/// request carries the bearer principal.
#[tokio::test]
async fn bearer_holds_narrows_the_credential_and_not_the_advertised_scopes() {
    use mcpmem::tools::ToolCategory;
    let server = support::oauth_server_with_scopes(support::Scopes::bearer_holds(vec![
        ToolCategory::GraphRead,
    ]))
    .await;
    let res = server
        .request(get("/.well-known/oauth-protected-resource"))
        .await;
    let body: serde_json::Value = support::json(res).await;
    assert_eq!(
        body["scopes_supported"],
        json!(["graph-read", "graph-write", "vectors", "code", "admin"]),
        "the document advertises the enabled categories, not the credential"
    );
}

/// One `Server` at a time: the guard on the process-wide category flags is not
/// reentrant, so a second one on the same thread would wait forever. That is
/// the whole failure a hang gives the harness — no panic, no message, no test
/// name, and under `--test-threads=1` a stalled binary.
///
/// The detection is holder identity, not a deadline, so it must fire at once. A
/// deadline long enough to survive a full binary's queueing would take that
/// long to report, and would also fire on a test that did nothing wrong.
///
/// A `#[tokio::test]` drives a current-thread runtime on one libtest thread, so
/// the spawned task below runs on the thread that already holds the guard: it
/// is the two-server case, and its panic arrives as a `JoinError`.
#[tokio::test]
async fn a_second_server_on_one_thread_panics_at_once() {
    use std::time::{Duration, Instant};

    let first = support::oauth_server().await;

    let started = Instant::now();
    let error = tokio::spawn(async {
        drop(support::oauth_server().await);
    })
    .await
    .expect_err("a second server on this thread must panic");
    let waited = started.elapsed();

    assert!(error.is_panic(), "the task failed without panicking");
    let panic = error.into_panic();
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("<not a string>");
    assert!(
        message.contains("one Server at a time"),
        "message was: {message}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "the panic must not wait out a deadline; it took {waited:?}"
    );

    // Releasing the first server releases the guard, so the next one is built
    // without complaint. A holder that is recorded but never cleared would make
    // every later test on this thread panic instead.
    drop(first);
    let _second = support::oauth_server().await;
}

/// A construction that panics must leave the guard record clean. The local
/// guard unwinds and frees the mutex, and no `Server` exists for `Drop` to
/// clear, so a record taken before the construction would outlive the guard it
/// describes. Every later `server()` call on this thread would then trip the
/// reentrance assertion and fail with a confidently false cause — under
/// `--test-threads=1` that is every remaining test in the binary, because
/// libtest runs them all on the main thread and a `ThreadId` is never reused.
#[tokio::test]
async fn a_failed_construction_leaves_the_guard_record_clean() {
    let error = tokio::spawn(async {
        drop(support::server_that_fails_to_build().await);
    })
    .await
    .expect_err("the construction must panic");
    assert!(error.is_panic(), "the task failed without panicking");

    // The guard is free, so nothing must be recorded as holding it.
    let _server = support::oauth_server().await;
}
