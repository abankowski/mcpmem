#![cfg(feature = "oauth")]
//! Client identifier metadata documents at the authorization endpoint.
//!
//! A client may become known to this server in two ways. `tests/oauth_registration.rs`
//! covers the first, RFC 7591 dynamic registration, and the resolution rules of
//! the second in isolation. This file covers the second where a client actually
//! meets it: `GET /oauth/authorize`, presented with an https URL as its
//! `client_id`, which the discovery document advertises support for.
//!
//! The document arrives from a fetcher the fixture supplies. The shipped one
//! speaks `https` to a host on the operator's allow-list, and no loopback
//! fixture can be such a host — so what is driven here is the wiring, the
//! allow-list decision and the record that results, while
//! `mcpmem_oauth::upstream`'s own tests cover the fetcher's limits.
//!
//! One `Server` at a time per thread: each test drops its server before the
//! next is built.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mcpmem_oauth::registration::Fetch;

mod support;
use support::flow::{self, code_challenge, query_string};

/// The document URL the test client presents as its identifier. `claude.ai` is
/// on the default allow-list.
const DOCUMENT_URL: &str = "https://claude.ai/oauth/client-metadata.json";

/// A fetch that answers every URL with one document.
struct StubFetch(String);
impl Fetch for StubFetch {
    fn get(&self, _url: &str) -> Result<String, String> {
        Ok(self.0.clone())
    }
}

/// A fetch that must never be called: the host is checked before the request,
/// so a refused URL leaves no network trace at all.
struct PanicFetch;
impl Fetch for PanicFetch {
    fn get(&self, url: &str) -> Result<String, String> {
        panic!("must not fetch {url}");
    }
}

/// A fetch that answers one document and counts the calls, for the test that a
/// resolved document is read once.
struct CountingFetch {
    body: String,
    calls: Arc<AtomicUsize>,
}
impl Fetch for CountingFetch {
    fn get(&self, _url: &str) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.body.clone())
    }
}

/// A well-formed document naming `url` as its own identifier.
fn document(url: &str) -> String {
    format!(
        r#"{{"client_id":"{url}","client_name":"Claude",
             "redirect_uris":["{}"]}}"#,
        flow::CLIENT_REDIRECT
    )
}

/// A server whose upstream provider is never reached — every test here is
/// refused or redirected before the provider matters — reading documents with
/// `fetch`.
async fn server_reading(fetch: Arc<dyn Fetch>) -> support::Server {
    support::server_with_fetch(
        Some(support::oauth_config("https://idp.invalid")),
        fetch,
        support::Scopes::all(),
        None,
    )
    .await
}

fn authorize_request(client_id: &str) -> Request<Body> {
    let challenge = code_challenge();
    let query = query_string(&[
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", flow::CLIENT_REDIRECT),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("scope", "graph-read"),
    ]);
    Request::get(format!("/oauth/authorize?{query}"))
        .body(Body::empty())
        .unwrap()
}

/// The capability the discovery document advertises, end to end: a client that
/// never registered presents its document URL and the authorization request is
/// accepted.
///
/// The upstream provider is unreachable here, so the request cannot reach the
/// redirect — but it gets past `unknown client_id`, which is where it died
/// before, and the stored row is the proof that the document was resolved and
/// kept.
#[tokio::test]
async fn a_metadata_document_identifier_is_accepted_by_the_authorize_endpoint() {
    let server = server_reading(Arc::new(StubFetch(document(DOCUMENT_URL)))).await;
    let res = server.request(authorize_request(DOCUMENT_URL)).await;

    assert_eq!(
        res.status(),
        StatusCode::BAD_GATEWAY,
        "the request must pass the client check and die at the unreachable \
         provider: {}",
        flow::body_text(res).await
    );

    let stored = flow::with_store(&server, |store| store.get_client(DOCUMENT_URL))
        .unwrap()
        .expect("the resolved document must be stored as a client");
    assert_eq!(stored.client_id, DOCUMENT_URL);
    assert_eq!(stored.source, "cimd");
    assert_eq!(stored.redirect_uris, vec![flow::CLIENT_REDIRECT.to_owned()]);
}

