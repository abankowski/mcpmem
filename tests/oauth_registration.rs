//! Client registration: RFC 7591 dynamic registration, and the client
//! identifier metadata document a client publishes at an https URL.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mcpmem_oauth::registration::{Fetch, RegistrationError, resolve_metadata_document};

mod support;

/// A fetch that answers every URL with one document.
struct StubFetch(String);
impl Fetch for StubFetch {
    fn get(&self, _url: &str) -> Result<String, String> {
        Ok(self.0.clone())
    }
}

/// A fetch that must never be called. Every domain check has to happen before
/// the request, so a refused URL leaves no network trace at all.
struct PanicFetch;
impl Fetch for PanicFetch {
    fn get(&self, url: &str) -> Result<String, String> {
        panic!("must not fetch {url}");
    }
}

/// A fetch that fails, as a host that does not resolve would.
struct FailingFetch;
impl Fetch for FailingFetch {
    fn get(&self, _url: &str) -> Result<String, String> {
        Err("connection refused".into())
    }
}

const NOW_US: i64 = 1_700_000_000_000_000;

fn allowed() -> Vec<String> {
    vec!["claude.ai".to_string()]
}

fn post_to(path: &str, body: &str) -> Request<Body> {
    Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn post_register(body: &str) -> Request<Body> {
    post_to("/oauth/register", body)
}

/// An `https` URL of exactly `bytes` bytes, for the redirect-URI length cap.
fn long_uri(bytes: usize) -> String {
    const PREFIX: &str = "https://claude.ai/";
    format!("{PREFIX}{}", "c".repeat(bytes - PREFIX.len()))
}

/// One registration against a fresh server. The server lives for the call and
/// drops before the assertions, so no test holds two.
async fn register(body: &str) -> (StatusCode, serde_json::Value) {
    let server = support::oauth_server().await;
    let res = server.request(post_register(body)).await;
    let status = res.status();
    (status, support::json(res).await)
}

const CLAUDE: &str = r#"{"client_name":"Claude",
    "redirect_uris":["https://claude.ai/api/mcp/auth_callback"],
    "grant_types":["authorization_code","refresh_token"],
    "token_endpoint_auth_method":"none",
    "application_type":"native"}"#;

