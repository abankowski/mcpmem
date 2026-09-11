//! Precedence of the `--config` file against the command line and the
//! environment. The rule under test is one sentence: a command-line flag beats
//! an environment variable, which beats the file, which beats the clap default.
//!
//! Nothing here mutates the process environment. `FileConfig::apply` takes the
//! environment probe as a parameter, so a fake probe proves the rule without
//! the data race that `set_var` would introduce under a parallel test run.

use std::collections::HashSet;

use clap::CommandFactory;
use mcpmem::Args;
use mcpmem::config::Config;
use mcpmem::config_file::{ExplicitArgs, FileConfig, env_keys, resolve};

fn write_config(dir: &std::path::Path, text: &str) -> std::path::PathBuf {
    let path = dir.join("mcpmem.toml");
    std::fs::write(&path, text).expect("write config");
    path
}

/// Runs the real startup path: `main` calls exactly this function, so a
/// regression in the wiring fails these tests too.
fn merge(argv: &[&str], text: &str) -> Args {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), text);
    let mut full: Vec<std::ffi::OsString> = vec!["mcpmem".into()];
    full.extend(argv.iter().map(std::ffi::OsString::from));
    full.push("--config".into());
    full.push(path.into_os_string());
    resolve(full).expect("resolve").0
}

/// Applies a file with a fake environment: every name in `set` counts as a
/// variable that is present and non-empty.
fn merge_with_env(argv: &[&str], text: &str, set: &[&str]) -> Args {
    let present: HashSet<String> = set.iter().map(|k| (*k).to_string()).collect();
    let probe = move |key: &str| !present.contains(key);

    let mut full: Vec<std::ffi::OsString> = vec!["mcpmem".into()];
    full.extend(argv.iter().map(std::ffi::OsString::from));
    let matches = Args::command().get_matches_from(full);
    let mut args = <Args as clap::FromArgMatches>::from_arg_matches(&matches).expect("parse args");
    let explicit = ExplicitArgs::from_matches(&matches);
    let file: FileConfig = toml::from_str(text).expect("parse config");
    file.apply(&mut args, &explicit, &probe).expect("apply");
    args
}

#[test]
fn the_file_fills_an_argument_the_command_line_left_alone() {
    let args = merge(
        &[],
        r#"
[server]
bind = "0.0.0.0:9999"
transport = "http"

[vectors]
embedding-dims = 1024
index = "ivf"
"#,
    );

    assert_eq!(args.bind, "0.0.0.0:9999");
    assert!(matches!(args.transport, mcpmem::Transport::Http));
    assert_eq!(args.embedding_dims, 1024);
    assert!(matches!(args.vec_index, mcpmem::VecIndex::Ivf));
}

#[test]
fn a_command_line_flag_beats_the_file() {
    let args = merge(
        &["--bind", "127.0.0.1:1", "--embedding-dims", "384"],
        "[server]\nbind = \"0.0.0.0:9999\"\n\n[vectors]\nembedding-dims = 1024\n",
    );

    assert_eq!(args.bind, "127.0.0.1:1");
    assert_eq!(args.embedding_dims, 384);
}

/// The subtle case: `--embedding-dims 384` passes the value that is also the
/// default. Comparing against the default would treat it as absent and let the
/// file win; reading clap's value source does not.
#[test]
fn a_flag_set_to_its_own_default_still_beats_the_file() {
    let args = merge(
        &["--embedding-dims", "384"],
        "[vectors]\nembedding-dims = 1024\n",
    );
    assert_eq!(args.embedding_dims, 384);
}

#[test]
fn a_boolean_flag_is_only_ever_turned_on_by_the_file_when_absent() {
    let from_file = merge(&[], "[tools]\nall = true\n");
    assert!(from_file.enable_all);

    let from_cli = merge(&["--enable-graph-read"], "[tools]\ngraph-read = false\n");
    assert!(
        from_cli.enable_graph_read,
        "an explicit flag must not be turned off by the file"
    );
}

