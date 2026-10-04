//! The `--ui` switch and the `[server] ui` key: the runtime default, the
//! file-to-args merge, and the startup refusal of a build without the `ui`
//! feature.
//!
//! The absence of the flag never refuses: the runtime default is true only
//! when the feature is compiled. The two tests that need the `ui` feature
//! are gated on it. The refusal test compiles only without it, mirroring
//! the `oauth` refusal in `oauth_config.rs`.

use mcpmem::Args;
use mcpmem::config::Config;

#[cfg(not(feature = "ui"))]
use clap::CommandFactory;

/// Runs the real startup merge path: `main` calls exactly this function, so
/// a regression in the wiring fails here too.
fn merge(argv: &[&str], text: &str) -> Args {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mcpmem.toml");
    std::fs::write(&path, text).expect("write config");
    let mut full: Vec<std::ffi::OsString> = vec!["mcpmem".into()];
    full.extend(argv.iter().map(std::ffi::OsString::from));
    full.push("--config".into());
    full.push(path.into_os_string());
    mcpmem::config_file::resolve(full).expect("resolve").0
}

/// Parses the command line without a config file. The config-file merge has
/// no part in these refusal tests.
#[cfg(not(feature = "ui"))]
fn args(argv: &[&str]) -> Args {
    let mut full: Vec<std::ffi::OsString> = vec!["mcpmem".into()];
    full.extend(argv.iter().map(std::ffi::OsString::from));
    let matches = Args::command().get_matches_from(full);
    <Args as clap::FromArgMatches>::from_arg_matches(&matches).expect("parse args")
}

/// With the `ui` feature, the runtime default enables the UI. The no-feature
/// build never enables it, and the refusal test below owns that behavior.
#[cfg(feature = "ui")]
#[test]
fn the_ui_defaults_on_in_a_build_with_the_feature() {
    assert!(Config::default().ui_enabled);

    let no_flag = merge(&[], "");
    assert!(
        Config::from_args(&no_flag).expect("config").ui_enabled,
        "the runtime default is true when the feature is compiled"
    );
}

/// The file turns the UI off; an explicit flag turns it back on. The file
/// wins when the command line leaves the flag alone, and the flag wins over
/// the file.
#[cfg(feature = "ui")]
#[test]
fn the_file_turns_the_ui_off_and_the_cli_turns_it_back_on() {
    let from_file = merge(&[], "[server]\nui = false\n");
    assert_eq!(from_file.ui, Some(false));
    assert!(!Config::from_args(&from_file).expect("config").ui_enabled);

    let from_cli = merge(&["--ui", "true"], "[server]\nui = false\n");
    assert_eq!(from_cli.ui, Some(true));
    assert!(Config::from_args(&from_cli).expect("config").ui_enabled);
}

/// A build without the `ui` feature has no `/ui` route. The absence of the
/// flag never refuses and leaves the UI disabled; an explicit enable — a
/// flag or a file true — refuses startup with a named error.
#[cfg(not(feature = "ui"))]
#[test]
fn an_explicit_ui_true_is_refused_by_a_build_without_the_feature() {
    let no_flag = Config::from_args(&args(&[])).expect("no enable must start");
    assert!(!no_flag.ui_enabled, "the default is off without the feature");

    let error = Config::from_args(&args(&["--ui", "true"]))
        .expect_err("an explicit flag enable must be refused")
        .to_string();
    assert!(
        error.contains("ui") && error.contains("feature"),
        "the refusal must name the feature this build lacks: {error}"
    );

    let from_file = merge(&[], "[server]\nui = true\n");
    let error = Config::from_args(&from_file)
        .expect_err("an explicit file enable must be refused")
        .to_string();
    assert!(
        error.contains("ui") && error.contains("feature"),
        "the refusal must name the feature this build lacks: {error}"
    );
}