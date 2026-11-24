// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::OwnedFd;

const XKB_KEY_BACKSPACE: u32 = 0xff08;
const XKB_KEY_DELETE: u32 = 0xffff;
const XKB_KEY_LEFT: u32 = 0xff51;
const XKB_KEY_RIGHT: u32 = 0xff53;

fn ctrl_edit_sequence(sym: u32) -> Option<&'static [u8]> {
    match sym {
        // Ctrl+W (ETB): Readline's unix-word-rubout, which uses blanks and tabs
        // as delimiters. This is understood by any interactive line editor,
        // unlike a bare kitty CSI-u sequence that requires shell-side bindings.
        XKB_KEY_BACKSPACE => Some(b"\x17"),
        XKB_KEY_DELETE => Some(b"\x1b[3;5~"),
        XKB_KEY_LEFT => Some(b"\x1b[1;5D"),
        XKB_KEY_RIGHT => Some(b"\x1b[1;5C"),
        _ => None,
    }
}

#[link(name = "xkbcommon")]
unsafe extern "C" {
    fn xkb_context_new(flags: c_int) -> *mut c_void;
    fn xkb_context_unref(ctx: *mut c_void);
    fn xkb_keymap_new_from_string(
        ctx: *mut c_void,
        text: *const c_char,
        format: c_int,
        flags: c_int,
    ) -> *mut c_void;
    fn xkb_keymap_unref(map: *mut c_void);
    fn xkb_state_new(map: *mut c_void) -> *mut c_void;
    fn xkb_state_unref(state: *mut c_void);
    fn xkb_state_update_mask(
        state: *mut c_void,
        depressed: u32,
        latched: u32,
        locked: u32,
        depressed_layout: u32,
        latched_layout: u32,
        locked_layout: u32,
    ) -> u32;
    fn xkb_state_mod_name_is_active(state: *mut c_void, name: *const c_char, kind: c_int) -> c_int;
    fn xkb_state_key_get_one_sym(state: *mut c_void, key: u32) -> u32;
    fn xkb_state_key_get_utf8(
        state: *mut c_void,
        key: u32,
        buffer: *mut c_char,
        size: usize,
    ) -> c_int;
    fn xkb_keymap_num_layouts(keymap: *mut c_void) -> u32;
    fn xkb_keymap_layout_get_name(keymap: *mut c_void, idx: u32) -> *const c_char;
    fn xkb_state_layout_index_is_active(state: *mut c_void, idx: u32, kind: c_int) -> c_int;
}

/// Derive a compact 2-3 character layout code from an XKB layout name.
pub fn short_layout_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for (needle, code) in [
        ("english", "EN"),
        ("russian", "RU"),
        ("ukrainian", "UA"),
        ("belarusian", "BY"),
        ("german", "DE"),
        ("french", "FR"),
        ("spanish", "ES"),
        ("italian", "IT"),
        ("portuguese", "PT"),
        ("polish", "PL"),
        ("turkish", "TR"),
        ("japanese", "JA"),
        ("korean", "KO"),
        ("chinese", "ZH"),
        ("arabic", "AR"),
        ("hebrew", "HE"),
        ("greek", "EL"),
        ("czech", "CS"),
        ("swedish", "SV"),
        ("norwegian", "NO"),
        ("finnish", "FI"),
        ("dutch", "NL"),
        ("kazakh", "KK"),
        ("(us)", "EN"),
    ] {
        if lower.contains(needle) {
            return code.to_string();
        }
    }
    if lower == "us" {
        return "EN".to_string();
    }
    let code: String = name
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .take(2)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if code.is_empty() {
        "??".to_string()
    } else {
        code
    }
}