#[test]
fn roles_from_the_file_reach_the_config() {
    let args = merge(&[], "[server]\nroles = [\"mcp\"]\n");
    let config = Config::from_args(&args).expect("config");
    assert_eq!(config.roles.roles(), &[mcpmem::runtime::RuntimeRole::Mcp]);
}

#[test]
fn an_unknown_role_in_the_file_fails_with_the_same_error_as_the_flag() {
    let args = merge(&[], "[server]\nroles = [\"indexerr\"]\n");
    let error = Config::from_args(&args).expect_err("an unknown role must be rejected");
    assert!(
        error
            .to_string()
            .contains("unknown runtime role 'indexerr'"),
        "{error}"
    );
}

#[test]
fn durability_comes_from_the_file_and_a_bad_value_stops_the_server() {
    assert!(
        std::env::var_os(env_keys::DURABILITY).is_none(),
        "this test needs {} unset",
        env_keys::DURABILITY
    );

    let args = merge(&[], "[storage]\ndurability = \"sync\"\n");
    assert!(
        Config::from_args(&args)
            .expect("config")
            .durability
            .is_sync()
    );

    let args = merge(&[], "[storage]\ndurability = \"eventually\"\n");
    let error = Config::from_args(&args).expect_err("a bad durability must be rejected");
    assert!(error.to_string().contains("unknown durability"), "{error}");
}

#[test]
fn the_oauth_waitlist_keys_reach_the_args() {
    let args = merge(
        &[],
        "[oauth]\napproval-waitlist = true\napproval-waitlist-ttl-seconds = 3600\ndefault-new-principal-scopes = [\"graph-read\", \"graph-write\"]\n",
    );
    assert!(args.approval_waitlist);
    assert_eq!(args.approval_waitlist_ttl_seconds, Some(3600));
    assert_eq!(
        args.default_new_principal_scopes,
        Some(vec!["graph-read".to_string(), "graph-write".to_string()])
    );
}

#[test]
fn an_oauth_waitlist_flag_beats_the_file() {
    let args = merge(
        &[
            "--approval-waitlist",
            "--approval-waitlist-ttl-seconds",
            "60",
            "--default-new-principal-scope",
            "graph-write",
        ],
        "[oauth]\napproval-waitlist = false\napproval-waitlist-ttl-seconds = 3600\ndefault-new-principal-scopes = [\"graph-read\"]\n",
    );
    assert!(args.approval_waitlist);
    assert_eq!(args.approval_waitlist_ttl_seconds, Some(60));
    assert_eq!(
        args.default_new_principal_scopes,
        Some(vec!["graph-write".to_string()])
    );
}

#[test]
fn an_empty_scopes_list_in_the_file_clears_the_default() {
    let args = merge(
        &[],
        "[oauth]\ndefault-new-principal-scopes = []\n",
    );
    assert_eq!(
        args.default_new_principal_scopes,
        Some(vec![]),
        "an explicitly empty list is a value, not an absence: it must reach \
         Config::from_args and clear the graph-read default"
    );
}

// --- precedence against the environment, with an injected probe -------------

#[test]
fn a_present_environment_variable_blocks_the_file() {
    let args = merge_with_env(
        &[],
        "[server]\nmemory-file = \"/tmp/from-file.mcpmem\"\n",
        &[env_keys::MEMORY_FILE],
    );
    assert_eq!(
        args.memory_file, None,
        "the file must not fill a setting the environment already answers"
    );
}

#[test]
fn an_absent_environment_variable_lets_the_file_through() {
    let args = merge_with_env(
        &[],
        "[server]\nmemory-file = \"/tmp/from-file.mcpmem\"\n",
        &[],
    );
    assert_eq!(args.memory_file.as_deref(), Some("/tmp/from-file.mcpmem"));
}

/// The security-relevant guard: three conditions, and dropping any one of them
/// changes who authenticates.
#[test]
fn the_auth_token_file_guard_checks_all_three_conditions() {
    let file = "[security]\nauth-token-file = \"/etc/from-file\"\n";

    let clean = merge_with_env(&[], file, &[]);
    assert_eq!(clean.auth_token_file.as_deref(), Some("/etc/from-file"));

    let env_set = merge_with_env(&[], file, &[env_keys::AUTH_TOKEN]);
    assert_eq!(env_set.auth_token_file, None, "the env token must win");

    let sibling_flag = merge_with_env(&["--auth-token", "from-flag"], file, &[]);
    assert_eq!(
        sibling_flag.auth_token_file, None,
        "--auth-token must stop the file naming a different credential"
    );
}

