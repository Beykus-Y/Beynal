// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headless terminal conformance, differential, chunked-input, DoS/OOM-resilience,
//! terminfo, X-Ray unified lifecycle, graphics damage tracking, and real interactive
//! PTY TUI integration test harness.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::keyboard::{KeyEventType, Keyboard, alternate_scroll_sequence};
use crate::pty::Pty;
use crate::terminal::{
    CellAttrs, SUPPORTED_KITTY_KEYBOARD_FLAGS, ShellEvent, Terminal, TerminalInputModes,
    UnderlineStyle,
};
use crate::terminal_graphics::base64_encode;

/// Headless PTY + Terminal test harness for driving real CLI/TUI applications.
struct HeadlessPtyHarness {
    pty: Pty,
    terminal: Terminal,
}

impl HeadlessPtyHarness {
    fn spawn(cols: u16, rows: u16) -> Self {
        let pty = Pty::spawn_with_size(Some((cols, rows))).expect("spawn PTY");
        let fd = pty.master.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0);
        Self {
            pty,
            terminal: Terminal::new(cols as usize, rows as usize),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.pty
            .resize_with_pixels(cols, rows, cols * 9, rows * 18)
            .expect("resize PTY");
        self.terminal.resize(cols as usize, rows as usize);
    }

    fn send(&self, bytes: &[u8]) {
        let mut offset = 0;
        let deadline = Instant::now() + Duration::from_secs(3);
        while offset < bytes.len() && Instant::now() < deadline {
            let n = unsafe {
                libc::write(
                    self.pty.master.as_raw_fd(),
                    bytes[offset..].as_ptr().cast(),
                    bytes.len() - offset,
                )
            };
            if n > 0 {
                offset += n as usize;
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(offset, bytes.len(), "failed to write full input to PTY");
    }

    fn pump(&mut self) -> usize {
        let mut buf = [0u8; 16384];
        let mut total = 0;
        loop {
            let n = unsafe {
                libc::read(
                    self.pty.master.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                )
            };
            if n > 0 {
                let n = n as usize;
                total += n;
                self.terminal.process(&buf[..n]);
                let responses = self.terminal.take_responses();
                if !responses.is_empty() {
                    self.send(&responses);
                }
            } else {
                break;
            }
        }
        total
    }

    fn screen_text(&self) -> String {
        (0..self.terminal.screen().rows)
            .map(|y| self.terminal.screen().line(y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Wait until a line on screen *equals* or *starts with* `needle` (trimmed),
    /// preventing false positives from matching echoed shell input commands.
    fn wait_for_output_line(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump();
            if (0..self.terminal.screen().rows)
                .any(|y| self.terminal.screen().line(y).trim() == needle)
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn wait_for_alt_screen(&mut self, expected: bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump();
            if self.terminal.screen().is_alternate_screen() == expected {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

impl Drop for HeadlessPtyHarness {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pty.child_pid, libc::SIGKILL);
            libc::waitpid(self.pty.child_pid, std::ptr::null_mut(), 0);
        }
    }
}

#[test]
fn chunked_input_invariance_matches_single_shot() {
    let rgba = [10u8, 20, 30, 255, 40, 50, 60, 255];
    let payload = format!(
        "\x1b[1;3;4:3;38;2;255;128;0mHello \x1b]8;;https://beyrix.dev\x07Link\x1b]8;;\x07 \
         \x1b(0lqqk\x1b(B 界 Привет e\u{301} \x1b[0m\r\n\
         \x1b]133;A\x07\x1b]133;D;0\x07\
         \x1b[6n\x1b[>c\x1bP$qm\x1b\\\
         \x1b_Ga=T,f=32,s=2,v=1,i=7;{}\x1b\\",
        base64_encode(&rgba)
    );
    let bytes = payload.as_bytes();

    let mut whole = Terminal::new(60, 10);
    whole.process(bytes);
    let whole_resp = whole.take_responses();

    for chunk_size in [1usize, 2, 3, 7, 13] {
        let mut chunked = Terminal::new(60, 10);
        for chunk in bytes.chunks(chunk_size) {
            chunked.process(chunk);
        }
        let chunked_resp = chunked.take_responses();
        assert_eq!(
            whole_resp, chunked_resp,
            "responses diverged at chunk={chunk_size}"
        );
        for y in 0..10 {
            assert_eq!(
                whole.screen().line(y),
                chunked.screen().line(y),
                "row {y} diverged at chunk={chunk_size}"
            );
        }
        assert_eq!(
            whole.screen().graphics.placements.len(),
            chunked.screen().graphics.placements.len()
        );
    }
}

#[test]
fn malformed_and_oversized_sequences_never_panic_or_oom() {
    let mut t = Terminal::new(40, 10);
    let cases: &[&[u8]] = &[
        b"\x1b[999999999;999999999H",
        b"\x1b[?99999999h\x1b[?99999999l",
        b"\x1b[38;2;999;999;999m",
        b"\x1b]8;;\x00\x01\x02bad\x07",
        b"\x1b]52;c;!!!not-base64!!!\x07",
        b"\x1bP+qZZZZ\x1b\\",
        b"\x1b_Ga=T,f=32,s=99999,v=99999;AAAA\x1b\\",
        b"\x1b_Ga=T,f=32,s=1,v=1,r=99999999,c=99999999;AAAA\x1b\\",
        b"\x1b_Ga=T,t=f;/etc/shadow\x1b\\",
        &[0xff, 0xfe, 0x80, 0xc0, 0x1b, b'[', 0xff],
    ];
    for c in cases {
        t.process(c);
    }
    // Reset parser back to ground before oversized APC test.
    t.process(b"\x1b\\");
    // Oversized APC (>16 MiB) must be dropped gracefully without panicking.
    t.process(b"\x1b_Ga=T,f=32;");
    let zeros = vec![b'A'; 64 * 1024];
    for _ in 0..270 {
        t.process(&zeros);
    }
    t.process(b"\x1b\\after");
    assert!(t.screen().line(t.screen().cursor_y).contains("after"));
}

#[test]
fn graphics_revision_tracks_add_delete_sixel_and_alt_screen_for_damage() {
    let mut t = Terminal::new(40, 10);
    let rev0 = t.screen().graphics.revision;

    // 1. Add Kitty Graphics image -> revision increments.
    let rgba = [255u8, 0, 0, 255];
    t.process(format!("\x1b_Ga=T,f=32,s=1,v=1,i=5;{}\x1b\\", base64_encode(&rgba)).as_bytes());
    let rev1 = t.screen().graphics.revision;
    assert!(
        rev1 > rev0,
        "adding Kitty image must bump graphics revision"
    );
    assert_eq!(t.screen().graphics.placements.len(), 1);

    // 2. Delete Kitty Graphics image without changing text -> revision increments again!
    t.process(b"\x1b_Ga=d,d=a\x1b\\");
    let rev2 = t.screen().graphics.revision;
    assert!(
        rev2 > rev1,
        "deleting Kitty image must bump graphics revision"
    );
    assert!(t.screen().graphics.placements.is_empty());

    // 3. SIXEL image on alternate screen -> cleared when leaving alt screen -> bumps revision!
    t.process(b"\x1b[?1049h\x1bPq#1;2;100;0;0#1!4~\x1b\\");
    let rev3 = t.screen().graphics.revision;
    assert!(rev3 > rev2, "SIXEL image must bump graphics revision");
    assert_eq!(t.screen().graphics.placements.len(), 1);

    t.process(b"\x1b[?1049l");
    let rev4 = t.screen().graphics.revision;
    assert!(
        rev4 > rev3,
        "leaving alt screen must clear alt images and bump revision"
    );
    assert!(t.screen().graphics.placements.is_empty());
}

#[test]
fn keyboard_protocols_decckm_deckpam_debkm_modify_other_keys_and_kitty() {
    let map = br#"xkb_keymap {
        xkb_keycodes { include "evdev+aliases(qwerty)" };
        xkb_types { include "complete" };
        xkb_compatibility { include "complete" };
        xkb_symbols { include "pc+us+ru:2" };
    };"#;
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(map).unwrap();
    let mut kb = Keyboard::new().unwrap();
    kb.keymap(file.into(), map.len() as u32).unwrap();

    // DECCKM: Up arrow (evdev 103) -> ESC [ A (normal) vs ESC O A (app cursor)
    kb.modifiers(0, 0, 0, 0);
    let mut modes = TerminalInputModes::default();
    assert_eq!(
        kb.key_with_modes(103, modes, KeyEventType::Press),
        b"\x1b[A"
    );
    modes.app_cursor_keys = true;
    assert_eq!(
        kb.key_with_modes(103, modes, KeyEventType::Press),
        b"\x1bOA"
    );

    // DECBKM: Backspace (evdev 14) -> 0x7f vs 0x08
    modes.backarrow_key = true;
    assert_eq!(kb.key_with_modes(14, modes, KeyEventType::Press), b"\x08");
    modes.backarrow_key = false;
    assert_eq!(kb.key_with_modes(14, modes, KeyEventType::Press), b"\x7f");

    // Alternate scroll sequence respects DECCKM
    assert_eq!(alternate_scroll_sequence(true, false), b"\x1b[A");
    assert_eq!(alternate_scroll_sequence(true, true), b"\x1bOA");
    assert_eq!(alternate_scroll_sequence(false, true), b"\x1bOB");

    // modifyOtherKeys=2: Ctrl+Enter (evdev 28) -> CSI 27;5;13~
    modes = TerminalInputModes {
        modify_other_keys: 2,
        ..Default::default()
    };
    kb.modifiers(1 << 2, 0, 0, 0);
    assert_eq!(
        kb.key_with_modes(28, modes, KeyEventType::Press),
        b"\x1b[27;5;13~"
    );

    // Kitty Keyboard Protocol: verify unsupported bit 4 (0x04) is never advertised.
    assert_eq!(SUPPORTED_KITTY_KEYBOARD_FLAGS & 0x04, 0);
    let mut term = Terminal::new(80, 24);
    term.process(b"\x1b[>31u\x1b[?u");
    assert_eq!(
        term.take_responses(),
        format!("\x1b[?{}u", SUPPORTED_KITTY_KEYBOARD_FLAGS).as_bytes()
    );

    // Kitty flags=3 (disambiguate + report events): Press / Repeat / Release
    modes = TerminalInputModes {
        kitty_flags: 3,
        ..Default::default()
    };
    kb.modifiers(1 << 2, 0, 0, 0); // Ctrl+A (evdev 30)
    assert_eq!(
        kb.key_with_modes(30, modes, KeyEventType::Press),
        b"\x1b[97;5u"
    );
    assert_eq!(
        kb.key_with_modes(30, modes, KeyEventType::Repeat),
        b"\x1b[97;5:2u"
    );
    assert_eq!(
        kb.key_with_modes(30, modes, KeyEventType::Release),
        b"\x1b[97;5:3u"
    );
}

#[test]
fn xray_unified_osc133_lifecycle_has_no_duplicates_and_tracks_exit_codes() {
    let cfg = crate::config::XRayConfig {
        enabled: true,
        poll_interval_ms: 50,
        ..Default::default()
    };
    let mut xray = crate::xray::XRayEngine::new(cfg);
    let bus = crate::events::EventBus::new();
    xray.ensure_started(std::process::id() as i32, None, bus);

    // 1. Fast builtin command (exit 0)
    xray.notify_shell_event(ShellEvent::CommandExecuted { abs_row: 0 });
    std::thread::sleep(Duration::from_millis(15));
    xray.notify_shell_event(ShellEvent::CommandFinished {
        abs_row: 1,
        exit_code: Some(0),
    });
    // 2. Non-zero command returning 130 (e.g. `bash -c 'exit 130'`):
    // must record exit_code=Some(130) and signal=None (never guess SIGINT from $? > 128!).
    xray.notify_shell_event(ShellEvent::CommandExecuted { abs_row: 2 });
    xray.notify_shell_event(ShellEvent::CommandFinished {
        abs_row: 3,
        exit_code: Some(130),
    });

    std::thread::sleep(Duration::from_millis(140));
    let snap = xray.snapshot();
    assert_eq!(
        snap.reports.len(),
        2,
        "must emit exactly one report per command with zero duplicates"
    );
    assert_eq!(snap.reports[0].exit_code, Some(0));
    assert_eq!(snap.reports[0].signal, None);
    assert!(
        snap.reports[0].duration_ms >= 10,
        "arrival-timestamped duration must reflect actual elapsed time (got {} ms)",
        snap.reports[0].duration_ms
    );
    assert_eq!(snap.reports[1].exit_code, Some(130));
    assert_eq!(
        snap.reports[1].signal, None,
        "must not fabricate a signal from exit_code > 128"
    );
}

#[test]
fn scrollback_search_and_unicode_vs16_zwj_and_bell() {
    let mut t = Terminal::new(30, 4);
    t.process("alpha\r\nBETA\r\ngamma\r\nalpha two\x07".as_bytes());
    assert!(t.screen_mut().take_bell());
    assert!(!t.screen_mut().take_bell());

    let insensitive = t.screen().search_scrollback("alpha", false);
    assert_eq!(insensitive.len(), 2);
    let sensitive = t.screen().search_scrollback("beta", true);
    assert!(sensitive.is_empty());
    assert_eq!(t.screen().search_scrollback("beta", false).len(), 1);

    // VS16 widens narrow symbol when space allows; ZWJ attaches without advancing cursor.
    let mut u = Terminal::new(10, 2);
    u.process("\u{2764}\u{fe0f}A".as_bytes());
    assert!(u.screen().get(0, 0).unwrap().wide);
    assert!(u.screen().get(1, 0).unwrap().continuation);
    assert_eq!(u.screen().get(2, 0).unwrap().character, 'A');

    let mut z = Terminal::new(10, 2);
    z.process("\u{1f468}\u{200d}\u{1f4bb}X".as_bytes());
    assert_eq!(z.screen().get(2, 0).unwrap().character, 'X');
}

#[test]
fn terminfo_file_compiles_with_tic_and_passes_infocmp_tput() {
    if Command::new("tic").arg("-V").output().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new("tic")
        .args(["-x", "-o"])
        .arg(dir.path())
        .arg("packaging/beyrix.terminfo")
        .status()
        .expect("run tic");
    assert!(status.success(), "tic failed to compile beyrix.terminfo");

    let info = Command::new("infocmp")
        .env("TERMINFO", dir.path())
        .args(["-x", "beyrix"])
        .output()
        .expect("run infocmp");
    assert!(info.status.success());
    let text = String::from_utf8_lossy(&info.stdout);
    assert!(text.contains("colors#0x100") || text.contains("colors#256"));
    assert!(text.contains("Smulx="));

    let tput = Command::new("tput")
        .env("TERMINFO", dir.path())
        .args(["-T", "beyrix", "colors"])
        .output()
        .expect("run tput");
    assert!(tput.status.success());
    assert_eq!(String::from_utf8_lossy(&tput.stdout).trim(), "256");
}

#[test]
fn pty_env_declares_xterm_256color_truecolor_and_beyrix_identity() {
    let mut h = HeadlessPtyHarness::spawn(80, 24);
    h.send(
        b"printf 'ENV:%s|%s|%s|%s\\n' \"$TERM\" \"$COLORTERM\" \"$TERM_PROGRAM\" \"$(tput colors)\"\n",
    );
    assert!(
        h.wait_for_output_line(
            "ENV:xterm-256color|truecolor|Beyrix|256",
            Duration::from_secs(4)
        ),
        "unexpected screen:\n{}",
        h.screen_text()
    );
}

#[test]
fn real_interactive_tui_apps_less_nano_btop_and_tmux_work_end_to_end() {
    // 1. Interactive `less` (split marker prevents echo false positive)
    if Command::new("which")
        .arg("less")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let mut h = HeadlessPtyHarness::spawn(80, 20);
        h.send(b"printf 'ROW_A\\nROW_B\\n' | less; printf 'LESS_%s\\n' EXITED\n");
        assert!(h.wait_for_alt_screen(true, Duration::from_secs(4)));
        h.send(b"q");
        assert!(
            h.wait_for_output_line("LESS_EXITED", Duration::from_secs(4)),
            "less did not exit cleanly:\n{}",
            h.screen_text()
        );
        assert!(!h.terminal.screen().is_alternate_screen());
    }

    // 2. Interactive `tmux` (attached session, command execution inside pane, resize, clean exit)
    if Command::new("which")
        .arg("tmux")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let sock_dir = tempfile::tempdir().unwrap();
        let sock_path = sock_dir.path().join("tmux.sock");
        let mut h = HeadlessPtyHarness::spawn(80, 24);
        h.send(
            format!(
                "tmux -S '{}' -f /dev/null new-session; printf 'TMUX_%s\\n' RETURNED\n",
                sock_path.display()
            )
            .as_bytes(),
        );
        assert!(
            h.wait_for_alt_screen(true, Duration::from_secs(5)),
            "interactive tmux did not enter alternate screen:\n{}",
            h.screen_text()
        );
        // Resize while inside attached tmux session
        h.resize(90, 26);
        std::thread::sleep(Duration::from_millis(120));
        // Execute command inside interactive tmux pane (echo-proof split marker)
        h.send(b"printf 'INSIDE_%s\\n' TMUX_PANE\n");
        assert!(
            h.wait_for_output_line("INSIDE_TMUX_PANE", Duration::from_secs(5)),
            "interactive tmux pane did not render command output:\n{}",
            h.screen_text()
        );
        // Exit the shell inside tmux -> closes tmux cleanly and returns to outer shell
        h.send(b"exit\n");
        assert!(
            h.wait_for_output_line("TMUX_RETURNED", Duration::from_secs(5)),
            "interactive tmux did not return to outer shell:\n{}",
            h.screen_text()
        );
        assert!(!h.terminal.screen().is_alternate_screen());
    }

    // 3. Interactive `btop` (launch -> MUST enter alt screen -> resize -> quit 'q' -> return to shell)
    if Command::new("which")
        .arg("btop")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let mut h = HeadlessPtyHarness::spawn(100, 30);
        h.send(b"LC_ALL=C.UTF-8 btop; printf 'BTOP_%s\\n' CLEAN\n");
        assert!(
            h.wait_for_alt_screen(true, Duration::from_secs(5)),
            "btop did not enter alternate screen:\n{}",
            h.screen_text()
        );
        h.resize(110, 32);
        std::thread::sleep(Duration::from_millis(150));
        h.send(b"q");
        assert!(
            h.wait_for_output_line("BTOP_CLEAN", Duration::from_secs(5)),
            "btop did not exit cleanly:\n{}",
            h.screen_text()
        );
        assert!(!h.terminal.screen().is_alternate_screen());
    }

    // 4. Interactive `nano` (launch -> alt screen -> type -> Ctrl+X -> 'n' -> return to shell)
    if Command::new("which")
        .arg("nano")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let mut h = HeadlessPtyHarness::spawn(80, 24);
        h.send(b"nano; printf 'NANO_%s\\n' CLEAN\n");
        assert!(h.wait_for_alt_screen(true, Duration::from_secs(4)));
        std::thread::sleep(Duration::from_millis(100));
        h.send(b"\x18"); // Ctrl+X
        assert!(
            h.wait_for_output_line("NANO_CLEAN", Duration::from_secs(4)),
            "nano did not exit cleanly:\n{}",
            h.screen_text()
        );
        assert!(!h.terminal.screen().is_alternate_screen());
    }
}

#[test]
fn sdk_showcase_extension_loads_and_renders_valid_ui_tree() {
    let path = std::path::Path::new("extensions/examples/sdk-showcase");
    assert!(path.join("beyrix-ext.toml").is_file());
    let loaded = crate::widgets::manifest::ExtensionManifest::load_from_dir(path).unwrap();
    let bus = crate::events::EventBus::new();
    let mut mgr = crate::widgets::WidgetManager::new(crate::config::WidgetsConfig {
        enabled: true,
        hot_reload: false,
        ..Default::default()
    });
    mgr.ensure_started(crate::theme::Theme::default(), bus)
        .unwrap();
    mgr.register_extension(loaded, crate::widgets::ExtensionTrust::ThirdParty)
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let snaps = mgr.render_snapshots();
    let st = snaps
        .get("beyrix.example.sdk-showcase")
        .expect("sdk-showcase must render");
    assert!(st.last_error.is_none(), "JS error: {:?}", st.last_error);
    assert!(st.tree.is_some());
    // When hot_reload is false, poll_hot_reload must do zero work.
    assert_eq!(mgr.poll_hot_reload(), 0);
}

#[test]
fn sgr_attributes_render_distinct_scenes_in_software_backend() {
    let mut t = Terminal::new(12, 2);
    t.process(b"\x1b[1;4:3;9;53mHi\x1b[0m");
    let cell = t.screen().get(0, 0).unwrap();
    assert!(cell.attrs.contains(CellAttrs::BOLD));
    assert!(cell.attrs.contains(CellAttrs::STRIKETHROUGH));
    assert!(cell.attrs.contains(CellAttrs::OVERLINE));
    assert_eq!(cell.underline, UnderlineStyle::Curly);

    let mut renderer = crate::render::Renderer::new().unwrap();
    let scene = renderer.build_terminal_scene(
        t.screen(),
        (400, 120),
        &crate::selection::Selection::default(),
        true,
        true,
        1.0,
        None,
    );
    assert!(scene.overlays.len() >= 4);
}

#[test]
fn all_official_extensions_load_render_and_report_load_errors_in_catalog() {
    let official_dir = std::path::Path::new("extensions/official");
    assert!(official_dir.is_dir());

    let bus = crate::events::EventBus::new();
    let mut mgr = crate::widgets::WidgetManager::new(crate::config::WidgetsConfig {
        enabled: true,
        hot_reload: false,
        ..Default::default()
    });
    let theme = crate::theme::Theme::default();
    mgr.ensure_started(theme.clone(), bus).unwrap();
    let loaded_count = mgr
        .load_directory(official_dir, crate::widgets::ExtensionTrust::HostOfficial)
        .unwrap();

    // Zero silent drops: every official extension must load cleanly.
    assert!(
        mgr.load_errors.is_empty(),
        "unexpected official extension load errors: {:?}",
        mgr.load_errors
    );
    assert!(
        loaded_count >= 2,
        "expected keyboard-layout and media-player, got {loaded_count}"
    );
    assert!(mgr.extensions.contains_key("beyrix.official.keyboard-layout"));
    assert!(mgr.extensions.contains_key("beyrix.official.media-player"));

    // Wait briefly for JS runtime to run initial renderWidget() for all official widgets.
    std::thread::sleep(Duration::from_millis(160));
    let snaps = mgr.render_snapshots();
    for id in [
        "beyrix.official.keyboard-layout",
        "beyrix.official.media-player",
    ] {
        let st = snaps
            .get(id)
            .unwrap_or_else(|| panic!("official extension {id} must produce render snapshot"));
        assert!(
            st.last_error.is_none(),
            "official extension {id} had JS error: {:?}",
            st.last_error
        );
        assert!(
            st.tree.is_some(),
            "official extension {id} must emit a UiNode tree"
        );
    }

    // Verify both widgets emit valid geometry across all required widths and HiDPI scales.
    let mut renderer = crate::render::Renderer::with_theme(theme.clone()).unwrap();
    for &logical_w in &[320.0_f32, 480.0, 720.0, 1024.0, 1600.0] {
        for &scale in &[1.0_f32, 1.25, 1.5, 1.75] {
            let win_w = logical_w * scale;
            let win_h = 640.0 * scale;
            let mut scene = crate::render::Scene::default();
            mgr.emit_to_scene(
                (win_w, win_h),
                scale,
                (10.0 * scale, 18.0 * scale),
                false,
                (0.0, 0.0),
                None,
                None,
                &theme,
                &mut renderer,
                &mut scene,
            );
            assert!(
                !scene.overlay_rounded.is_empty(),
                "widgets must emit rounded surfaces at {logical_w}px@{scale}x"
            );
            assert!(
                !scene.overlay_glyphs.is_empty(),
                "widgets must emit glyphs at {logical_w}px@{scale}x"
            );
            for (wid, rect) in &mgr.resolved_rects {
                assert!(
                    rect.x >= -0.5 && rect.y >= -0.5 && rect.x + rect.w <= win_w + 0.5 && rect.y + rect.h <= win_h + 0.5,
                    "widget {wid} overflowed viewport {win_w}x{win_h}: {rect:?}"
                );
            }
            // No button may be squished below readable minimum height.
            for hit in &mgr.hit_targets {
                if hit.kind == crate::widgets::ui::HitKind::Button {
                    assert!(
                        hit.h >= 19.0 * scale,
                        "button {} squished to h={} at {logical_w}px@{scale}x",
                        hit.node_id,
                        hit.h
                    );
                }
            }
        }
    }

    // Verify broken manifests are surfaced in load_errors (never silently dropped).
    let broken_root = tempfile::tempdir().unwrap();
    let broken_ext = broken_root.path().join("broken-widget");
    std::fs::create_dir_all(&broken_ext).unwrap();
    std::fs::write(broken_ext.join("index.js"), "beyrix.ui.render(beyrix.ui.text('x'));").unwrap();
    std::fs::write(
        broken_ext.join("beyrix-ext.toml"),
        r#"
id = "org.test.broken"
name = "Broken"
version = "1.0.0"
kind = "widget"
sdk_version = "1.0.0"
entry = "index.js"
permissions = ["ui", "nonexistent-cap"]
"#,
    )
    .unwrap();
    let _ = mgr.load_directory(broken_root.path(), crate::widgets::ExtensionTrust::ThirdParty);
    assert!(
        mgr.load_errors
            .iter()
            .any(|e| e.dir_name == "broken-widget" && e.error.contains("nonexistent-cap")),
        "broken manifest error must be recorded in load_errors: {:?}",
        mgr.load_errors
    );
}

#[test]
fn software_renderer_paints_true_rounded_corners_clipping_and_opaque_panels() {
    let theme = crate::theme::Theme::default();
    let mut renderer = crate::render::Renderer::with_theme(theme.clone()).unwrap();
    assert!(
        renderer.has_distinct_ui_font(),
        "proportional UI font stack should load separately from monospace terminal font"
    );

    // Paint bright green terminal background text, then an opaque rounded card on top.
    let w = 240usize;
    let h = 160usize;
    let mut scene = crate::render::Scene {
        background: [0.05, 0.06, 0.08, 1.0],
        ..Default::default()
    };
    scene.rects.push(crate::render::SceneRect {
        x: 0.0,
        y: 0.0,
        w: w as f32,
        h: h as f32,
        color: [0.0, 0.9, 0.1, 1.0], // bright green behind panel
    });
    // Rounded opaque card with radius=20 at (40, 30, 160, 100)
    scene.overlay_rounded.push(crate::render::SceneRoundedRect {
        x: 40.0,
        y: 30.0,
        w: 160.0,
        h: 100.0,
        radius: 20.0,
        color: [0.12, 0.14, 0.20, 1.0],
        border_width: 2.0,
        border_color: [0.4, 0.7, 1.0, 1.0],
        shadow_blur: 0.0,
        shadow_offset_y: 0.0,
        shadow_color: [0.0; 4],
        clip: None,
    });
    // Clipped bright red rect that attempts to draw outside [60..140, 50..90]
    scene.overlay_rounded.push(crate::render::SceneRoundedRect {
        x: 10.0,
        y: 10.0,
        w: 220.0,
        h: 140.0,
        radius: 0.0,
        color: [0.95, 0.1, 0.1, 1.0],
        border_width: 0.0,
        border_color: [0.0; 4],
        shadow_blur: 0.0,
        shadow_offset_y: 0.0,
        shadow_color: [0.0; 4],
        clip: Some([60.0, 50.0, 80.0, 40.0]),
    });

    let mut pixels = vec![0u8; w * h * 4];
    renderer.paint_scene(&scene, w, &mut pixels);
    let px_at = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * w + x) * 4;
        [pixels[off], pixels[off + 1], pixels[off + 2], pixels[off + 3]] // BGRA
    };

    // 1. Exact corner (40, 30) of radius=20 card MUST still show green background (NOT card fill!).
    let corner = px_at(40, 30);
    assert!(
        corner[1] > 180 && corner[2] < 40,
        "corner pixel (40,30) should be rounded away to show green bg, got BGRA={corner:?}"
    );
    // 2. Interior card pixel outside red clip (e.g. 50, 70) MUST be opaque card color (no green bleed).
    let interior = px_at(50, 70);
    assert!(
        interior[1] < 60 && interior[0] > 35,
        "interior pixel (50,70) should be opaque card fill without green bleed, got BGRA={interior:?}"
    );
    // 3. Inside clip rect (100, 70) MUST be red (BGRA: R=pixels[2]).
    let clipped_in = px_at(100, 70);
    assert!(
        clipped_in[2] > 200 && clipped_in[1] < 40,
        "inside clip pixel (100,70) should be red, got BGRA={clipped_in:?}"
    );
    // 4. Outside clip rect (15, 15) MUST NOT be red — still green bg.
    let clipped_out = px_at(15, 15);
    assert!(
        clipped_out[1] > 180 && clipped_out[2] < 40,
        "outside clip pixel (15,15) must remain green, got BGRA={clipped_out:?}"
    );
}

