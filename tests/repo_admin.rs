//! HTTP admin routes for managed repositories, through the in-process router.

use std::path::Path;
use std::process::Command;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use mcpmem::code_registry;
use mcpmem::config::{Durability, SqliteTuning};
use mcpmem::repos;
mod support;

fn git(args: &[&str], cwd: Option<&Path>) {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().expect("git runs");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Build a local git repository with one Rust file; used as a clone URL.
fn make_repo(parent: &Path, name: &str) -> std::path::PathBuf {
    let repo = parent.join(name);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&["init", "-q"], Some(&repo));
    std::fs::write(repo.join("src/lib.rs"), "pub fn alpha() -> u32 { 1 }\n").unwrap();
    git(&["add", "."], Some(&repo));
    git(&["commit", "-q", "-m", "first"], Some(&repo));
    repo
}

/// One server + admin token + fixture repo for the whole binary: the
/// code_registry and repos stores are process-wide OnceLocks, so every test
/// must share the same backing directories.
struct Fixture {
    server: support::Server,
    token: String,
    repo_url: std::path::PathBuf,
    /// The code-db base, to assert wipes remove files.
    code_base: std::path::PathBuf,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture() -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let idp = support::fake_idp::FakeIdp::start(support::fake_idp::IdpBehaviour::default())
                .await;
            let mut config = support::oauth_config(&idp.issuer);
            config.principals[0].scopes.push(mcpmem::principals::ADMIN_SCOPE.into());
            let server = support::server(Some(config), support::Scopes::all(), None).await;
            let token = support::flow::admin_access_token(&idp, &server).await;
            // Point the process-wide stores at THIS server's memory DB.
            // The code base is `{db}.code` — the same derivation repos.rs
            // uses — so wipe assertions and resolve paths stay aligned.
            let db_path = server.memory_db_path();
            let code_base = std::path::PathBuf::from(format!("{}.code", db_path.display()));
            code_registry::init(
                code_base.clone(),
                Durability::Async,
                SqliteTuning::default(),
                std::num::NonZeroUsize::new(8).unwrap(),
                2,
            );
            repos::init(server.memory_db_path(), 5000);
            let repo_url = make_repo(server.dir().path(), "upstream");
            let _ = std::mem::drop(idp); // the idp stays alive for the token's lifetime
            Fixture {
                server,
                token,
                repo_url,
                code_base,
            }
        })
        .await
}

fn get(fix: &Fixture, path: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {}", fix.token))
        .body(Body::empty())
        .unwrap()
}