#[tokio::test]
async fn dynamic_registration_returns_a_client_id() {
    let (status, body) = register(CLAUDE).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!body["client_id"].as_str().unwrap().is_empty());
    assert_eq!(body["client_name"], "Claude");
    assert_eq!(
        body["redirect_uris"][0],
        "https://claude.ai/api/mcp/auth_callback"
    );
    assert_eq!(
        body["grant_types"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(body["response_types"], serde_json::json!(["code"]));
    assert_eq!(body["token_endpoint_auth_method"], "none");
    assert!(body.get("client_secret").is_none(), "clients are public");
}

/// The response states what this server does; it never echoes the request.
/// A client that asked for `client_credentials` and a secret must read back
/// the one grant set this server implements and no secret, or it builds a
/// token request this server refuses.
#[tokio::test]
async fn the_response_states_the_server_capabilities_and_never_echoes_them() {
    let (status, body) = register(
        r#"{"client_name":"Greedy",
            "redirect_uris":["https://claude.ai/cb"],
            "grant_types":["client_credentials","implicit"],
            "response_types":["token"],
            "token_endpoint_auth_method":"client_secret_basic"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        body["grant_types"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(body["response_types"], serde_json::json!(["code"]));
    assert_eq!(body["token_endpoint_auth_method"], "none");
    assert!(body.get("client_secret").is_none(), "clients are public");
}

/// The path is spelled twice: `mcpmem_oauth::metadata` advertises it and
/// `attach` mounts it. A client only ever uses the advertised one, so this
/// test reads the document and posts to what it found. Change either spelling
/// alone and discovery leads a client to a 404.
#[tokio::test]
async fn the_advertised_registration_endpoint_is_the_one_that_answers() {
    let server = support::oauth_server().await;
    let document = support::json(
        server
            .request(
                Request::get("/.well-known/oauth-authorization-server")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await,
    )
    .await;
    let advertised = document["registration_endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = advertised
        .strip_prefix(support::PUBLIC_URL)
        .expect("the endpoint is published under the public URL");

    let res = server.request(post_to(path, CLAUDE)).await;
    assert_eq!(res.status(), StatusCode::CREATED, "posted to {path}");
}

/// A server with OAuth off registers nobody. The endpoint is absent, exactly
/// as the discovery documents are, so a client finds no way in at all.
#[tokio::test]
async fn the_registration_endpoint_is_absent_when_oauth_is_off() {
    let server = support::open_server().await;
    let res = server.request(post_register(CLAUDE)).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn registration_without_a_redirect_uri_is_refused() {
    let (status, body) = register(r#"{"client_name":"Claude","redirect_uris":[]}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_client_metadata");
}

#[tokio::test]
async fn registration_with_a_plain_http_redirect_uri_is_refused() {
    let (status, body) =
        register(r#"{"client_name":"Evil","redirect_uris":["http://evil.example/cb"]}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_redirect_uri");
}

#[tokio::test]
async fn a_loopback_redirect_uri_is_accepted() {
    let (status, _) =
        register(r#"{"client_name":"Local","redirect_uris":["http://127.0.0.1:3000/callback"]}"#)
            .await;
    assert_eq!(status, StatusCode::CREATED);
}

/// A native client that binds an ephemeral port names `localhost` as often as
/// it names an address, and RFC 8252 section 7.3 admits all three spellings.
/// Each registration holds its own server, and the servers are sequential: the
/// fixture allows one at a time.
#[tokio::test]
async fn every_loopback_spelling_is_accepted() {
    for uri in [
        "http://localhost:1410/cb",
        "http://[::1]:1410/cb",
        "http://LOCALHOST/cb",
    ] {
        let body = format!(r#"{{"client_name":"Local","redirect_uris":["{uri}"]}}"#);
        let (status, _) = register(&body).await;
        assert_eq!(status, StatusCode::CREATED, "{uri} was refused");
    }
}

/// A host that merely ends in `localhost` is a public name someone else can
/// own, so the loopback exception must not reach it.
#[tokio::test]
async fn a_plain_http_redirect_uri_that_only_ends_in_localhost_is_refused() {
    let (status, body) =
        register(r#"{"client_name":"Evil","redirect_uris":["http://evil.localhost/cb"]}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_redirect_uri");
}

/// Every redirect URI is checked, not only the first. A client that hides a
/// plain-http entry behind a good one would otherwise register it.
#[tokio::test]
async fn a_second_redirect_uri_is_checked_too() {
    let (status, body) = register(
        r#"{"client_name":"Evil",
            "redirect_uris":["https://claude.ai/cb","http://evil.example/cb"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_redirect_uri");
}

/// The identifier, the source and both timestamps are this server's to set. A
/// request that names any of them must not take it: a client that chose
/// `client_id` could name a client already registered and inherit its redirect
/// list, and one that chose `source` could pass itself off as a client whose
/// metadata this server fetched and checked itself.
///
/// The stored row is what a later task reads, so the assertions are on the row
/// and not only on the response. The clock is injected, so `created_us` has a
/// value the test knows.
#[tokio::test]
async fn a_request_cannot_choose_the_identifier_the_source_or_the_timestamps() {
    let server = support::oauth_server_with_clock(support::Clock::at(NOW_US)).await;
    let res = server
        .request(post_register(
            r#"{"client_id":"impostor","client_name":"Evil",
                "source":"cimd","created_us":1,"last_used_us":1,
                "client_id_issued_at":1,
                "redirect_uris":["https://claude.ai/cb"]}"#,
        ))
        .await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = support::json(res).await;
    let client_id = body["client_id"].as_str().unwrap().to_owned();
    assert_ne!(client_id, "impostor");
    assert_eq!(body["client_id_issued_at"], NOW_US / 1_000_000);

    let record = server
        .oauth()
        .with_store(|store| store.get_client(&client_id))
        .expect("the store answers")
        .expect("the registration is stored");
    assert_eq!(record.source, "dcr", "the request chose the source");
    assert_eq!(record.created_us, NOW_US);
    assert_eq!(record.last_used_us, NOW_US);
    assert!(
        server
            .oauth()
            .with_store(|store| store.get_client("impostor"))
            .expect("the store answers")
            .is_none(),
        "the identifier the request named must hold no client"
    );
}

/// Bytes, not entries, are what one unauthenticated POST costs. The only cap
/// above this endpoint is the global body limit of 16 MiB (`src/server.rs`),
/// nothing evicts a registration, and the name reaches a consent screen a
/// human is asked to trust. Each cap is checked at its own boundary below.
#[tokio::test]
async fn an_over_long_client_name_is_refused() {
    let name = "n".repeat(257);
    let (status, body) = register(&format!(
        r#"{{"client_name":"{name}","redirect_uris":["https://claude.ai/cb"]}}"#
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_client_metadata");
}

#[tokio::test]
async fn too_many_redirect_uris_are_refused() {
    let uris = (0..9)
        .map(|i| format!(r#""https://claude.ai/cb{i}""#))
        .collect::<Vec<_>>()
        .join(",");
    let (status, body) = register(&format!(
        r#"{{"client_name":"Many","redirect_uris":[{uris}]}}"#
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_client_metadata");
}

#[tokio::test]
async fn an_over_long_redirect_uri_is_refused() {
    let uri = long_uri(2049);
    let (status, body) = register(&format!(
        r#"{{"client_name":"Long","redirect_uris":["{uri}"]}}"#
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_client_metadata");
}

/// Each cap admits its own boundary. Without this the three tests above pass
/// for a check that refuses everything.
#[tokio::test]
async fn metadata_at_every_cap_is_accepted() {
    let name = "n".repeat(256);
    let mut uris: Vec<String> = (0..7)
        .map(|i| format!(r#""https://claude.ai/cb{i}""#))
        .collect();
    uris.push(format!(r#""{}""#, long_uri(2048)));
    let (status, _) = register(&format!(
        r#"{{"client_name":"{name}","redirect_uris":[{}]}}"#,
        uris.join(",")
    ))
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

/// RFC 6749 section 3.1.2: a redirection endpoint URI must carry no fragment.
/// The comparison a later task makes is an exact string match, so a registered
/// fragment survives into the redirect it builds: `…/cb#f` plus `?code=…`
/// puts the query inside the fragment, where it never leaves the browser. The
/// client then waits for a code that was never sent, which is a dead flow
/// rather than a refusal.
#[tokio::test]
async fn a_redirect_uri_carrying_a_fragment_is_refused() {
    let (status, body) =
        register(r#"{"client_name":"Fragmented","redirect_uris":["https://claude.ai/cb#f"]}"#)
            .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_redirect_uri");
}

/// A query is not a fragment. RFC 6749 section 3.1.2 admits a query component
/// in a registered redirection endpoint, so the fragment rule must not reach
/// it.
#[tokio::test]
async fn a_redirect_uri_carrying_a_query_is_accepted() {
    let (status, _) =
        register(r#"{"client_name":"Queried","redirect_uris":["https://claude.ai/cb?tenant=7"]}"#)
            .await;
    assert_eq!(status, StatusCode::CREATED);
}

/// A store failure is this server's fault, not the client's. The status says
/// so, and the store's own message — which names the database — stays out of
/// the response.
#[tokio::test]
async fn a_store_failure_answers_a_server_error_and_leaks_no_detail() {
    let server = support::oauth_server().await;
    server
        .oauth()
        .with_store(|store| store.connection().execute("DROP TABLE oauth_client", []))
        .expect("the table exists to be dropped");

    let res = server.request(post_register(CLAUDE)).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = support::json(res).await;
    assert_eq!(body["error"], "server_error");
    assert!(
        body.get("error_description").is_none(),
        "the store detail names the database: {body}"
    );
}

#[tokio::test]
async fn a_body_that_is_not_json_is_refused() {
    let (status, body) = register("not json at all").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_client_metadata");
}

/// The registration is durable, and it is marked as a dynamic registration.
/// Tasks 6 to 8 read this row to check the redirect URI a client presents.
#[tokio::test]
async fn a_registered_client_is_stored_as_a_dynamic_registration() {
    let server = support::oauth_server().await;
    let res = server.request(post_register(CLAUDE)).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body = support::json(res).await;
    let client_id = body["client_id"].as_str().unwrap().to_owned();

    let record = server
        .oauth()
        .with_store(|store| store.get_client(&client_id))
        .expect("the store answers")
        .expect("the registration is stored");
    assert_eq!(record.source, "dcr");
    assert_eq!(record.client_name, "Claude");
    assert_eq!(
        record.redirect_uris,
        vec!["https://claude.ai/api/mcp/auth_callback"]
    );
}

/// RFC 7591 section 3.2.1 states `client_id_issued_at` in seconds. The store
/// counts microseconds, so the response needs the conversion, and a response
/// carrying microseconds dates the client a few million years out.
#[tokio::test]
async fn the_issued_at_time_is_in_seconds_and_the_stored_time_is_in_microseconds() {
    let server = support::oauth_server_with_clock(support::Clock::at(NOW_US)).await;
    let res = server.request(post_register(CLAUDE)).await;
    let body = support::json(res).await;
    assert_eq!(body["client_id_issued_at"], NOW_US / 1_000_000);

    let client_id = body["client_id"].as_str().unwrap().to_owned();
    let record = server
        .oauth()
        .with_store(|store| store.get_client(&client_id))
        .expect("the store answers")
        .expect("the registration is stored");
    assert_eq!(record.created_us, NOW_US);
    assert_eq!(record.last_used_us, NOW_US);
}

#[test]
fn a_metadata_document_on_an_allowed_domain_is_accepted() {
    let url = "https://claude.ai/oauth/client-metadata.json";
    let doc = format!(
        r#"{{"client_id":"{url}","client_name":"Claude",
             "redirect_uris":["https://claude.ai/api/mcp/auth_callback"]}}"#
    );
    let record = resolve_metadata_document(url, &allowed(), &StubFetch(doc), NOW_US).unwrap();
    assert_eq!(record.client_id, url);
    assert_eq!(record.source, "cimd");
    assert_eq!(
        record.redirect_uris,
        vec!["https://claude.ai/api/mcp/auth_callback"]
    );
    assert_eq!(record.created_us, NOW_US);
}

/// One row and one consent screen, whatever the metadata came from. The
/// fetcher caps the body at 64 KB, which is far more than a client record may
/// cost, so the same caps apply here.
#[test]
fn a_metadata_document_outside_the_caps_is_refused() {
    let url = "https://claude.ai/c.json";
    let over_long_name = "n".repeat(257);
    let over_long_uri = long_uri(2049);
    let many = (0..9)
        .map(|i| format!(r#""https://claude.ai/cb{i}""#))
        .collect::<Vec<_>>()
        .join(",");
    for members in [
        format!(r#""client_name":"{over_long_name}","redirect_uris":["https://claude.ai/cb"]"#),
        format!(r#""client_name":"Claude","redirect_uris":["{over_long_uri}"]"#),
        format!(r#""client_name":"Claude","redirect_uris":[{many}]"#),
    ] {
        let doc = format!(r#"{{"client_id":"{url}",{members}}}"#);
        let err = resolve_metadata_document(url, &allowed(), &StubFetch(doc), NOW_US).unwrap_err();
        assert!(
            matches!(err, RegistrationError::MalformedDocument),
            "a document outside the caps was accepted, or refused for another reason: {err:?}"
        );
    }
}

/// The third client-chosen text field of the row is the identifier itself,
/// which on this path is the document URL. The URL cap therefore has to reach
/// it: an uncapped `client_id` leaves the row unbounded however tight the
/// other two caps are. Both directions, because a cap that refuses its own
/// boundary is as wrong as no cap.
#[test]
fn a_metadata_document_client_id_is_capped_at_the_url_length() {
    for (bytes, accepted) in [(2048_usize, true), (2049, false)] {
        let url = long_uri(bytes);
        let doc = format!(
            r#"{{"client_id":"{url}","client_name":"Claude",
                 "redirect_uris":["https://claude.ai/cb"]}}"#
        );
        let outcome = resolve_metadata_document(&url, &allowed(), &StubFetch(doc), NOW_US);
        match (accepted, outcome) {
            (true, Ok(record)) => assert_eq!(record.client_id, url),
            (false, Err(RegistrationError::MalformedDocument)) => {}
            (_, other) => panic!("a client_id of {bytes} bytes gave {other:?}"),
        }
    }
}

/// One redirect-URI rule, whatever the metadata came from.
#[test]
fn a_metadata_document_with_a_fragment_in_a_redirect_uri_is_refused() {
    let url = "https://claude.ai/c.json";
    let doc = format!(
        r#"{{"client_id":"{url}","client_name":"Claude",
             "redirect_uris":["https://claude.ai/cb#f"]}}"#
    );
    let err = resolve_metadata_document(url, &allowed(), &StubFetch(doc), NOW_US).unwrap_err();
    assert!(matches!(err, RegistrationError::InvalidRedirectUri));
}

#[test]
fn a_metadata_document_whose_client_id_differs_from_its_url_is_refused() {
    let url = "https://claude.ai/oauth/client-metadata.json";
    let doc = r#"{"client_id":"https://claude.ai/other.json","client_name":"Claude",
                  "redirect_uris":["https://claude.ai/cb"]}"#;
    let err =
        resolve_metadata_document(url, &allowed(), &StubFetch(doc.into()), NOW_US).unwrap_err();
    assert!(matches!(err, RegistrationError::MetadataMismatch));
}

#[test]
fn a_metadata_document_on_a_domain_outside_the_list_is_never_fetched() {
    let err = resolve_metadata_document(
        "https://evil.example/client.json",
        &allowed(),
        &PanicFetch,
        NOW_US,
    )
    .unwrap_err();
    assert!(matches!(err, RegistrationError::DomainNotAllowed));
}

/// The allowed list holds domains, and the comparison is the whole host. A
/// suffix match would accept `claude.ai.evil.example`, and a prefix match
/// would accept a subdomain of an allowed domain that its owner never
/// published.
#[test]
fn a_metadata_document_on_a_neighbouring_host_is_never_fetched() {
    for url in [
        "https://claude.ai.evil.example/c.json",
        "https://evil.claude.ai/c.json",
        "https://notclaude.ai/c.json",
    ] {
        let err = resolve_metadata_document(url, &allowed(), &PanicFetch, NOW_US).unwrap_err();
        assert!(
            matches!(err, RegistrationError::DomainNotAllowed),
            "{url} was not refused"
        );
    }
}

/// A URL carrying userinfo is refused, and is not parsed into a host.
///
/// Both shapes matter, and only the second one discriminates. The first puts
/// an allowed domain in front of the real host, and any check that reads the
/// authority whole already refuses it. The second puts an allowed domain in
/// the host position, so a parser that learns to strip userinfo instead of
/// refusing it would fetch this URL — and `https://evil.example@claude.ai/`
/// is what a client sends when it wants a document URL that reads as one
/// host to a person and resolves to another.
#[test]
fn a_metadata_document_url_carrying_userinfo_is_never_fetched() {
    for url in [
        "https://claude.ai@evil.example/c.json",
        "https://evil.example@claude.ai/c.json",
    ] {
        let err = resolve_metadata_document(url, &allowed(), &PanicFetch, NOW_US).unwrap_err();
        assert!(
            matches!(err, RegistrationError::DomainNotAllowed),
            "{url} was not refused"
        );
    }
}

#[test]
fn a_metadata_document_url_without_https_is_refused() {
    let err = resolve_metadata_document("http://claude.ai/c.json", &allowed(), &PanicFetch, NOW_US)
        .unwrap_err();
    assert!(matches!(err, RegistrationError::DomainNotAllowed));
}

#[test]
fn a_metadata_document_that_is_not_json_is_refused() {
    let err = resolve_metadata_document(
        "https://claude.ai/c.json",
        &allowed(),
        &StubFetch("<html>login</html>".into()),
        NOW_US,
    )
    .unwrap_err();
    assert!(matches!(err, RegistrationError::MalformedDocument));
}

/// The consent screen names the client, so a document that names none is not
/// usable. RFC 7591 makes `client_name` optional for a registration request,
/// where the identifier is opaque and the server issues it; a metadata
/// document is fetched from a URL the client chose, and that URL is what the
/// human would otherwise be asked to trust.
///
/// Absent and empty are two separate refusals: the member is required, and a
/// present but empty name names nothing.
#[test]
fn a_metadata_document_with_no_usable_client_name_is_refused() {
    let url = "https://claude.ai/c.json";
    for members in [
        r#""redirect_uris":["https://claude.ai/cb"]"#,
        r#""client_name":"","redirect_uris":["https://claude.ai/cb"]"#,
    ] {
        let doc = format!(r#"{{"client_id":"{url}",{members}}}"#);
        let err = resolve_metadata_document(url, &allowed(), &StubFetch(doc), NOW_US).unwrap_err();
        assert!(
            matches!(err, RegistrationError::MalformedDocument),
            "{members} was not refused"
        );
    }
}

#[test]
fn a_metadata_document_without_a_redirect_uri_is_refused() {
    let url = "https://claude.ai/c.json";
    let doc = format!(r#"{{"client_id":"{url}","client_name":"Claude","redirect_uris":[]}}"#);
    let err = resolve_metadata_document(url, &allowed(), &StubFetch(doc), NOW_US).unwrap_err();
    assert!(matches!(err, RegistrationError::MalformedDocument));
}

/// One rule for redirect URIs, wherever the client metadata came from.
#[test]
fn a_metadata_document_with_a_plain_http_redirect_uri_is_refused() {
    let url = "https://claude.ai/c.json";
    let doc = format!(
        r#"{{"client_id":"{url}","client_name":"Claude",
             "redirect_uris":["http://evil.example/cb"]}}"#
    );
    let err = resolve_metadata_document(url, &allowed(), &StubFetch(doc), NOW_US).unwrap_err();
    assert!(matches!(err, RegistrationError::InvalidRedirectUri));
}

#[test]
fn a_metadata_document_that_cannot_be_fetched_is_reported_as_a_fetch_failure() {
    let err = resolve_metadata_document(
        "https://claude.ai/c.json",
        &allowed(),
        &FailingFetch,
        NOW_US,
    )
    .unwrap_err();
    assert!(matches!(err, RegistrationError::Fetch(_)), "{err:?}");
}
