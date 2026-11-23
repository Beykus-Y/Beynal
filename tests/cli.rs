// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{path::Path, process::Command};

fn cli(config: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_beyrix"))
        .env("XDG_CONFIG_HOME", config)
        .env_remove("BEYRIX_CONTROL_SOCKET")
        .env_remove("BEYRIX_PROMPT_THEME")
        .env_remove("BEYNAL_CONTROL_SOCKET")
        .env_remove("BEYNAL_PROMPT_THEME")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn redirected_help_inside_beyrix_is_short_plain_text() {
    let output = Command::new(env!("CARGO_BIN_EXE_beyrix"))
        .env("BEYRIX_CONTROL_SOCKET", "/nonexistent")
        .env("TERM", "xterm-256color")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Open a new window") && help.contains("theme set NAME"));
    assert!(!help.contains('\x1b') && help.lines().count() < 16);
}

#[test]
fn cli_renderer_commands_persist_choice() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path();
    assert_eq!(cli(config, &["renderer"]).stdout, b"auto\n");
    assert_eq!(
        cli(config, &["renderer", "list"]).stdout,
        b"auto\nopengl\nvulkan\nsoftware\n"
    );
    assert!(cli(config, &["renderer", "set", "opengl"]).status.success());
    assert_eq!(cli(config, &["renderer"]).stdout, b"opengl\n");
    let text = std::fs::read_to_string(config.join("beyrix/config.toml")).unwrap();
    assert!(text.contains("opengl"));
    for (args, error) in [
        (&["renderer", "set"][..], "usage: beyrix renderer"),
        (&["renderer", "set", "metal"][..], "unknown renderer"),
        (&["renderer", "switch"][..], "usage: beyrix renderer"),
    ] {
        let out = cli(config, args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains(error));
    }
    assert_eq!(cli(config, &["renderer"]).stdout, b"opengl\n");
}

#[test]
fn cli_layout_commands_persist_choice() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path();
    assert_eq!(cli(config, &["layout"]).stdout, b"as-is\n");
    assert_eq!(
        cli(config, &["layout", "list"]).stdout,
        b"as-is\nbottom\nbottom-input\n"
    );
    assert!(
        cli(config, &["layout", "set", "bottom-input"])
            .status
            .success()
    );
    assert_eq!(cli(config, &["layout"]).stdout, b"bottom-input\n");
    assert!(cli(config, &["layout", "set", "bottom"]).status.success());
    assert_eq!(cli(config, &["layout"]).stdout, b"bottom\n");
    for (args, error) in [
        (&["layout", "set"][..], "usage: beyrix layout"),
        (&["layout", "set", "middle"][..], "unknown layout"),
        (&["layout", "switch"][..], "usage: beyrix layout"),
    ] {
        let out = cli(config, args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains(error));
    }
    assert_eq!(cli(config, &["layout"]).stdout, b"bottom\n");
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
        (&["theme", "set"][..], "usage: beyrix theme"),
        (&["theme", "set", "unknown"][..], "unknown theme"),
        (&["open", "extra"][..], "takes no arguments"),
    ] {
        let out = cli(config, args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains(error));
    }
    assert_eq!(cli(config, &["theme"]).stdout, b"signature\n");
}

#[test]
fn cli_migrates_legacy_config_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path();
    let legacy = config.join("beynal/config.toml");
    let current = config.join("beyrix/config.toml");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let original = "theme = 'developer'\nlayout = 'bottom'\nrenderer = 'software'\n";
    std::fs::write(&legacy, original).unwrap();

    assert_eq!(cli(config, &["theme"]).stdout, b"developer\n");
    assert_eq!(cli(config, &["layout"]).stdout, b"bottom\n");
    assert_eq!(cli(config, &["renderer"]).stdout, b"software\n");
    assert_eq!(std::fs::read_to_string(&current).unwrap(), original);
    assert_eq!(std::fs::read_to_string(&legacy).unwrap(), original);

    assert!(cli(config, &["theme", "set", "signature"]).status.success());
    assert!(
        std::fs::read_to_string(&current)
            .unwrap()
            .contains("signature")
    );
    assert_eq!(std::fs::read_to_string(&legacy).unwrap(), original);

    let other = tempfile::tempdir().unwrap();
    let other_legacy = other.path().join("beynal/config.toml");
    let other_current = other.path().join("beyrix/config.toml");
    std::fs::create_dir_all(other_legacy.parent().unwrap()).unwrap();
    std::fs::create_dir_all(other_current.parent().unwrap()).unwrap();
    std::fs::write(&other_legacy, "theme = 'developer'\n").unwrap();
    std::fs::write(&other_current, "theme = 'minimal'\n").unwrap();
    assert_eq!(cli(other.path(), &["theme"]).stdout, b"minimal\n");
    assert_eq!(
        std::fs::read_to_string(&other_current).unwrap(),
        "theme = 'minimal'\n"
    );
}

#[test]
fn cli_edit_validates_arguments_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path();
    for args in [&["edit"][..], &["edit", "one", "two"][..]] {
        let out = cli(config, args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("usage: beyrix edit FILE"));
    }
    let invalid = dir.path().join("invalid.txt");
    std::fs::write(&invalid, b"\xff\xfe").unwrap();
    let out = cli(config, &["edit", invalid.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not valid UTF-8"));

    let folder = dir.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    let out = cli(config, &["edit", folder.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));
}

#[test]
fn bedit_launcher_and_symlink_dispatch_identically() {
    let dir = tempfile::tempdir().unwrap();
    let alias = dir.path().join("bedit");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_beyrix"), &alias).unwrap();
    for path in [std::path::PathBuf::from(env!("CARGO_BIN_EXE_bedit")), alias] {
        let help = Command::new(&path)
            .arg("--help")
            .env("BEYRIX_CONTROL_SOCKET", "/unavailable")
            .output()
            .unwrap();
        assert!(help.status.success());
        assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: bedit"));
        assert!(
            !Command::new(path)
                .arg("--bad-option")
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}

#[test]
fn bedit_sends_project_and_multiple_native_paths_to_current_window() {
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStringExt, net::UnixDatagram},
    };
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    let server = UnixDatagram::bind(&socket).unwrap();
    server
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    let raw = OsString::from_vec(b"native-\xff.rs".to_vec());
    for args in [
        vec![],
        vec![OsString::from(".")],
        vec![
            OsString::from("."),
            raw.clone(),
            OsString::from("second.rs"),
        ],
        vec![raw.clone()],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_bedit"))
            .current_dir(dir.path())
            .env("BEYRIX_CONTROL_SOCKET", &socket)
            .args(&args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        let mut packet = [0; 4096];
        let bytes = server.recv(&mut packet).unwrap();
        let mut expected = b"bedit:".to_vec();
        if args.is_empty() || args[0] == "." {
            expected.extend_from_slice(dir.path().as_os_str().as_encoded_bytes());
        }
        for arg in args.into_iter().filter(|arg| arg != ".") {
            expected.push(0);
            expected.extend_from_slice(dir.path().join(arg).as_os_str().as_encoded_bytes());
        }
        assert_eq!(&packet[..bytes], expected);
    }
}
