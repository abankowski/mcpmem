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
    assert_eq!(
        body["redirect_uris"][0],
        "https://claude.ai/api/mcp/auth_callback"
    );
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

/// The identifier is this server's to issue. A request that names one must not
/// take it: a client that chose `client_id` could name a client already
/// registered and inherit its redirect list.
#[tokio::test]
async fn a_client_supplied_client_id_is_ignored() {
    let (status, body) = register(
        r#"{"client_id":"impostor","client_name":"Evil",
            "source":"cimd","created_us":1,
            "redirect_uris":["https://claude.ai/cb"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(body["client_id"], "impostor");
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