#[test]
fn visual_regression_studio_xray_settings_and_widgets_geometry_and_screenshots() {
    let theme = crate::theme::Theme::builtin("developer").unwrap();
    let mut renderer = crate::render::Renderer::with_theme(theme.clone()).unwrap();
    let mut terminal = Terminal::with_theme(120, 36, theme.clone());
    terminal.process(
        b"\x1b[1;36mbeykus@fedora\x1b[0m \x1b[33m~/Beynal/beynal\x1b[0m $ cargo test --release\r\n\
          \x1b[32m   Compiling\x1b[0m beyrix v0.1.0-alpha.2 (/home/beykus/Beynal/beynal)\r\n\
          \x1b[32m    Finished\x1b[0m `release` profile [optimized] target(s) in 4.12s\r\n\
          \x1b[1;36mbeykus@fedora\x1b[0m \x1b[33m~/Beynal/beynal\x1b[0m $ ",
    );

    let bus = crate::events::EventBus::new();
    let mut mgr = crate::widgets::WidgetManager::new(crate::config::WidgetsConfig {
        enabled: true,
        hot_reload: false,
        ..Default::default()
    });
    mgr.ensure_started(theme.clone(), bus.clone()).unwrap();
    let _ = mgr.load_directory(
        std::path::Path::new("extensions/official"),
        crate::widgets::ExtensionTrust::HostOfficial,
    );
    std::thread::sleep(Duration::from_millis(160));

    let mut platform_cfg = crate::config::PlatformConfig::default();
    platform_cfg.widgets.enabled = true;
    platform_cfg.xray.enabled = true;

    let mut xray = crate::xray::XRayEngine::new(platform_cfg.xray.clone());
    let mut settings = crate::widgets::editor_ui::SettingsOverlay::default();

    let save_dir = std::env::var_os("BEYRIX_SAVE_SCREENSHOTS").map(std::path::PathBuf::from);
    if let Some(dir) = &save_dir {
        let _ = std::fs::create_dir_all(dir);
    }

    let save_png = |name: &str, w: usize, h: usize, bgra: &[u8]| {
        if let Some(dir) = &save_dir {
            let mut rgba = vec![0u8; w * h * 4];
            for (src, dst) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                dst[3] = 255;
            }
            let img = image::RgbaImage::from_raw(w as u32, h as u32, rgba).unwrap();
            img.save(dir.join(format!("{name}.png"))).unwrap();
        }
    };

    // 1. Live Desktop (Widgets + X-Ray compact indicator) at Wide (1280x720) and Narrow (480x640)
    for (label, lw, lh, scale) in [
        ("widgets_1280x720", 1280.0_f32, 720.0_f32, 1.0_f32),
        ("widgets_480x640", 480.0, 640.0, 1.0),
        ("widgets_320x480_hidpi", 320.0, 480.0, 1.25),
    ] {
        mgr.update_window(lw, lh, scale, true);
        std::thread::sleep(Duration::from_millis(90));
        let pw = (lw * scale).round() as usize;
        let ph = (lh * scale).round() as usize;
        let mut scene = renderer.build_terminal_scene(
            terminal.screen(),
            (pw, ph),
            &crate::selection::Selection::default(),
            true,
            true,
            scale,
            None,
        );
        mgr.emit_to_scene_with_config(
            Some(&platform_cfg),
            (pw as f32, ph as f32),
            scale,
            (10.0 * scale, 18.0 * scale),
            false,
            (0.0, 0.0),
            None,
            None,
            &theme,
            &mut renderer,
            &mut scene,
        );
        let mut xhits = Vec::new();
        let xrects = xray.emit_to_scene(
            (pw as f32, ph as f32),
            scale,
            (0.0, 0.0),
            &theme,
            &mut renderer,
            &mut scene,
            &mut xhits,
        );
        assert!(xrects.indicator.is_some());
        let mut pixels = vec![0u8; pw * ph * 4];
        renderer.paint_scene(&scene, pw, &mut pixels);
        save_png(label, pw, ph, &pixels);
    }

    // 2. Beyrix Studio Wide (1600x900) — Topbar, Left Sidebar, Right Inspector must not overlap!
    mgr.editor.active = true;
    mgr.editor.selected_widget = Some("beyrix.official.media-player".into());
    {
        let (pw, ph, scale) = (1600usize, 900usize, 1.0_f32);
        let mut scene = renderer.build_terminal_scene(
            terminal.screen(),
            (pw, ph),
            &crate::selection::Selection::default(),
            true,
            true,
            scale,
            None,
        );
        mgr.emit_to_scene_with_config(
            Some(&platform_cfg),
            (pw as f32, ph as f32),
            scale,
            (10.0, 18.0),
            false,
            (0.0, 0.0),
            None,
            None,
            &theme,
            &mut renderer,
            &mut scene,
        );
        assert_eq!(
            mgr.editor_panel_rects.len(),
            3,
            "Wide Studio must emit Topbar + Left + Right panels"
        );
        for i in 0..mgr.editor_panel_rects.len() {
            let a = mgr.editor_panel_rects[i];
            for b in &mgr.editor_panel_rects[i + 1..] {
                let overlaps = a.0 < b.0 + b.2
                    && a.0 + a.2 > b.0
                    && a.1 < b.1 + b.3
                    && a.1 + a.3 > b.1;
                assert!(!overlaps, "Studio panels overlapped: {a:?} vs {b:?}");
            }
        }
        // Must expose real TextInput fields (not W+/W-/H+/H- cycling buttons)
        assert!(
            mgr.editor_hits
                .iter()
                .any(|h| h.node_id == "ed.in.w" && h.kind == crate::widgets::ui::HitKind::TextInput),
            "Studio inspector must expose editable TextInput fields"
        );
        let mut pixels = vec![0u8; pw * ph * 4];
        renderer.paint_scene(&scene, pw, &mut pixels);
        save_png("studio_wide_1600x900", pw, ph, &pixels);
    }

    // 3. Beyrix Studio Narrow (480x640) — Collapsible Inspector Drawer, zero overflow!
    mgr.editor.narrow_pane = crate::widgets::layout::NarrowStudioPane::Inspector;
    {
        let (pw, ph, scale) = (480usize, 640usize, 1.0_f32);
        let mut scene = renderer.build_terminal_scene(
            terminal.screen(),
            (pw, ph),
            &crate::selection::Selection::default(),
            true,
            true,
            scale,
            None,
        );
        mgr.emit_to_scene_with_config(
            Some(&platform_cfg),
            (pw as f32, ph as f32),
            scale,
            (10.0, 18.0),
            false,
            (0.0, 0.0),
            None,
            None,
            &theme,
            &mut renderer,
            &mut scene,
        );
        for h in &mgr.editor_hits {
            assert!(
                h.x >= -0.5 && h.x + h.w <= pw as f32 + 0.5 && h.y + h.h <= ph as f32 + 0.5,
                "Narrow Studio hit overflowed viewport: {h:?}"
            );
        }
        let mut pixels = vec![0u8; pw * ph * 4];
        renderer.paint_scene(&scene, pw, &mut pixels);
        save_png("studio_narrow_480x640", pw, ph, &pixels);
    }
    mgr.editor.active = false;

    // 4. X-Ray Panel (1024x640)
    xray.panel_open = true;
    {
        let (pw, ph, scale) = (1024usize, 640usize, 1.0_f32);
        let mut scene = renderer.build_terminal_scene(
            terminal.screen(),
            (pw, ph),
            &crate::selection::Selection::default(),
            true,
            true,
            scale,
            None,
        );
        let mut xhits = Vec::new();
        let xrects = xray.emit_to_scene(
            (pw as f32, ph as f32),
            scale,
            (0.0, 0.0),
            &theme,
            &mut renderer,
            &mut scene,
            &mut xhits,
        );
        assert!(xrects.panel.is_some());
        let mut pixels = vec![0u8; pw * ph * 4];
        renderer.paint_scene(&scene, pw, &mut pixels);
        save_png("xray_panel_1024x640", pw, ph, &pixels);
    }
    xray.panel_open = false;

    // 5. Settings Overlay (1024x640)
    settings.open = true;
    {
        let (pw, ph, scale) = (1024usize, 640usize, 1.0_f32);
        let mut scene = renderer.build_terminal_scene(
            terminal.screen(),
            (pw, ph),
            &crate::selection::Selection::default(),
            true,
            true,
            scale,
            None,
        );
        settings.emit_to_scene(
            &platform_cfg,
            (pw as f32, ph as f32),
            scale,
            (0.0, 0.0),
            &theme,
            &mut renderer,
            &mut scene,
        );
        assert!(settings.panel_rect.is_some());
        let mut pixels = vec![0u8; pw * ph * 4];
        renderer.paint_scene(&scene, pw, &mut pixels);
        save_png("settings_1024x640", pw, ph, &pixels);
    }
}
