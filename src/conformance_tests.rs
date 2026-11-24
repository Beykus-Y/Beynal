// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headless terminal conformance, differential, chunked-input, fuzz-resilience,
//! terminfo, and real-PTY TUI integration test harness.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::keyboard::{KeyEventType, Keyboard, alternate_scroll_sequence};
use crate::pty::Pty;
use crate::terminal::{CellAttrs, Terminal, TerminalInputModes, UnderlineStyle};
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

    fn wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump();
            if self.screen_text().contains(needle) {
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
    // Truncated / malformed CSI, OSC, DCS, APC, invalid UTF-8
    let cases: &[&[u8]] = &[
        b"\x1b[999999999;999999999H",
        b"\x1b[?99999999h\x1b[?99999999l",
        b"\x1b[38;2;999;999;999m",
        b"\x1b]8;;\x00\x01\x02bad\x07",
        b"\x1b]52;c;!!!not-base64!!!\x07",
        b"\x1bP+qZZZZ\x1b\\",
        b"\x1b_Ga=T,f=32,s=99999,v=99999;AAAA\x1b\\",
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

    // Kitty Keyboard Protocol: flags=3 (disambiguate + report events)
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
    h.send(b"printf 'ENV:%s|%s|%s|%s\\n' \"$TERM\" \"$COLORTERM\" \"$TERM_PROGRAM\" \"$(tput colors)\"\n");
    assert!(
        h.wait_for(
            "ENV:xterm-256color|truecolor|Beyrix|256",
            Duration::from_secs(4)
        ),
        "unexpected screen:\n{}",
        h.screen_text()
    );
}

#[test]
fn real_tui_apps_less_and_tmux_run_cleanly_in_pty() {
    // 1. Test `less` with alternate screen & clean exit via 'q'
    if Command::new("which")
        .arg("less")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let mut h = HeadlessPtyHarness::spawn(80, 20);
        h.send(b"printf 'LINE_ONE\\nLINE_TWO\\n' | less; echo LESS_EXITED\n");
        assert!(h.wait_for("LINE_ONE", Duration::from_secs(4)));
        h.send(b"q");
        assert!(
            h.wait_for("LESS_EXITED", Duration::from_secs(4)),
            "less did not exit cleanly:\n{}",
            h.screen_text()
        );
        assert!(!h.terminal.screen().is_alternate_screen());
    }

    // 2. Test `tmux` in an isolated socket (if installed)
    if Command::new("which")
        .arg("tmux")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let mut h = HeadlessPtyHarness::spawn(80, 24);
        let sock = format!("beyrix-test-{}", std::process::id());
        h.send(
            format!("tmux -L {sock} -f /dev/null new-session -d 'sleep 2' && tmux -L {sock} list-sessions && tmux -L {sock} kill-server && echo TMUX_OK\n")
                .as_bytes(),
        );
        assert!(
            h.wait_for("TMUX_OK", Duration::from_secs(5)),
            "tmux check failed:\n{}",
            h.screen_text()
        );
    }

    // 3. Test `fastfetch` (if installed)
    if Command::new("which")
        .arg("fastfetch")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let mut h = HeadlessPtyHarness::spawn(100, 30);
        h.send(b"fastfetch --logo none; echo FF_OK\n");
        assert!(
            h.wait_for("FF_OK", Duration::from_secs(5)),
            "fastfetch check failed:\n{}",
            h.screen_text()
        );
    }
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
    // Curly underline + strikethrough + overline must emit overlay decoration rects.
    assert!(scene.overlays.len() >= 4);
}