#[test]
fn the_tls_pair_respects_the_environment() {
    let file = "[security]\ntls-cert = \"/etc/c.pem\"\ntls-key = \"/etc/k.pem\"\n";

    let clean = merge_with_env(&[], file, &[]);
    assert_eq!(clean.tls_cert.as_deref(), Some("/etc/c.pem"));
    assert_eq!(clean.tls_key.as_deref(), Some("/etc/k.pem"));

    let env_set = merge_with_env(&[], file, &[env_keys::TLS_CERT, env_keys::TLS_KEY]);
    assert_eq!(env_set.tls_cert, None);
    assert_eq!(env_set.tls_key, None);
}

/// An empty variable is the shape a compose file produces from an unset shell
/// variable. Every consumer filters it away, so the merge must treat it as
/// unset too — otherwise both sources defer to the other and the setting is
/// silently lost. For `auth-token-file` that meant starting unauthenticated.
#[test]
fn an_empty_environment_variable_counts_as_unset() {
    use mcpmem::config_file::value_absent;
    assert!(value_absent(None), "an unset variable is absent");
    assert!(
        value_absent(Some(std::ffi::OsStr::new(""))),
        "an empty variable must count as absent, or the file value is dropped \
         while the empty value is filtered away and the setting is lost"
    );
    assert!(!value_absent(Some(std::ffi::OsStr::new("token"))));
}

// --- file discovery ---------------------------------------------------------

#[test]
fn the_config_path_comes_from_the_flag_then_the_environment() {
    let args = Args::command().get_matches_from(["mcpmem"]);
    let plain = <Args as clap::FromArgMatches>::from_arg_matches(&args).expect("args");
    assert_eq!(FileConfig::resolve_path(&plain, None), None);
    assert_eq!(
        FileConfig::resolve_path(&plain, Some("/from/env.toml".into())),
        Some(std::path::PathBuf::from("/from/env.toml"))
    );

    let args = Args::command().get_matches_from(["mcpmem", "--config", "/from/flag.toml"]);
    let flagged = <Args as clap::FromArgMatches>::from_arg_matches(&args).expect("args");
    assert_eq!(
        FileConfig::resolve_path(&flagged, Some("/from/env.toml".into())),
        Some(std::path::PathBuf::from("/from/flag.toml")),
        "the flag must win over the environment"
    );
}

#[test]
fn a_missing_file_names_the_path() {
    let error = FileConfig::load(std::path::Path::new("/nonexistent/mcpmem.toml"))
        .expect_err("a named file that is absent must be an error");
    assert!(
        error.to_string().contains("/nonexistent/mcpmem.toml"),
        "{error}"
    );
}

#[test]
fn invalid_syntax_names_the_path_and_the_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "[server\nbind = \"x\"\n");
    let error = FileConfig::load(&path).expect_err("broken TOML must be an error");
    let text = error.to_string();
    assert!(text.contains("mcpmem.toml"), "{text}");
    assert!(text.contains("line"), "{text}");
}

/// A secret written where a path belongs must not come back in the error. The
/// `Display` of a TOML error quotes the offending source line, so the parser
/// error is reduced to its message before it reaches a log.
#[test]
fn a_parse_error_never_echoes_the_offending_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "[security]\nauth-token = \"sup3r-s3cr3t\"\n");
    let error = FileConfig::load(&path).expect_err("an unknown key must be rejected");
    let text = error.to_string();
    assert!(
        !text.contains("sup3r-s3cr3t"),
        "the error must not repeat the value: {text}"
    );
    assert!(text.contains("auth-token"), "{text}");
}

// --- traps the file can reach and the command line cannot -------------------

