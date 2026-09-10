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

// ── Fix round 1 ──────────────────────────────────────────────────────────

/// A principals file holding one valid entry.
fn one_principal(name: &str) -> String {
    write_tmp(
        name,
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["graph-read"]}]"#,
    )
}

/// The shortest command line that turns OAuth on and passes every refusal.
/// A flag named in `extra` replaces the default for that flag, because clap
/// refuses a repeated `Option` argument.
fn valid_oauth<'a>(p: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["--transport", "http", "--oauth-trust-forwarded-proto"];
    for (flag, default) in [
        ("--oidc-issuer", "https://idp.example"),
        ("--oidc-client-id", "abc"),
        ("--public-url", "https://mem.example.com"),
        ("--principals-file", p),
    ] {
        if !extra.contains(&flag) {
            v.push(flag);
            v.push(default);
        }
    }
    v.extend_from_slice(extra);
    v
}

#[test]
fn oauth_on_the_stdio_transport_is_refused() {
    let p = one_principal("f1.json");
    let err = Config::from_args(&args(&[
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
    assert!(err.contains("--transport http"), "message was: {err}");
}

#[test]
fn an_oauth_flag_without_the_issuer_is_refused() {
    let p = one_principal("f2.json");
    let secret = write_tmp("f2-secret.txt", "s3cr3t\n");
    for orphan in [
        vec!["--public-url", "https://mem.example.com"],
        vec!["--oidc-client-id", "abc"],
        vec!["--oidc-client-secret-file", secret.as_str()],
        vec!["--principals-file", p.as_str()],
        vec!["--cimd-allowed-domain", "claude.ai"],
        vec!["--oauth-trust-forwarded-proto"],
    ] {
        let mut argv = vec!["--transport", "http"];
        argv.extend_from_slice(&orphan);
        let err = Config::from_args(&args(&argv)).unwrap_err().to_string();
        assert!(err.contains("--oidc-issuer"), "{orphan:?} gave: {err}");
    }
}

#[test]
fn a_public_url_with_a_query_or_a_fragment_is_refused() {
    let p = one_principal("f3.json");
    for bad in [
        "https://mem.example.com/mcp?x=1",
        "https://mem.example.com#f",
    ] {
        let err = Config::from_args(&args(&valid_oauth(&p, &["--public-url", bad])))
            .unwrap_err()
            .to_string();
        assert!(err.contains("query"), "{bad} gave: {err}");
    }
}

#[test]
fn a_public_url_keeps_its_path_and_lowercases_its_scheme_and_host() {
    let p = one_principal("f4.json");
    let cfg = Config::from_args(&args(&valid_oauth(
        &p,
        &["--public-url", "HTTPS://MEM.Example.com/Server/MCP/"],
    )))
    .unwrap();
    // A path prefix is legal: the MCP authorization specification names
    // `https://mcp.example.com/server/mcp` as a canonical resource URI. The
    // path keeps its case; the scheme and the host do not.
    assert_eq!(
        cfg.oauth.unwrap().public_url,
        "https://mem.example.com/Server/MCP"
    );
}

#[test]
fn a_public_url_with_no_host_is_refused() {
    let p = one_principal("f5.json");
    let err = Config::from_args(&args(&valid_oauth(&p, &["--public-url", "https:///mcp"])))
        .unwrap_err()
        .to_string();
    assert!(err.contains("host"), "message was: {err}");
}

#[test]
fn a_plaintext_issuer_is_refused() {
    let p = one_principal("f6.json");
    let err = Config::from_args(&args(&valid_oauth(
        &p,
        &["--oidc-issuer", "http://idp.example"],
    )))
    .unwrap_err()
    .to_string();
    assert!(err.contains("https"), "message was: {err}");
}

#[test]
fn a_populated_client_secret_file_is_trimmed_and_kept() {
    let p = one_principal("f7.json");
    let secret = write_tmp("secret-ok.txt", "  s3cr3t\n");
    let cfg = Config::from_args(&args(&valid_oauth(
        &p,
        &["--oidc-client-secret-file", &secret],
    )))
    .unwrap();
    assert_eq!(
        cfg.oauth.unwrap().oidc_client_secret.as_deref(),
        Some("s3cr3t")
    );
}

#[test]
fn oauth_without_the_client_id_is_refused() {
    let p = one_principal("f8.json");
    let err = Config::from_args(&args(&[
        "--transport",
        "http",
        "--oauth-trust-forwarded-proto",
        "--oidc-issuer",
        "https://idp.example",
        "--public-url",
        "https://mem.example.com",
        "--principals-file",
        &p,
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("--oidc-client-id"), "message was: {err}");
}

#[test]
fn oauth_without_the_principals_file_is_refused() {
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
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("--principals-file"), "message was: {err}");
}

#[test]
fn a_principals_file_missing_a_field_is_not_called_invalid_json() {
    let path = write_tmp("f9.json", r#"[{"name":"a","iss":"https://i"}]"#);
    let err = mcpmem::principals::load(&path).unwrap_err().to_string();
    assert!(
        !err.contains("not valid JSON"),
        "valid JSON was called invalid: {err}"
    );
    assert!(err.contains("missing field"), "message was: {err}");
}

#[test]
fn principal_identity_fields_are_trimmed() {
    let path = write_tmp(
        "f10.json",
        r#"[{"name":" a ","iss":"https://i ","sub":" 42","scopes":["code"]}]"#,
    );
    let list = mcpmem::principals::load(&path).unwrap();
    assert_eq!(list[0].key(), ("https://i", "42"));
    assert_eq!(list[0].name, "a");
}

#[test]
fn a_duplicate_identity_that_differs_only_by_whitespace_is_refused() {
    let path = write_tmp(
        "f11.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["code"]},
            {"name":"b","iss":"https://i ","sub":" 1","scopes":["code"]}]"#,
    );
    let err = mcpmem::principals::load(&path).unwrap_err().to_string();
    assert!(err.contains("duplicate"), "message was: {err}");
}

#[test]
fn principal_scopes_are_stored_as_canonical_slugs() {
    let path = write_tmp(
        "f12.json",
        r#"[{"name":"a","iss":"https://i","sub":"1","scopes":["Graph_Read"," code "]}]"#,
    );
    let list = mcpmem::principals::load(&path).unwrap();
    assert_eq!(list[0].scopes, vec!["graph-read", "code"]);
}

#[test]
fn a_public_url_with_userinfo_is_refused() {
    let p = one_principal("f13.json");
    let err = Config::from_args(&args(&valid_oauth(
        &p,
        &["--public-url", "https://user:pW@mem.example.com"],
    )))
    .unwrap_err()
    .to_string();
    assert!(err.contains("userinfo"), "message was: {err}");
}

#[test]
fn an_issuer_with_userinfo_is_refused() {
    let p = one_principal("f14.json");
    let err = Config::from_args(&args(&valid_oauth(
        &p,
        &["--oidc-issuer", "https://user:pW@idp.example"],
    )))
    .unwrap_err()
    .to_string();
    assert!(err.contains("userinfo"), "message was: {err}");
}

#[test]
fn a_public_url_of_nothing_but_the_scheme_names_the_host() {
    let p = one_principal("f15.json");
    let err = Config::from_args(&args(&valid_oauth(&p, &["--public-url", "https://"])))
        .unwrap_err()
        .to_string();
    assert!(err.contains("host"), "message was: {err}");
}

#[test]
fn scope_set_canonicalizes_an_entry_that_did_not_come_from_load() {
    // `PrincipalEntry` is public and derives `Deserialize`, so an entry can
    // reach `scope_set` without passing through `load`. The set feeds
    // `authz::missing_scope`, which compares against `tools::scope_of` output,
    // so a raw `Graph_Read` here would silently grant nothing.
    let entry: mcpmem::principals::PrincipalEntry = serde_json::from_str(
        r#"{"name":"a","iss":"https://i","sub":"1","scopes":["Graph_Read","nonsense"]}"#,
    )
    .unwrap();
    let set = entry.scope_set();
    assert!(set.contains("graph-read"), "{set:?}");
    // An unknown scope grants nothing rather than being carried through.
    assert_eq!(set.len(), 1, "{set:?}");
}