pub struct Keyboard {
    context: *mut c_void,
    keymap: *mut c_void,
    state: *mut c_void,
}
impl Keyboard {
    pub fn new() -> io::Result<Self> {
        let context = unsafe { xkb_context_new(0) };
        if context.is_null() {
            return Err(io::Error::other("xkb context creation failed"));
        }
        Ok(Self {
            context,
            keymap: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
        })
    }
    pub fn keymap(&mut self, fd: OwnedFd, size: u32) -> io::Result<()> {
        let mut bytes = Vec::new();
        let mut file = std::fs::File::from(fd);
        file.seek(SeekFrom::Start(0))?;
        file.take(size as u64).read_to_end(&mut bytes)?;
        if bytes.len() != size as usize {
            return Err(io::Error::other("short xkb keymap"));
        }
        if bytes.last() == Some(&0) {
            bytes.pop();
        }
        let text = CString::new(bytes).map_err(|_| io::Error::other("invalid xkb keymap"))?;
        let map = unsafe { xkb_keymap_new_from_string(self.context, text.as_ptr(), 1, 0) };
        if map.is_null() {
            return Err(io::Error::other("xkb keymap compilation failed"));
        }
        let state = unsafe { xkb_state_new(map) };
        if state.is_null() {
            unsafe { xkb_keymap_unref(map) };
            return Err(io::Error::other("xkb state creation failed"));
        }
        unsafe {
            if !self.state.is_null() {
                xkb_state_unref(self.state);
            }
            if !self.keymap.is_null() {
                xkb_keymap_unref(self.keymap);
            }
        }
        self.keymap = map;
        self.state = state;
        Ok(())
    }
    pub fn modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        if !self.state.is_null() {
            unsafe {
                xkb_state_update_mask(self.state, depressed, latched, locked, 0, 0, group);
            }
        }
    }

    /// Return all XKB layout names configured in the active keymap.
    pub fn layouts(&self) -> Vec<String> {
        if self.keymap.is_null() {
            return Vec::new();
        }
        let count = unsafe { xkb_keymap_num_layouts(self.keymap) };
        let mut out = Vec::with_capacity(count as usize);
        for idx in 0..count {
            let ptr = unsafe { xkb_keymap_layout_get_name(self.keymap, idx) };
            let name = if ptr.is_null() {
                format!("Layout {}", idx + 1)
            } else {
                unsafe { CStr::from_ptr(ptr) }
                    .to_string_lossy()
                    .into_owned()
            };
            out.push(name);
        }
        out
    }

    /// Return the currently active effective XKB layout group index.
    pub fn active_layout_index(&self) -> u32 {
        if self.keymap.is_null() || self.state.is_null() {
            return 0;
        }
        let count = unsafe { xkb_keymap_num_layouts(self.keymap) };
        for idx in 0..count {
            // XKB_STATE_LAYOUT_EFFECTIVE = 1 << 7 = 128
            if unsafe { xkb_state_layout_index_is_active(self.state, idx, 128) > 0 } {
                return idx;
            }
        }
        0
    }

    /// Build a [`crate::events::KeyboardLayoutEvent`] snapshot for the active window.
    pub fn layout_event(&self) -> Option<crate::events::KeyboardLayoutEvent> {
        let layouts = self.layouts();
        if layouts.is_empty() {
            return None;
        }
        let active_index = self.active_layout_index();
        let active_name = layouts
            .get(active_index as usize)
            .cloned()
            .unwrap_or_else(|| "Unknown".to_string());
        let short_name = short_layout_name(&active_name);
        Some(crate::events::KeyboardLayoutEvent {
            active_index,
            active_name,
            short_name,
            layouts,
        })
    }
    fn modifier(&self, name: &CStr) -> bool {
        !self.state.is_null()
            && unsafe { xkb_state_mod_name_is_active(self.state, name.as_ptr(), 8) > 0 }
    }
    pub fn shift(&self) -> bool {
        self.modifier(c"Shift")
    }
    pub fn control(&self) -> bool {
        self.modifier(c"Control")
    }
    pub fn alt(&self) -> bool {
        self.modifier(c"Mod1")
    }
    /// Keysym for the editor's key bindings.
    pub fn sym(&self, key: u32) -> u32 {
        if self.state.is_null() {
            return 0;
        }
        unsafe { xkb_state_key_get_one_sym(self.state, key + 8) }
    }
    /// Text produced by a key without control-key translation.
    pub fn text(&self, key: u32) -> Vec<u8> {
        if self.state.is_null() {
            return Vec::new();
        }
        let key = key + 8;
        let sym = unsafe { xkb_state_key_get_one_sym(self.state, key) };
        let mut buffer = [0i8; 64];
        unsafe {
            xkb_state_key_get_utf8(self.state, key, buffer.as_mut_ptr(), buffer.len());
        }
        let text = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_bytes();
        translate(sym, text)
    }
    pub fn key(&self, key: u32) -> Vec<u8> {
        if self.state.is_null() {
            return Vec::new();
        }
        let key = key + 8;
        let sym = unsafe { xkb_state_key_get_one_sym(self.state, key) };
        if let Some(sequence) = ctrl_edit_sequence(sym)
            && self.control()
            && !self.shift()
            && !self.modifier(c"Mod1")
            && !self.modifier(c"Mod4")
            && !self.modifier(c"Mod5")
        {
            return sequence.to_vec();
        }
        let mut buffer = [0i8; 64];
        unsafe {
            xkb_state_key_get_utf8(self.state, key, buffer.as_mut_ptr(), buffer.len());
        }
        let text = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_bytes();
        translate(sym, text)
    }
}
impl Drop for Keyboard {
    fn drop(&mut self) {
        unsafe {
            if !self.state.is_null() {
                xkb_state_unref(self.state);
            }
            if !self.keymap.is_null() {
                xkb_keymap_unref(self.keymap);
            }
            xkb_context_unref(self.context);
        }
    }
}
fn translate(sym: u32, text: &[u8]) -> Vec<u8> {
    match sym {
        0xff0d | 0xff8d => b"\r".to_vec(),
        XKB_KEY_BACKSPACE => vec![0x7f],
        0xff09 | 0xfe20 => vec![9],
        0xff1b => vec![27],
        0xff50 => b"\x1b[H".to_vec(),
        0xff57 => b"\x1b[F".to_vec(),
        XKB_KEY_DELETE => b"\x1b[3~".to_vec(),
        0xff55 => b"\x1b[5~".to_vec(),
        0xff56 => b"\x1b[6~".to_vec(),
        0xffbe..=0xffc1 => vec![27, b'O', b'P' + (sym - 0xffbe) as u8],
        0xffc2..=0xffc9 => {
            let codes = [15, 17, 18, 19, 20, 21, 23, 24];
            format!("\x1b[{}~", codes[(sym - 0xffc2) as usize]).into_bytes()
        }
        XKB_KEY_LEFT => b"\x1b[D".to_vec(),
        0xff52 => b"\x1b[A".to_vec(),
        XKB_KEY_RIGHT => b"\x1b[C".to_vec(),
        0xff54 => b"\x1b[B".to_vec(),
        _ => text.to_vec(),
    }
}
#[cfg(test)]
mod tests {
    use super::translate;
    #[test]
    fn ctrl_edit_keys_use_xkb_in_english_and_russian() {
        use std::io::Write;

        let map = br#"xkb_keymap {
            xkb_keycodes { include "evdev+aliases(qwerty)" };
            xkb_types { include "complete" };
            xkb_compatibility { include "complete" };
            xkb_symbols { include "pc+us+ru:2" };
        };"#;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(map).unwrap();
        let mut keyboard = super::Keyboard::new().unwrap();
        keyboard.keymap(file.into(), map.len() as u32).unwrap();

