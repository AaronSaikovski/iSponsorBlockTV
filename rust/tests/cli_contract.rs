use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_iSponsorBlockTV"))
}

#[test]
fn help_and_version_need_no_configuration_or_python() {
    for flag in ["--help", "--version"] {
        let result = binary().arg(flag).env("PATH", "").output().unwrap();
        assert!(result.status.success());
        let output = String::from_utf8(result.stdout).unwrap();
        assert!(output.contains(if flag == "--version" {
            "2.11.0"
        } else {
            "setup-cli"
        }));
        assert!(!output.contains("self update"));
    }
}

#[test]
fn piped_setup_saves_only_after_explicit_confirmation() {
    let data = tempfile::tempdir().unwrap();
    let mut child = binary()
        .args(["--data", data.path().to_str().unwrap(), "setup-cli"])
        .env("PATH", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Proxy, devices, key, categories, whitelist, minimum, reporting, mute,
    // skip, autoplay, URL, then explicit save.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"n\nn\nn\nsponsor hook\nn\nn\ny\nn\nn\ny\n\ny\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(data.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(
        json["skip_categories"],
        serde_json::json!(["sponsor", "hook"])
    );
    assert_eq!(json["skip_count_tracking"], true);
    assert_eq!(json["mute_ads"], false);
    assert_eq!(json["skip_ads"], false);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Config saved to"));
}

#[test]
fn eof_leaves_existing_configuration_untouched() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("config.json");
    let original = b"{\"future\":18446744073709551617}";
    fs::write(&path, original).unwrap();
    let output = binary()
        .args(["--data", data.path().to_str().unwrap(), "setup-cli"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn malformed_configuration_is_not_replaced() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("config.json");
    fs::write(&path, b"{invalid").unwrap();
    let result = binary()
        .args(["--data", data.path().to_str().unwrap(), "setup-cli"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(fs::read(path).unwrap(), b"{invalid");
}

#[test]
fn invalid_arguments_exit_two() {
    assert_eq!(
        binary()
            .arg("--not-an-option")
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn legacy_precedence_and_noninteractive_terminal_are_preserved() {
    let data = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["--setup"],
        vec!["--setup-cli", "--setup"],
        vec!["setup"],
    ] {
        let output = binary()
            .arg("--data")
            .arg(data.path())
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("Use setup-cli in a non-interactive terminal")
        );
    }
    for arguments in [
        vec!["--setup-cli"],
        vec!["--setup", "--setup-cli", "setup-cli"],
    ] {
        let output = binary()
            .arg("--data")
            .arg(data.path())
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(130));
    }
    assert!(!data.path().join("config.json").exists());
    let output = binary()
        .arg("--data")
        .arg(data.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "default command validates service configuration"
    );
    assert_eq!(
        binary()
            .env("PYAPP", "1")
            .arg("self")
            .arg("update")
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}
