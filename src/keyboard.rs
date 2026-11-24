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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyEventType {
    #[default]
    Press,
    Repeat,
    Release,
}

impl KeyEventType {
    fn kitty_code(self) -> u8 {
        match self {
            Self::Press => 1,
            Self::Repeat => 2,
            Self::Release => 3,
        }
    }
}

/// Encode arrow Up/Down for Alternate Scroll Mode (DEC ?1007).
pub fn alternate_scroll_sequence(up: bool, app_cursor_keys: bool) -> &'static [u8] {
    match (up, app_cursor_keys) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1b[A",
        (false, true) => b"\x1bOB",
        (false, false) => b"\x1b[B",
    }
}

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
    fn xkb_keymap_key_repeats(keymap: *mut c_void, key: u32) -> c_int;
    fn xkb_state_layout_index_is_active(state: *mut c_void, idx: u32, kind: c_int) -> c_int;
}

/// Layout-independent Linux evdev keycode to ASCII control byte (`Ctrl+A..Z`,
/// `Ctrl+Space`, `Ctrl+[`, `Ctrl+\`, `Ctrl+]`, `Ctrl+^`, `Ctrl+_`) used when
/// a non-Latin XKB layout (e.g. Russian) is active so terminal control chords
/// like `Ctrl+D`, `Ctrl+Z`, `Ctrl+S`, `Ctrl+Q`, `Ctrl+L` always work.
fn evdev_ctrl_byte(evdev_key: u32) -> Option<u8> {
    match evdev_key {
        30 => Some(0x01), // A
        48 => Some(0x02), // B
        46 => Some(0x03), // C
        32 => Some(0x04), // D
        18 => Some(0x05), // E
        33 => Some(0x06), // F
        34 => Some(0x07), // G
        35 => Some(0x08), // H
        23 => Some(0x09), // I
        36 => Some(0x0a), // J
        37 => Some(0x0b), // K
        38 => Some(0x0c), // L
        50 => Some(0x0d), // M
        49 => Some(0x0e), // N
        24 => Some(0x0f), // O
        25 => Some(0x10), // P
        16 => Some(0x11), // Q
        19 => Some(0x12), // R
        31 => Some(0x13), // S
        20 => Some(0x14), // T
        22 => Some(0x15), // U
        47 => Some(0x16), // V
        17 => Some(0x17), // W
        45 => Some(0x18), // X
        21 => Some(0x19), // Y
        44 => Some(0x1a), // Z
        57 => Some(0x00), // Space -> NUL
        26 => Some(0x1b), // [ -> ESC
        43 => Some(0x1c), // \ -> FS
        27 => Some(0x1d), // ] -> GS
        7 => Some(0x1e),  // 6 / ^ -> RS
        12 => Some(0x1f), // - / _ -> US
        _ => None,
    }
}

