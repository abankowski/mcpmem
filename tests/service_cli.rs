use std::process::Command;

#[test]
fn service_commands_are_discoverable_without_a_config() {
    for (args, expected) in [
        (vec!["--help"], "service"),
        (vec!["service", "--help"], "uninstall"),
        (vec!["service", "--help"], "status"),
        (vec!["service", "install", "--help"], "--config"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mcpmem"))
            .args(args)
            .env("MCP_MEMORY_CONFIG", "/missing/mcpmem/config")
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn service_commands_use_the_user_manager_and_preserve_server_files() {
    use std::{fs, os::unix::fs::PermissionsExt};

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let ctl = root.join("systemctl");
    fs::write(&ctl, r#"#!/bin/sh
echo "$*" >> "$LOG"
if [ "$2" = show ]; then
    if [ -f missing-unit ] || [ ! -f "$XDG_CONFIG_HOME/systemd/user/mcpmem.service" ]; then
        echo LoadState=not-found
        exit 4
    fi
    if [ -f unavailable ]; then
        echo "Failed to connect to bus" >&2
        exit 1
    fi
    printf '%s\n' 'LoadState=loaded' 'UnitFileState=enabled' 'ActiveState=active' 'SubState=running' 'NeedDaemonReload=no' "FragmentPath=$XDG_CONFIG_HOME/systemd/user/mcpmem.service"
fi
"#).unwrap();
    fs::set_permissions(&ctl, fs::Permissions::from_mode(0o755)).unwrap();
    let log = root.join("commands");
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mcpmem"))
            .args(args)
            .current_dir(root)
            .env("PATH", root)
            .env("XDG_CONFIG_HOME", root)
            .env("LOG", &log)
            .env("MCP_MEMORY_CONFIG", root.join("missing"))
            .output()
            .unwrap()
    };
    let unit = root.join("systemd/user/mcpmem.service");
    let config = root.join("settings.toml");
    let install = ["service", "install", "--config", "settings.toml"];
    let status = ["service", "status"];
    let result = invoke(&status);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("not installed"));
    assert!(!root.join("systemd").exists());
    fs::write(&config, "").unwrap();
    let result = invoke(&install);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("stdin"));
    assert!(!unit.exists());
    fs::write(
        &config,
        "[server]\ntransport = 'http'\n[security]\nauth-token-file = 'token'\n",
    )
    .unwrap();
    fs::write(root.join("token"), "test-secret-value").unwrap();
    let result = invoke(&install);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = fs::read_to_string(&unit).unwrap();
    assert!(text.contains(config.to_str().unwrap()));
    assert!(!text.contains("test-secret-value"));
    let result = invoke(&status);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("enabled and running"));
    assert_eq!(fs::read_to_string(&unit).unwrap(), text);
    for (flag, expected) in [
        ("missing-unit", "LoadState=not-found"),
        ("unavailable", "Failed to connect to bus"),
    ] {
        fs::write(root.join(flag), "").unwrap();
        let result = invoke(&status);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(expected));
        fs::remove_file(root.join(flag)).unwrap();
    }
    let result = invoke(&["service", "uninstall"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!unit.exists());
    assert!(config.exists());
    assert!(!invoke(&status).status.success());
    assert_eq!(
        fs::read_to_string(root.join("token")).unwrap(),
        "test-secret-value"
    );
    assert_eq!(
        fs::read_to_string(log).unwrap().lines().collect::<Vec<_>>(),
        [
            "--user show --all --property LoadState,FragmentPath mcpmem.service",
            "--user daemon-reload",
            "--user show --all --property LoadState,FragmentPath mcpmem.service",
            "--user enable mcpmem.service",
            "--user show --all --property LoadState,FragmentPath mcpmem.service",
            "--user restart mcpmem.service",
            "--user show --all --property LoadState,UnitFileState,ActiveState,SubState,FragmentPath,NeedDaemonReload mcpmem.service",
            "--user show --all --property LoadState,UnitFileState,ActiveState,SubState,FragmentPath,NeedDaemonReload mcpmem.service",
            "--user show --all --property LoadState,UnitFileState,ActiveState,SubState,FragmentPath,NeedDaemonReload mcpmem.service",
            "--user show --all --property LoadState,FragmentPath mcpmem.service",
            "--user daemon-reload",
            "--user show --all --property LoadState,FragmentPath mcpmem.service",
            "--user --no-reload disable --now mcpmem.service",
            "--user daemon-reload",
        ]
    );
}
