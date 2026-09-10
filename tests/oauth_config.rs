use std::io::Write;

use clap::Parser;
use mcpmem::{Args, config::Config};

fn write_tmp(name: &str, body: &str) -> String {
    let dir = std::env::temp_dir().join(format!("mcpmem-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn a_valid_principals_file_loads() {
    let path = write_tmp(
        "ok.json",
        r#"[{"name":"adam","iss":"https://idp.example","sub":"42",
             "label":"adam@example","scopes":["graph-read","graph-write"]}]"#,
    );
    let list = mcpmem::principals::load(&path).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].scopes, vec!["graph-read", "graph-write"]);
}

#[test]
fn an_empty_principals_file_is_refused() {
    let path = write_tmp("empty.json", "[]");
    let err = mcpmem::principals::load(&path).unwrap_err().to_string();
    assert!(err.contains("empty"), "message was: {err}");
}

#[test]
fn an_unknown_scope_is_refused() {
    let path = write_tmp(
        "bad-scope.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-admin"]}]"#,
    );
    let err = mcpmem::principals::load(&path).unwrap_err().to_string();
    assert!(err.contains("graph-admin"), "message was: {err}");
}

#[test]
fn a_duplicate_identity_is_refused() {
    let path = write_tmp(
        "dup.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["code"]},
            {"name":"b","iss":"https://i","sub":"1","scopes":["code"]}]"#,
    );
    let err = mcpmem::principals::load(&path).unwrap_err().to_string();
    assert!(err.contains("duplicate"), "message was: {err}");
}

fn args(extra: &[&str]) -> Args {
    let mut argv = vec!["mcpmem"];
    argv.extend_from_slice(extra);
    Args::parse_from(argv)
}

#[test]
fn oauth_without_tls_is_refused() {
    let p = write_tmp(
        "p1.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let err = Config::from_args(&args(&[
        "--transport",
        "http",
        "--oidc-issuer",
        "https://idp.example",
        "--oidc-client-id",
        "abc",
        "--public-url",
        "https://mem.example.com",
        "--principals-file",
        &p,
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("TLS"), "message was: {err}");
}

#[test]
fn oauth_without_public_url_is_refused() {
    let p = write_tmp(
        "p2.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let err = Config::from_args(&args(&[
        "--transport",
        "http",
        "--oauth-trust-forwarded-proto",
        "--oidc-issuer",
        "https://idp.example",
        "--oidc-client-id",
        "abc",
        "--principals-file",
        &p,
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("--public-url"), "message was: {err}");
}

// `webhooks` is not a default feature, and `RoleSet::parse_csv` rejects a role
// that is not compiled. Without this gate the test would fail on the role name
// rather than on the refusal it exists to prove.
#[cfg(feature = "webhooks")]
#[test]
fn oauth_without_the_mcp_role_is_refused() {
    let p = write_tmp(
        "p3.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let err = Config::from_args(&args(&[
        "--transport",
        "http",
        "--role",
        "webhooks",
        "--oauth-trust-forwarded-proto",
        "--oidc-issuer",
        "https://idp.example",
        "--oidc-client-id",
        "abc",
        "--public-url",
        "https://mem.example.com",
        "--principals-file",
        &p,
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("mcp role"), "message was: {err}");
}

#[test]
fn a_public_url_with_a_trailing_slash_is_normalized() {
    let p = write_tmp(
        "p4.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let cfg = Config::from_args(&args(&[
        "--transport",
        "http",
        "--oauth-trust-forwarded-proto",
        "--oidc-issuer",
        "https://idp.example",
        "--oidc-client-id",
        "abc",
        "--public-url",
        "https://mem.example.com/",
        "--principals-file",
        &p,
    ]))
    .unwrap();
    assert_eq!(cfg.oauth.unwrap().public_url, "https://mem.example.com");
}

#[test]
fn no_static_bearer_scopes_flag_grants_every_category() {
    let cfg = Config::from_args(&args(&[])).unwrap();
    assert_eq!(cfg.bearer_scopes, mcpmem::tools::ToolCategory::ALL.to_vec());
    assert!(cfg.oauth.is_none());
}

#[test]
fn static_bearer_scopes_narrow_the_static_token() {
    use mcpmem::tools::ToolCategory as C;
    let cfg = Config::from_args(&args(&["--static-bearer-scopes", "graph-read,code"])).unwrap();
    assert_eq!(cfg.bearer_scopes, vec![C::GraphRead, C::Code]);
}

#[test]
fn an_unknown_static_bearer_scope_is_refused() {
    let err = Config::from_args(&args(&["--static-bearer-scopes", "graph-admin"]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("graph-admin"), "message was: {err}");
}

#[test]
fn a_plaintext_public_url_is_refused() {
    let p = write_tmp(
        "p5.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let err = Config::from_args(&args(&[
        "--transport",
        "http",
        "--oauth-trust-forwarded-proto",
        "--oidc-issuer",
        "https://idp.example",
        "--oidc-client-id",
        "abc",
        "--public-url",
        "http://mem.example.com",
        "--principals-file",
        &p,
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("https"), "message was: {err}");
}

#[test]
fn an_empty_client_secret_file_is_refused() {
    let p = write_tmp(
        "p6.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let secret = write_tmp("secret-empty.txt", "   \n");
    let err = Config::from_args(&args(&[
        "--transport",
        "http",
        "--oauth-trust-forwarded-proto",
        "--oidc-issuer",
        "https://idp.example",
        "--oidc-client-id",
        "abc",
        "--public-url",
        "https://mem.example.com",
        "--principals-file",
        &p,
        "--oidc-client-secret-file",
        &secret,
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("is empty"), "message was: {err}");
}

#[test]
fn the_client_metadata_domain_allowlist_has_a_default() {
    let p = write_tmp(
        "p7.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    );
    let cfg = Config::from_args(&args(&[
        "--transport",
        "http",
        "--oauth-trust-forwarded-proto",
        "--oidc-issuer",
        "https://idp.example/",
        "--oidc-client-id",
        "abc",
        "--public-url",
        "https://mem.example.com",
        "--principals-file",
        &p,
    ]))
    .unwrap();
    let oauth = cfg.oauth.unwrap();
    assert_eq!(oauth.cimd_allowed_domains, vec!["claude.ai", "chatgpt.com"]);
    // The issuer loses its trailing slash, so later tasks can concatenate paths.
    assert_eq!(oauth.oidc_issuer, "https://idp.example");
    assert!(oauth.oidc_client_secret.is_none());
    assert!(oauth.trust_forwarded_proto);
}