/// The document is read once. The row it produced is what every later
/// authorization request for that identifier resolves against, so a client in
/// daily service costs its host one request in its life rather than one per
/// login.
#[tokio::test]
async fn a_resolved_document_is_not_fetched_again() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = server_reading(Arc::new(CountingFetch {
        body: document(DOCUMENT_URL),
        calls: Arc::clone(&calls),
    }))
    .await;

    for attempt in 1..=3 {
        let res = server.request(authorize_request(DOCUMENT_URL)).await;
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY, "attempt {attempt}");
    }
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "the document must be read once, not once per authorization request"
    );
    assert_eq!(flow::count_rows(&server, "oauth_client"), 1);
}

/// A document on a host the operator did not allow is refused, and nothing
/// leaves this process. The endpoint is anonymous, so without that order this
/// server would fetch any URL any caller names.
#[tokio::test]
async fn a_document_on_a_domain_outside_the_list_is_refused_without_a_request() {
    let server = server_reading(Arc::new(PanicFetch)).await;
    let res = server
        .request(authorize_request("https://evil.example/client.json"))
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(
        res.headers().get("location").is_none(),
        "a refusal at the authorization endpoint never redirects"
    );
    assert_eq!(
        flow::count_rows(&server, "oauth_client"),
        0,
        "a refused document stores nothing"
    );
}

/// An identifier that is not an https URL cannot be a document, so it is the
/// plain `unknown client_id` it always was — and no fetch is attempted for it.
#[tokio::test]
async fn an_identifier_that_is_not_a_url_is_still_an_unknown_client() {
    let server = server_reading(Arc::new(PanicFetch)).await;
    for client_id in [
        "never-registered",
        "http://claude.ai/oauth/client-metadata.json",
    ] {
        let res = server.request(authorize_request(client_id)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{client_id}");
        assert_eq!(
            flow::body_text(res).await,
            "unknown client_id",
            "{client_id}"
        );
    }
}

/// A document whose `client_id` is not the URL it was fetched from is refused.
/// That equality is the whole binding between the identifier a client presents
/// and the metadata this server acts on, and it has to hold on the route and
/// not only in the resolver.
#[tokio::test]
async fn a_document_claiming_another_identifier_is_refused_by_the_endpoint() {
    let other = document("https://claude.ai/someone-else.json");
    let server = server_reading(Arc::new(StubFetch(other))).await;
    let res = server.request(authorize_request(DOCUMENT_URL)).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        flow::count_rows(&server, "oauth_client"),
        0,
        "a refused document stores nothing"
    );
}

/// The redirect URI is still compared byte for byte against the document's own
/// list. A resolved client is a client like any other.
#[tokio::test]
async fn a_redirect_uri_the_document_does_not_name_is_refused() {
    let server = server_reading(Arc::new(StubFetch(document(DOCUMENT_URL)))).await;
    let challenge = code_challenge();
    let query = query_string(&[
        ("response_type", "code"),
        ("client_id", DOCUMENT_URL),
        ("redirect_uri", "https://claude.ai/somewhere-else"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("scope", "graph-read"),
    ]);
    let res = server
        .request(
            Request::get(format!("/oauth/authorize?{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        flow::body_text(res).await,
        "redirect_uri is not registered for this client"
    );
}

/// The shipped fetcher, on the path it actually runs on.
///
/// `Fetch::get` is synchronous and reaches the network, and `client_of` runs it
/// inside `tokio::task::spawn_blocking`. The client underneath it is
/// `reqwest::blocking`, which owns a runtime on its own thread and blocks the
/// caller on a channel — so this is the one combination that could panic
/// rather than answer, and no test above reaches it: the real fetcher speaks
/// `https` to a host on the allow-list, and no loopback fixture can be one.
///
/// A closed loopback port is enough to prove the mechanism. The request is
/// built, the client is created, the connection is refused, and an `Err` comes
/// back through the blocking task rather than a panic.
#[tokio::test]
async fn the_shipped_fetcher_answers_from_a_blocking_task() {
    use mcpmem_oauth::upstream::MetadataFetch;

    let outcome = tokio::task::spawn_blocking(|| {
        MetadataFetch::new().get("https://127.0.0.1:1/client-metadata.json")
    })
    .await
    .expect("the fetch must not panic under the blocking pool");
    let err = outcome.expect_err("a closed port cannot answer a document");
    assert!(
        err.contains("127.0.0.1:1"),
        "the failure must name the URL it was reading: {err}"
    );
}