#[test]
fn an_empty_scope_list_is_rejected_rather_than_granting_everything() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), "[security]\nstatic-bearer-scopes = []\n");
    let mut args = <Args as clap::FromArgMatches>::from_arg_matches(
        &Args::command().get_matches_from(["mcpmem"]),
    )
    .expect("args");
    let matches = Args::command().get_matches_from(["mcpmem"]);
    let explicit = ExplicitArgs::from_matches(&matches);
    let error = FileConfig::load(&path)
        .expect("loads")
        .apply(&mut args, &explicit, &|_| true)
        .expect_err("an empty scope list must be rejected");
    assert!(error.to_string().contains("grants every scope"), "{error}");
}

// --- the shipped example ----------------------------------------------------

fn example_text() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/mcpmem.example.toml"))
        .expect("the example config must exist in the repository")
}

#[test]
fn the_shipped_example_changes_nothing_by_itself() {
    let file: FileConfig = toml::from_str(&example_text()).expect("the example config must parse");
    assert_eq!(
        file,
        FileConfig::default(),
        "every key in the example must be commented out, so the example is a safe starting point"
    );
}

/// The example is the documentation surface for 45 keys. Parsing it while every
/// key is commented out proves only that the seven section headers exist, so
/// this test uncomments every key and parses the result. A misspelled key or a
/// wrong enum spelling in the example fails here instead of failing an operator.
#[test]
fn every_key_documented_in_the_example_is_a_real_key() {
    let uncommented: String = example_text()
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("# ")
                && rest.contains(" = ")
                && rest.split(" = ").next().is_some_and(|key| {
                    !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                })
            {
                return Some(rest.to_string());
            }
            (!trimmed.starts_with('#') && !trimmed.is_empty()).then(|| line.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        uncommented.matches(" = ").count() >= 40,
        "the extractor found too few keys; it is no longer reading the example"
    );

    let file: FileConfig = toml::from_str(&uncommented).unwrap_or_else(|error| {
        panic!("a key documented in mcpmem.example.toml is not a real key: {error}")
    });

    // Every documented enum spelling must also resolve.
    let matches = Args::command().get_matches_from(["mcpmem"]);
    let explicit = ExplicitArgs::from_matches(&matches);
    let mut args = <Args as clap::FromArgMatches>::from_arg_matches(&matches).expect("args");
    file.apply(&mut args, &explicit, &|_| true)
        .expect("every value documented in the example must be accepted");
}

// --- the provider-publication gate ------------------------------------------

/// `main` publishes the provider registry only when the resolved settings are
/// not the default value. Bedrock names no URL and no key, so without the
/// `bedrock` flag a correct Bedrock deployment would resolve to the default
/// value, publish nothing, and hide `semantic_search`.
#[cfg(feature = "indexer")]
#[test]
fn a_bedrock_profile_produces_settings_that_the_publication_gate_accepts() {
    let file: FileConfig = toml::from_str(
        "[indexer]\nprovider = \"bedrock\"\nmodel = \"amazon.titan-embed-text-v2:0\"\ndimensions = 1024\n",
    )
    .expect("parses");
    let settings = mcpmem::config_file::indexer_settings(Some(&file)).expect("settings");

    assert!(settings.bedrock, "the profile kind must select Bedrock");
    assert_ne!(
        settings,
        mcpmem_indexer::ProviderSettings::default(),
        "the gate in main publishes only a non-default value"
    );
}

/// The mirror case, and the reason the flag exists at all: an Ollama profile
/// must not ask for Bedrock. Building the Bedrock provider resolves the AWS
/// credential chain, which fails on a host that has none.
#[cfg(feature = "indexer")]
#[test]
fn an_ollama_profile_never_asks_for_bedrock() {
    let file: FileConfig = toml::from_str(
        "[indexer]\nollama-url = \"http://127.0.0.1:11434\"\nprovider = \"ollama\"\nmodel = \"nomic-embed-text\"\ndimensions = 768\n",
    )
    .expect("parses");
    let settings = mcpmem::config_file::indexer_settings(Some(&file)).expect("settings");

    assert!(!settings.bedrock);
    assert_eq!(
        settings.ollama_url.as_deref(),
        Some("http://127.0.0.1:11434")
    );
}
