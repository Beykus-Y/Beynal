// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Bash expands parameter values only once, so directory and Git output are never shell code.
pub fn script() -> &'static str {
    include_str!("prompt_developer.sh")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompt_reads_changed_theme_without_restarting_bash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("theme");
        std::fs::write(&path, "minimal\nfalse\n").unwrap();
        let code = format!(
            r#"{} 
            __beyrix_prompt; printf '%s\n' "$PS1"
            printf 'signature\nfalse\n' > "$BEYRIX_PROMPT_THEME"
            __beyrix_prompt; printf '%s\n' "$PS1"
            printf 'signature\ntrue\n' > "$BEYRIX_PROMPT_THEME"
            __beyrix_prompt; printf '%s\n' "$PS1"
            "#,
            script()
        );
        let out = std::process::Command::new("bash")
            .args(["-c", &code])
            .env("HOME", dir.path())
            .env("BEYRIX_PROMPT_THEME", path)
            .output()
            .unwrap();
        let prompts = String::from_utf8(out.stdout).unwrap();
        let mut lines = prompts.lines();
        assert!(!lines.next().unwrap().contains('◆'));
        assert!(lines.next().unwrap().contains(r"\n❯"));
        let compact = lines.next().unwrap();
        assert!(compact.contains('◆') && !compact.contains(r"\n"));
    }
    #[test]
    fn command_available_after_bashrc_replaces_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::os::unix::fs::symlink("/bin/true", bin.join("beyrix")).unwrap();
        std::fs::write(dir.path().join(".bashrc"), "PATH=/bin\n").unwrap();
        let code = format!("{}\ncommand -v beyrix; beyrix theme list", script());
        let output = std::process::Command::new("bash")
            .args(["-c", &code])
            .env("HOME", dir.path())
            .env("BEYRIX_COMMAND_DIR", &bin)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("beyrix"));
    }
    #[test]
    fn no_git_indicator_outside_repository() {
        let dir = tempfile::tempdir().unwrap();
        let code = format!(
            "{}\n__beyrix_prompt; printf '%s' \"${{__beyrix_git:-${{__beyrix_branch:-}}}}\"",
            script()
        );
        let output = std::process::Command::new("bash")
            .args(["-c", &code])
            .env("HOME", dir.path())
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.stdout.is_empty());
    }
    #[test]
    fn prompts_keep_variables_literal_and_detect_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("test $(touch INJECTED)");
        std::fs::create_dir(&root).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.join("modified"), "").unwrap();
        let theme = dir.path().join("theme");
        for name in ["minimal", "developer", "signature"] {
            std::fs::write(&theme, format!("{name}\nfalse\n")).unwrap();
            let run = format!(
                "{}\nfalse; __beyrix_prompt; printf '%s\n%s\n%s' \"$PS1\" \"${{__beyrix_git:-${{__beyrix_branch:-}}}}\" \"${{__beyrix_status:-0}}\"",
                script()
            );
            let output = std::process::Command::new("bash")
                .args(["-c", &run])
                .env("HOME", dir.path())
                .current_dir(&root)
                .env("BEYRIX_PROMPT_THEME", &theme)
                .output()
                .unwrap();
            assert!(output.status.success());
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains("${__beyrix_path}"));
            assert!(text.contains("master") || text.contains("main"));
            assert!(!text.contains("No commits yet"));
            if name != "minimal" {
                assert!(text.ends_with("exit 1"));
                assert!(text.contains(" *"));
            } else {
                assert!(!text.contains("exit 1"));
                assert!(!text.contains(" *"));
            }
            assert!(!root.join("INJECTED").exists());
        }
    }
}
