use std::{path::Path, process::Command};

fn cli(config: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_beynal"))
        .env("XDG_CONFIG_HOME", config)
        .env_remove("BEYNAL_CONTROL_SOCKET")
        .env_remove("BEYNAL_PROMPT_THEME")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn redirected_help_inside_beynal_is_short_plain_text() {
    let output = Command::new(env!("CARGO_BIN_EXE_beynal"))
        .env("BEYNAL_CONTROL_SOCKET", "/nonexistent")
        .env("TERM", "xterm-256color")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Open a new window") && help.contains("theme set NAME"));
    assert!(!help.contains('\x1b') && help.lines().count() < 12);
}

#[test]
fn cli_theme_and_invalid_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path();
    assert_eq!(cli(config, &["theme"]).stdout, b"minimal\n");
    assert_eq!(
        cli(config, &["theme", "list"]).stdout,
        b"minimal\ndeveloper\nsignature\n"
    );
    assert!(cli(config, &["theme", "set", "signature"]).status.success());
    assert_eq!(cli(config, &["theme"]).stdout, b"signature\n");
    for (args, error) in [
        (&["unknown"][..], "unknown command"),
        (&["theme", "set"][..], "usage: beynal theme"),
        (&["theme", "set", "unknown"][..], "unknown theme"),
        (&["open", "extra"][..], "takes no arguments"),
    ] {
        let out = cli(config, args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains(error));
    }
    assert_eq!(cli(config, &["theme"]).stdout, b"signature\n");
}