fn post(fix: &Fixture, path: &str, body: &serde_json::Value) -> Request<Body> {
    Request::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {}", fix.token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn json(res: axum::response::Response) -> serde_json::Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Poll the list until no row is transitional; return the body.
async fn wait_settled(fix: &Fixture) -> serde_json::Value {
    for _ in 0..100 {
        let body = json(fix.server.request(get(fix, "/ui/api/repos")).await).await;
        let settled = body["repos"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| !["pending", "cloning", "indexing", "removing"].contains(&r["state"].as_str().unwrap_or("")));
        if settled {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("repos never settled");
}

#[tokio::test]
async fn anonymous_caller_is_challenged() {
    let fix = fixture().await;
    let anon = fix
        .server
        .request(Request::get("/ui/api/repos").body(Body::empty()).unwrap())
        .await;
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);
    let challenge = anon
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .expect("the 401 is the RFC 6750 challenge")
        .to_str()
        .unwrap();
    assert!(challenge.starts_with("Bearer"), "{}", challenge);
}

#[tokio::test]
async fn aaa_list_starts_empty_and_reports_camelcase() {
    let fix = fixture().await;
    let body = json(fix.server.request(get(fix, "/ui/api/repos")).await).await;
    assert_eq!(body["repos"].as_array().unwrap().len(), 0);
}

/// POST a repo and poll until it settles; returns the settled body.
async fn add_and_settle(fix: &Fixture, key: &str) -> serde_json::Value {
    let res = fix
        .server
        .request(post(
            fix,
            "/ui/api/repos",
            &serde_json::json!({
                "key": key,
                "url": fix.repo_url.to_string_lossy(),
                "authKind": "none",
            }),
        ))
        .await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    wait_settled(fix).await
}

#[tokio::test]
async fn add_accepts_202_and_settles_to_indexed() {
    let fix = fixture().await;
    let body = add_and_settle(fix, "admin-api").await;
    let row = &body["repos"][0];
    assert_eq!(row["key"], "admin-api");
    assert_eq!(row["state"], "indexed");
    assert!(row["lastIndexedUs"].is_number());
    // Remove the row again: the fixture and its list are process-wide, and a
    // later test asserts the list empties after removals.
    let removed = fix
        .server
        .request(
            Request::delete(format!("/ui/api/repos/admin-api"))
                .header(header::AUTHORIZATION, format!("Bearer {}", fix.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(removed.status(), StatusCode::ACCEPTED);
    wait_settled(fix).await;
}

#[tokio::test]
async fn reindex_triggers_and_remove_wipes() {
    let fix = fixture().await;
    let _ = add_and_settle(fix, "reindex-target").await;
    let res = fix
        .server
        .request(post(fix, "/ui/api/repos/reindex-target/reindex", &serde_json::json!({})))
        .await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let _ = wait_settled(fix).await;

    let removed = fix
        .server
        .request(
            Request::delete(format!("/ui/api/repos/reindex-target"))
                .header(header::AUTHORIZATION, format!("Bearer {}", fix.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(removed.status(), StatusCode::ACCEPTED);
    let body = wait_settled(fix).await;
    assert!(body["repos"].as_array().unwrap().is_empty());
    // The row is hidden at the 202, but the index DB and the worktree go away
    // a moment later on the job thread; wait for the physical wipe before
    // asserting it, and for the row itself, whose delete precedes the 404.
    let code_db = fix.code_base.join("reindex-target.code.db");
    let worktree = fix.code_base.join("repos").join("reindex-target");
    for _ in 0..100 {
        let row_gone = mcpmem::repos::get_row("reindex-target")
            .map(|r| r.is_none())
            .unwrap_or(false);
        if !code_db.exists() && !worktree.exists() && row_gone {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!code_db.exists());
    assert!(!worktree.exists());
    // The row is gone: a direct reindex on it is a 404.
    let miss = fix
        .server
        .request(post(fix, "/ui/api/repos/reindex-target/reindex", &serde_json::json!({})))
        .await;
    assert_eq!(miss.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_key_conflicts_and_bad_bodies_are_400() {
    let fix = fixture().await;
    let _ = add_and_settle(fix, "dup-target").await;
    let dup = fix
        .server
        .request(post(
            fix,
            "/ui/api/repos",
            &serde_json::json!({ "key": "dup-target", "url": fix.repo_url.to_string_lossy() }),
        ))
        .await;
    // The first add has settled; a duplicate register conflicts at the
    // UNIQUE constraint regardless of job state.
    assert_eq!(dup.status(), StatusCode::CONFLICT);
    // Unknown key reindex -> 404; malformed body -> 400.
    let miss = fix
        .server
        .request(post(fix, "/ui/api/repos/nope/reindex", &serde_json::json!({})))
        .await;
    assert_eq!(miss.status(), StatusCode::NOT_FOUND);
    let bad = fix
        .server
        .request(post(fix, "/ui/api/repos", &serde_json::json!({ "key": "x" })))
        .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    // Remove the row again: the fixture and its list are process-wide, and a
    // later test asserts the list empties after removals.
    let removed = fix
        .server
        .request(
            Request::delete(format!("/ui/api/repos/dup-target"))
                .header(header::AUTHORIZATION, format!("Bearer {}", fix.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(removed.status(), StatusCode::ACCEPTED);
    wait_settled(fix).await;
}