        for (group, letter) in [(0, "f"), (1, "а")] {
            // Wayland uses evdev keycodes. Shift and Control are core XKB mask bits 0 and 2.
            keyboard.modifiers(0, 0, 0, group);
            assert_eq!(keyboard.key(33), letter.as_bytes());
            assert_eq!(keyboard.key(15), b"\t");

            for (key, plain, ctrl) in [
                (14, &b"\x7f"[..], &b"\x17"[..]),
                (111, b"\x1b[3~", b"\x1b[3;5~"),
                (105, b"\x1b[D", b"\x1b[1;5D"),
                (106, b"\x1b[C", b"\x1b[1;5C"),
            ] {
                keyboard.modifiers(0, 0, 0, group);
                assert_eq!(keyboard.key(key), plain);
                keyboard.modifiers(1 << 2, 0, 0, group);
                assert_eq!(keyboard.key(key), ctrl);
                keyboard.modifiers((1 << 2) | 1, 0, 0, group);
                assert_eq!(keyboard.key(key), plain);
            }
        }
        keyboard.modifiers(1 << 2, 0, 0, 0);
        assert_eq!(keyboard.key(35), b"\x08"); // Ctrl+H remains distinct.
    }

    #[test]
    fn ctrl_backspace_sends_readline_word_rubout() {
        // 0x17 is the standard Ctrl+W control character. Readline binds it to
        // unix-word-rubout, which splits words on blanks and tabs only.
        assert_eq!(super::ctrl_edit_sequence(0xff08), Some(&b"\x17"[..]));
        assert_eq!(super::translate(0xff08, b""), b"\x7f");
        assert_eq!(super::translate(b'w' as u32, b"\x17"), b"\x17");
        // Ctrl+H (0x08) and Ctrl+Delete must stay available for their own bindings.
        assert_eq!(super::translate(b'h' as u32, b"\x08"), b"\x08");
        assert_eq!(super::ctrl_edit_sequence(0xffff), Some(&b"\x1b[3;5~"[..]));
    }

    #[test]
    fn special_keys() {
        for (sym, expected) in [
            (0xff0d, &b"\r"[..]),
            (0xff08, b"\x7f"),
            (0xff09, b"\t"),
            (0xff1b, b"\x1b"),
            (0xff51, b"\x1b[D"),
            (0xff52, b"\x1b[A"),
            (0xff53, b"\x1b[C"),
            (0xff54, b"\x1b[B"),
            (0xff50, b"\x1b[H"),
            (0xff57, b"\x1b[F"),
            (0xffff, b"\x1b[3~"),
            (0xff55, b"\x1b[5~"),
            (0xff56, b"\x1b[6~"),
            (0xffbe, b"\x1bOP"),
            (0xffc9, b"\x1b[24~"),
        ] {
            assert_eq!(translate(sym, b""), expected);
        }
    }
    #[test]
    fn text_and_controls() {
        for text in [&b"\x03"[..], b"\x04", b"\x0c", "я".as_bytes()] {
            assert_eq!(translate(0, text), text);
        }
    }

    #[test]
    fn enumerates_layouts_and_tracks_active_group() {
        use std::io::Write;
        let map = br#"xkb_keymap {
            xkb_keycodes { include "evdev+aliases(qwerty)" };
            xkb_types { include "complete" };
            xkb_compatibility { include "complete" };
            xkb_symbols { include "pc+us+ru:2" };
        };"#;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(map).unwrap();
        let mut keyboard = super::Keyboard::new().unwrap();
        keyboard.keymap(file.into(), map.len() as u32).unwrap();

        let layouts = keyboard.layouts();
        assert!(layouts.len() >= 2);
        keyboard.modifiers(0, 0, 0, 1);
        assert_eq!(keyboard.active_layout_index(), 1);
        let ev = keyboard.layout_event().unwrap();
        assert_eq!(ev.active_index, 1);
        assert_eq!(ev.short_name, "RU");

        keyboard.modifiers(0, 0, 0, 0);
        let ev0 = keyboard.layout_event().unwrap();
        assert_eq!(ev0.active_index, 0);
        assert_eq!(ev0.short_name, "EN");
    }
}