/// Xterm modifier parameter (`1 + shift + 2*alt + 4*ctrl`) when at least one
/// modifier is active.
fn xterm_modifier(shift: bool, alt: bool, ctrl: bool) -> Option<u8> {
    let code = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    (code > 1).then_some(code)
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
    /// Whether this evdev keycode should autorepeat according to the active XKB keymap.
    pub fn key_repeats(&self, key: u32) -> bool {
        if self.keymap.is_null() {
            return false;
        }
        unsafe { xkb_keymap_key_repeats(self.keymap, key + 8) > 0 }
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
        let xkb_key = key + 8;
        let sym = unsafe { xkb_state_key_get_one_sym(self.state, xkb_key) };
        let mut buffer = [0i8; 64];
        unsafe {
            xkb_state_key_get_utf8(self.state, xkb_key, buffer.as_mut_ptr(), buffer.len());
        }
        let text = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_bytes();
        translate(sym, text)
    }
    #[cfg(test)]
    pub fn key(&self, key: u32) -> Vec<u8> {
        self.key_with_modes(
            key,
            crate::terminal::TerminalInputModes::default(),
            KeyEventType::Press,
        )
    }

    /// Encode a key event according to active terminal modes (DECCKM, DECKPAM,
    /// DECBKM, modifyOtherKeys, and Kitty Keyboard Protocol).
    pub fn key_with_modes(
        &self,
        key: u32,
        modes: crate::terminal::TerminalInputModes,
        event_type: KeyEventType,
    ) -> Vec<u8> {
        if self.state.is_null() {
            return Vec::new();
        }
        let xkb_key = key + 8;
        let sym = unsafe { xkb_state_key_get_one_sym(self.state, xkb_key) };
        let ctrl = self.control();
        let shift = self.shift();
        let alt = self.alt();
        let super_mod = self.modifier(c"Mod4");
        let altgr = self.modifier(c"Mod5");

        let mut buffer = [0i8; 64];
        unsafe {
            xkb_state_key_get_utf8(self.state, xkb_key, buffer.as_mut_ptr(), buffer.len());
        }
        let text = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_bytes();

        // 1. Kitty Keyboard Protocol (active when stack top > 0).
        if modes.kitty_flags != 0 {
            let report_events = (modes.kitty_flags & 2) != 0;
            if event_type == KeyEventType::Release && !report_events {
                return Vec::new();
            }
            if let Some(seq) = encode_kitty_key(
                sym,
                key,
                text,
                shift,
                alt,
                ctrl,
                super_mod,
                modes.kitty_flags,
                event_type,
            ) {
                return seq;
            }
            if event_type == KeyEventType::Release {
                return Vec::new();
            }
        } else if event_type == KeyEventType::Release {
            // Legacy protocols never send key release events.
            return Vec::new();
        }

        // 2. XTerm modifyOtherKeys (CSI > 4 ; 1/2 m).
        if modes.modify_other_keys >= 1
            && !super_mod
            && let Some(seq) =
                encode_modify_other_keys(sym, text, shift, alt, ctrl, modes.modify_other_keys)
        {
            return seq;
        }

        // 3. Preserve existing Ctrl+Backspace/Delete/Left/Right editing semantics.
        if let Some(sequence) = ctrl_edit_sequence(sym)
            && ctrl
            && !shift
            && !alt
            && !super_mod
            && !altgr
        {
            return sequence.to_vec();
        }

        // 4. Modified navigation & function keys (xterm CSI 1;mod ...).
        if !super_mod
            && !altgr
            && let Some(seq) = translate_modified_special(sym, shift, alt, ctrl)
        {
            return seq;
        }

        // 5. Application Keypad (DECKPAM) for unmodified numpad keys.
        if modes.app_keypad
            && !ctrl
            && !alt
            && !super_mod
            && let Some(seq) = translate_app_keypad(sym)
        {
            return seq.to_vec();
        }

        // 6. Ctrl+A..Z / Ctrl+Space / Ctrl+[\]^_ fallback for non-Latin layouts (e.g. Russian)
        // when XKB did not produce an ASCII control character.
        if ctrl && !super_mod && !altgr {
            let has_ascii_ctrl = text.len() == 1 && text[0] < 0x20;
            if !has_ascii_ctrl && let Some(ctrl_byte) = evdev_ctrl_byte(key) {
                return if alt {
                    vec![0x1b, ctrl_byte]
                } else {
                    vec![ctrl_byte]
                };
            }
        }

        let mut out = translate_with_modes(sym, text, modes.app_cursor_keys, modes.backarrow_key);

        // Alt+key sends ESC prefix for Meta bindings in Bash/Readline/Vim/tmux
        // (e.g. Alt+b, Alt+f, Alt+d, Alt+Backspace, Alt+.).
        if alt && !super_mod && !altgr && !out.is_empty() && out[0] != 0x1b {
            let mut prefixed = Vec::with_capacity(out.len() + 1);
            prefixed.push(0x1b);
            prefixed.extend_from_slice(&out);
            out = prefixed;
        }
        out
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
fn translate_modified_special(sym: u32, shift: bool, alt: bool, ctrl: bool) -> Option<Vec<u8>> {
    // Shift+Tab -> BackTab (CSI Z)
    if sym == 0xfe20 || (sym == 0xff09 && shift && !ctrl && !alt) {
        return Some(b"\x1b[Z".to_vec());
    }
    // Keep existing Ctrl+Shift+Backspace/Delete/Left/Right untouched when both
    // Ctrl and Shift are held on those 4 keys? Wait: in standard xterm,
    // Ctrl+Shift+Left is CSI 1;6D, whereas our existing test checks
    // ctrl_edit_keys_use_xkb_in_english_and_russian line 351:
    // `keyboard.modifiers((1 << 2) | 1, 0, 0, group); assert_eq!(keyboard.key(key), plain);`
    // Preserve that exact existing contract for Ctrl+Shift on those 4 keys!
    if ctrl && shift && !alt && ctrl_edit_sequence(sym).is_some() {
        return None;
    }
    let m = xterm_modifier(shift, alt, ctrl)?;
    match sym {
        // Arrows
        0xff52 => Some(format!("\x1b[1;{m}A").into_bytes()),
        0xff54 => Some(format!("\x1b[1;{m}B").into_bytes()),
        XKB_KEY_RIGHT => Some(format!("\x1b[1;{m}C").into_bytes()),
        XKB_KEY_LEFT => Some(format!("\x1b[1;{m}D").into_bytes()),
        // Home / End
        0xff50 => Some(format!("\x1b[1;{m}H").into_bytes()),
        0xff57 => Some(format!("\x1b[1;{m}F").into_bytes()),
        // Delete / PageUp / PageDown / Insert
        XKB_KEY_DELETE => Some(format!("\x1b[3;{m}~").into_bytes()),
        0xff55 => Some(format!("\x1b[5;{m}~").into_bytes()),
        0xff56 => Some(format!("\x1b[6;{m}~").into_bytes()),
        0xff63 => Some(format!("\x1b[2;{m}~").into_bytes()),
        // F1-F4
        0xffbe..=0xffc1 => {
            let suffix = (b'P' + (sym - 0xffbe) as u8) as char;
            Some(format!("\x1b[1;{m}{suffix}").into_bytes())
        }
        // F5-F12
        0xffc2..=0xffc9 => {
            let codes = [15, 17, 18, 19, 20, 21, 23, 24];
            Some(format!("\x1b[{};{m}~", codes[(sym - 0xffc2) as usize]).into_bytes())
        }
        _ => None,
    }
}

fn translate(sym: u32, text: &[u8]) -> Vec<u8> {
    translate_with_modes(sym, text, false, false)
}

fn translate_with_modes(
    sym: u32,
    text: &[u8],
    app_cursor_keys: bool,
    backarrow_key: bool,
) -> Vec<u8> {
    match sym {
        0xff0d | 0xff8d => b"\r".to_vec(),
        XKB_KEY_BACKSPACE => vec![if backarrow_key { 0x08 } else { 0x7f }],
        0xff09 => vec![9],
        0xfe20 => b"\x1b[Z".to_vec(),
        0xff1b => vec![27],
        0xff50 => if app_cursor_keys {
            b"\x1bOH"
        } else {
            b"\x1b[H"
        }
        .to_vec(),
        0xff57 => if app_cursor_keys {
            b"\x1bOF"
        } else {
            b"\x1b[F"
        }
        .to_vec(),
        XKB_KEY_DELETE => b"\x1b[3~".to_vec(),
        0xff55 => b"\x1b[5~".to_vec(),
        0xff56 => b"\x1b[6~".to_vec(),
        0xff63 => b"\x1b[2~".to_vec(),
        0xffbe..=0xffc1 => vec![27, b'O', b'P' + (sym - 0xffbe) as u8],
        0xffc2..=0xffc9 => {
            let codes = [15, 17, 18, 19, 20, 21, 23, 24];
            format!("\x1b[{}~", codes[(sym - 0xffc2) as usize]).into_bytes()
        }
        XKB_KEY_LEFT => if app_cursor_keys {
            b"\x1bOD"
        } else {
            b"\x1b[D"
        }
        .to_vec(),
        0xff52 => if app_cursor_keys {
            b"\x1bOA"
        } else {
            b"\x1b[A"
        }
        .to_vec(),
        XKB_KEY_RIGHT => if app_cursor_keys {
            b"\x1bOC"
        } else {
            b"\x1b[C"
        }
        .to_vec(),
        0xff54 => if app_cursor_keys {
            b"\x1bOB"
        } else {
            b"\x1b[B"
        }
        .to_vec(),
        _ => text.to_vec(),
    }
}

fn translate_app_keypad(sym: u32) -> Option<&'static [u8]> {
    match sym {
        0xff8d => Some(b"\x1bOM"), // KP_Enter
        0xffaa => Some(b"\x1bOj"), // KP_Multiply
        0xffab => Some(b"\x1bOk"), // KP_Add
        0xffac => Some(b"\x1bOl"), // KP_Separator
        0xffad => Some(b"\x1bOm"), // KP_Subtract
        0xffae => Some(b"\x1bOn"), // KP_Decimal
        0xffaf => Some(b"\x1bOo"), // KP_Divide
        0xffb0 => Some(b"\x1bOp"), // KP_0
        0xffb1 => Some(b"\x1bOq"), // KP_1
        0xffb2 => Some(b"\x1bOr"), // KP_2
        0xffb3 => Some(b"\x1bOs"), // KP_3
        0xffb4 => Some(b"\x1bOt"), // KP_4
        0xffb5 => Some(b"\x1bOu"), // KP_5
        0xffb6 => Some(b"\x1bOv"), // KP_6
        0xffb7 => Some(b"\x1bOw"), // KP_7
        0xffb8 => Some(b"\x1bOx"), // KP_8
        0xffb9 => Some(b"\x1bOy"), // KP_9
        _ => None,
    }
}

fn encode_modify_other_keys(
    sym: u32,
    text: &[u8],
    shift: bool,
    alt: bool,
    ctrl: bool,
    mode: u8,
) -> Option<Vec<u8>> {
    let m = xterm_modifier(shift, alt, ctrl)?;
    let codepoint = match sym {
        0xff0d => 13u32,
        0xff09 => 9,
        0xff1b => 27,
        XKB_KEY_BACKSPACE => 127,
        _ if mode >= 2 && (ctrl || alt) => {
            if (0x20..=0x7e).contains(&sym) {
                sym
            } else if let Ok(s) = std::str::from_utf8(text)
                && let Some(ch) = s.chars().next()
                && s.chars().count() == 1
                && (ch as u32) >= 0x20
            {
                ch as u32
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(format!("\x1b[27;{m};{codepoint}~").into_bytes())
}

#[allow(clippy::too_many_arguments)]
fn encode_kitty_key(
    sym: u32,
    evdev_key: u32,
    text: &[u8],
    shift: bool,
    alt: bool,
    ctrl: bool,
    super_mod: bool,
    flags: u8,
    event_type: KeyEventType,
) -> Option<Vec<u8>> {
    let disambiguate = (flags & 1) != 0;
    let report_events = (flags & 2) != 0;
    let report_all = (flags & 8) != 0;
    let report_text = (flags & 16) != 0;

    let mod_bits =
        u8::from(shift) | (u8::from(alt) << 1) | (u8::from(ctrl) << 2) | (u8::from(super_mod) << 3);
    let mod_param = mod_bits + 1;
    let needs_event_suffix = report_events && event_type != KeyEventType::Press;

    // Special functional keys using CSI 1 ; mod[:ev] final or CSI num ; mod[:ev] ~
    let functional_suffix = |prefix_num: u32, final_ch: char| -> Vec<u8> {
        if mod_bits == 0 && !needs_event_suffix {
            if prefix_num == 1 {
                format!("\x1b[{final_ch}").into_bytes()
            } else {
                format!("\x1b[{prefix_num}{final_ch}").into_bytes()
            }
        } else if needs_event_suffix {
            format!(
                "\x1b[{prefix_num};{mod_param}:{}{final_ch}",
                event_type.kitty_code()
            )
            .into_bytes()
        } else {
            format!("\x1b[{prefix_num};{mod_param}{final_ch}").into_bytes()
        }
    };

    match sym {
        0xff52 => return Some(functional_suffix(1, 'A')),
        0xff54 => return Some(functional_suffix(1, 'B')),
        XKB_KEY_RIGHT => return Some(functional_suffix(1, 'C')),
        XKB_KEY_LEFT => return Some(functional_suffix(1, 'D')),
        0xff50 => return Some(functional_suffix(1, 'H')),
        0xff57 => return Some(functional_suffix(1, 'F')),
        0xff63 => return Some(functional_suffix(2, '~')),
        XKB_KEY_DELETE => return Some(functional_suffix(3, '~')),
        0xff55 => return Some(functional_suffix(5, '~')),
        0xff56 => return Some(functional_suffix(6, '~')),
        0xffbe..=0xffc1 => {
            let ch = (b'P' + (sym - 0xffbe) as u8) as char;
            return Some(functional_suffix(1, ch));
        }
        0xffc2..=0xffc9 => {
            let codes = [15, 17, 18, 19, 20, 21, 23, 24];
            return Some(functional_suffix(codes[(sym - 0xffc2) as usize], '~'));
        }
        _ => {}
    }

    // Unicode keycode for CSI u
    let keycode = match sym {
        0xff1b => 27u32,
        0xff0d | 0xff8d => 13,
        0xff09 | 0xfe20 => 9,
        XKB_KEY_BACKSPACE => 127,
        _ if (0x20..=0x7e).contains(&sym) => sym.to_ascii_lowercase_u32(),
        _ => {
            if let Ok(s) = std::str::from_utf8(text)
                && let Some(ch) = s.chars().next()
                && s.chars().count() == 1
                && (ch as u32) >= 0x20
            {
                ch.to_lowercase().next().unwrap_or(ch) as u32
            } else if ctrl && let Some(ctrl_b) = evdev_ctrl_byte(evdev_key) {
                match ctrl_b {
                    0x01..=0x1a => (b'a' + ctrl_b - 1) as u32,
                    0x00 => b' ' as u32,
                    _ => return None,
                }
            } else {
                return None;
            }
        }
    };

    let should_csi_u = report_all
        || needs_event_suffix
        || (disambiguate && (keycode == 27 || ctrl || alt || super_mod))
        || (disambiguate && shift && matches!(keycode, 9 | 13 | 127));
    if !should_csi_u {
        return None;
    }

    let mut seq = format!("\x1b[{keycode}");
    let text_part = if report_text
        && event_type != KeyEventType::Release
        && !ctrl
        && !alt
        && !super_mod
        && let Ok(s) = std::str::from_utf8(text)
        && !s.is_empty()
        && !s.chars().any(|c| c.is_control())
    {
        let cps: Vec<String> = s.chars().map(|c| (c as u32).to_string()).collect();
        Some(cps.join(":"))
    } else {
        None
    };

    if mod_bits != 0 || needs_event_suffix || text_part.is_some() {
        seq.push(';');
        seq.push_str(&mod_param.to_string());
        if needs_event_suffix {
            seq.push(':');
            seq.push_str(&event_type.kitty_code().to_string());
        }
        if let Some(tp) = text_part {
            seq.push(';');
            seq.push_str(&tp);
        }
    }
    seq.push('u');
    Some(seq.into_bytes())
}

trait AsciiLowerU32 {
    fn to_ascii_lowercase_u32(self) -> u32;
}
impl AsciiLowerU32 for u32 {
    fn to_ascii_lowercase_u32(self) -> u32 {
        if (b'A' as u32..=b'Z' as u32).contains(&self) {
            self + 32
        } else {
            self
        }
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

    #[test]
    fn full_keyboard_audit_ctrl_alt_shift_and_russian_layout() {
        use std::io::Write;
        let map = br#"xkb_keymap {
            xkb_keycodes { include "evdev+aliases(qwerty)" };
            xkb_types { include "complete" };
            xkb_compatibility { include "complete" };
            xkb_symbols { include "pc+us+ru:2" };
        };"#;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(map).unwrap();
        let mut kb = super::Keyboard::new().unwrap();
        kb.keymap(file.into(), map.len() as u32).unwrap();

        // Key repeat metadata: Backspace/Delete/arrows/letters repeat; LeftShift (42) does not.
        assert!(kb.key_repeats(14)); // Backspace
        assert!(kb.key_repeats(111)); // Delete
        assert!(kb.key_repeats(105)); // Left
        assert!(kb.key_repeats(30)); // A
        assert!(!kb.key_repeats(42)); // Left Shift

        // Ctrl+D, Ctrl+Z, Ctrl+S, Ctrl+Q, Ctrl+A work in BOTH English (group 0) and Russian (group 1).
        for group in [0, 1] {
            kb.modifiers(1 << 2, 0, 0, group);
            assert_eq!(kb.key(30), b"\x01", "Ctrl+A group={group}");
            assert_eq!(kb.key(32), b"\x04", "Ctrl+D group={group}");
            assert_eq!(kb.key(44), b"\x1a", "Ctrl+Z group={group}");
            assert_eq!(kb.key(31), b"\x13", "Ctrl+S group={group}");
            assert_eq!(kb.key(16), b"\x11", "Ctrl+Q group={group}");
        }

        // Shift+Arrows send CSI 1;2A/B/C/D; Shift+Tab sends CSI Z.
        kb.modifiers(1, 0, 0, 0);
        assert_eq!(kb.key(103), b"\x1b[1;2A");
        assert_eq!(kb.key(108), b"\x1b[1;2B");
        assert_eq!(kb.key(106), b"\x1b[1;2C");
        assert_eq!(kb.key(105), b"\x1b[1;2D");
        assert_eq!(kb.key(15), b"\x1b[Z");

        // Alt+b / Alt+f / Alt+Backspace send ESC-prefixed sequences.
        kb.modifiers(1 << 3, 0, 0, 0);
        assert_eq!(kb.key(48), b"\x1bb");
        assert_eq!(kb.key(33), b"\x1bf");
        assert_eq!(kb.key(14), b"\x1b\x7f");
    }
}
