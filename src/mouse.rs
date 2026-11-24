// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mouse reporting requested by terminal applications.
//!
//! Applications enable DEC private modes 9 (X10), 1000 (Click), 1002 (Drag),
//! or 1003 (Motion) and select an encoding via 1005 (UTF-8), 1006 (SGR),
//! 1015 (URXVT), or 1016 (SGR-Pixels). Without an extended encoding mode,
//! legacy X10/normal 6-byte encoding is used.

/// Tracking level selected by the application.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Off,
    /// X10 compatibility mode: report button presses only, no modifiers/releases (DEC mode 9).
    X10,
    /// Report button presses and releases (DEC mode 1000).
    Click,
    /// Additionally report motion while a button is held (DEC mode 1002).
    Drag,
    /// Report all motion, including without a pressed button (DEC mode 1003).
    Motion,
}

impl Mode {
    pub fn from_decimal(mode: usize) -> Option<Self> {
        match mode {
            9 => Some(Self::X10),
            1000 => Some(Self::Click),
            1002 => Some(Self::Drag),
            1003 => Some(Self::Motion),
            _ => None,
        }
    }
}

/// Coordinate encoding selected by the application.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Legacy,
    /// UTF-8 extended coordinates (DEC mode 1005).
    Utf8,
    /// SGR extended coordinates in cells (DEC mode 1006).
    Sgr,
    /// URXVT decimal coordinates in cells (DEC mode 1015).
    Urxvt,
    /// SGR extended coordinates in physical/logical pixels (DEC mode 1016).
    SgrPixels,
}

impl Encoding {
    pub fn from_decimal(mode: usize) -> Option<Self> {
        match mode {
            1005 => Some(Self::Utf8),
            1006 => Some(Self::Sgr),
            1015 => Some(Self::Urxvt),
            1016 => Some(Self::SgrPixels),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    Back,
    Forward,
}

impl Button {
    fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
            Self::WheelUp => 64,
            Self::WheelDown => 65,
            Self::WheelLeft => 66,
            Self::WheelRight => 67,
            Self::Back => 8,
            Self::Forward => 9,
        }
    }

    pub fn from_linux(button: u32) -> Option<Self> {
        // Linux evdev button codes as delivered by wl_pointer.
        match button {
            0x110 => Some(Self::Left),
            0x111 => Some(Self::Right),
            0x112 => Some(Self::Middle),
            0x113 => Some(Self::Back),
            0x114 => Some(Self::Forward),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Modifiers {
    fn bits(self) -> u8 {
        u8::from(self.shift) * 4 + u8::from(self.alt) * 8 + u8::from(self.ctrl) * 16
    }
}

/// One pointer event as seen by the terminal core.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub mode: Mode,
    pub encoding: Encoding,
    /// `None` only for motion without a pressed button.
    pub button: Option<Button>,
    /// Zero-based cell coordinates.
    pub column: usize,
    pub row: usize,
    /// Optional pixel coordinates relative to the terminal viewport origin (for mode 1016).
    pub pixel_x: usize,
    pub pixel_y: usize,
    pub pressed: bool,
    pub motion: bool,
    pub modifiers: Modifiers,
}

fn push_utf8_coord(out: &mut Vec<u8>, value: usize) -> bool {
    // DEC mode 1005 encodes 1-based coordinate + 32 as a UTF-8 codepoint (max 2047 -> coord 2015).
    let Some(cp) = value.checked_add(32).and_then(|v| u32::try_from(v).ok()) else {
        return false;
    };
    if cp > 2047 {
        return false;
    }
    let Some(ch) = char::from_u32(cp) else {
        return false;
    };
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    true
}

/// Encode one mouse event for the application.
///
/// Motion must be sent with `pressed` set, because SGR uses `M` for motion and
/// `m` exclusively for releases. `None` means the event is not part of the
/// selected tracking level.
pub fn encode(event: Event) -> Option<Vec<u8>> {
    let Event {
        mode,
        encoding,
        button,
        column,
        row,
        pixel_x,
        pixel_y,
        pressed,
        motion,
        modifiers,
    } = event;
    if mode == Mode::Off {
        return None;
    }
    if mode == Mode::X10 {
        // X10 only reports button press events (no releases, no motion, no modifiers).
        if motion || !pressed {
            return None;
        }
        let btn = button?;
        let base = btn.code();
        if base > 2 {
            return None;
        }
        let legacy_column = u8::try_from(column + 1).ok()?;
        let legacy_row = u8::try_from(row + 1).ok()?;
        if legacy_column > 223 || legacy_row > 223 {
            return None;
        }
        return Some(vec![
            27,
            b'[',
            b'M',
            32u8.saturating_add(base),
            32u8.saturating_add(legacy_column),
            32u8.saturating_add(legacy_row),
        ]);
    }
    if motion && mode == Mode::Click {
        return None;
    }
    if motion && mode == Mode::Drag && button.is_none() {
        return None;
    }
    if !motion && button.is_none() {
        return None;
    }
    let base = button.map_or(3, Button::code);
    let motion_bit = if motion { 32 } else { 0 };
    let code = base
        .saturating_add(modifiers.bits())
        .saturating_add(motion_bit);

    match encoding {
        Encoding::Sgr => {
            let separator = if pressed || motion { 'M' } else { 'm' };
            Some(format!("\x1b[<{code};{};{}{separator}", column + 1, row + 1).into_bytes())
        }
        Encoding::SgrPixels => {
            let separator = if pressed || motion { 'M' } else { 'm' };
            Some(format!("\x1b[<{code};{};{}{separator}", pixel_x, pixel_y).into_bytes())
        }
        Encoding::Urxvt => {
            let urxvt_button = if button.is_some() && !pressed && !motion {
                3u8.saturating_add(modifiers.bits())
            } else {
                code
            };
            Some(
                format!(
                    "\x1b[{};{};{}M",
                    32u16 + u16::from(urxvt_button),
                    column + 1,
                    row + 1
                )
                .into_bytes(),
            )
        }
        Encoding::Utf8 => {
            let legacy_button = if button.is_some() && !pressed && !motion {
                3u8.saturating_add(modifiers.bits())
            } else {
                code
            };
            let mut out = vec![27, b'[', b'M'];
            if !push_utf8_coord(&mut out, usize::from(legacy_button))
                || !push_utf8_coord(&mut out, column + 1)
                || !push_utf8_coord(&mut out, row + 1)
            {
                return None;
            }
            Some(out)
        }
        Encoding::Legacy => {
            // The legacy encoding stores button, column and row in single bytes. It
            // cannot represent coordinates past 223, so such events are dropped
            // instead of being reported at the wrong cell. Releases use button 3.
            let legacy_button = if button.is_some() && !pressed && !motion {
                3u8.saturating_add(modifiers.bits())
            } else {
                code
            };
            let legacy_column = u8::try_from(column + 1).ok()?;
            let legacy_row = u8::try_from(row + 1).ok()?;
            if legacy_column > 223 || legacy_row > 223 {
                return None;
            }
            Some(vec![
                27,
                b'[',
                b'M',
                32u8.saturating_add(legacy_button),
                32u8.saturating_add(legacy_column),
                32u8.saturating_add(legacy_row),
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(mode: Mode, button: Option<Button>) -> Event {
        Event {
            mode,
            encoding: Encoding::Sgr,
            button,
            column: 0,
            row: 0,
            pixel_x: 0,
            pixel_y: 0,
            pressed: true,
            motion: false,
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn dec_modes_map_to_tracking_levels_and_encodings() {
        assert_eq!(Mode::from_decimal(9), Some(Mode::X10));
        assert_eq!(Mode::from_decimal(1000), Some(Mode::Click));
        assert_eq!(Mode::from_decimal(1002), Some(Mode::Drag));
        assert_eq!(Mode::from_decimal(1003), Some(Mode::Motion));
        assert_eq!(Mode::from_decimal(1006), None);
        assert_eq!(Mode::default(), Mode::Off);

        assert_eq!(Encoding::from_decimal(1005), Some(Encoding::Utf8));
        assert_eq!(Encoding::from_decimal(1006), Some(Encoding::Sgr));
        assert_eq!(Encoding::from_decimal(1015), Some(Encoding::Urxvt));
        assert_eq!(Encoding::from_decimal(1016), Some(Encoding::SgrPixels));
    }

    #[test]
    fn linux_buttons_map_to_terminal_buttons() {
        assert_eq!(Button::from_linux(0x110), Some(Button::Left));
        assert_eq!(Button::from_linux(0x111), Some(Button::Right));
        assert_eq!(Button::from_linux(0x112), Some(Button::Middle));
        assert_eq!(Button::from_linux(0x113), Some(Button::Back));
        assert_eq!(Button::from_linux(0x114), Some(Button::Forward));
        assert_eq!(Button::from_linux(0x115), None);
    }

    #[test]
    fn sgr_reports_press_release_wheel_and_motion() {
        let mut press = event(Mode::Click, Some(Button::Left));
        press.column = 2;
        press.row = 3;
        assert_eq!(encode(press).unwrap(), b"\x1b[<0;3;4M");

        press.pressed = false;
        assert_eq!(encode(press).unwrap(), b"\x1b[<0;3;4m");

        let wheel = event(Mode::Click, Some(Button::WheelDown));
        assert_eq!(encode(wheel).unwrap(), b"\x1b[<65;1;1M");

        let hwheel = event(Mode::Click, Some(Button::WheelRight));
        assert_eq!(encode(hwheel).unwrap(), b"\x1b[<67;1;1M");

        let mut drag = event(Mode::Drag, Some(Button::Left));
        drag.motion = true;
        assert_eq!(encode(drag).unwrap(), b"\x1b[<32;1;1M");

        let mut motion = event(Mode::Motion, None);
        motion.motion = true;
        assert_eq!(encode(motion).unwrap(), b"\x1b[<35;1;1M");
    }

    #[test]
    fn sgr_pixels_urxvt_utf8_and_x10_encodings() {
        let mut px = event(Mode::Click, Some(Button::Left));
        px.encoding = Encoding::SgrPixels;
        px.pixel_x = 142;
        px.pixel_y = 288;
        assert_eq!(encode(px).unwrap(), b"\x1b[<0;142;288M");

        let mut urxvt = event(Mode::Click, Some(Button::Right));
        urxvt.encoding = Encoding::Urxvt;
        urxvt.column = 10;
        urxvt.row = 20;
        assert_eq!(encode(urxvt).unwrap(), b"\x1b[34;11;21M");

        let mut utf8 = event(Mode::Click, Some(Button::Left));
        utf8.encoding = Encoding::Utf8;
        utf8.column = 300;
        utf8.row = 10;
        let bytes = encode(utf8).unwrap();
        assert!(bytes.starts_with(b"\x1b[M "));
        assert!(bytes.len() > 6);

        let mut x10 = event(Mode::X10, Some(Button::Left));
        x10.encoding = Encoding::Legacy;
        assert_eq!(encode(x10).unwrap(), b"\x1b[M !!");
        x10.pressed = false;
        assert!(encode(x10).is_none());
    }

    #[test]
    fn sgr_includes_shift_alt_and_control_bits() {
        let mut press = event(Mode::Click, Some(Button::Left));
        press.modifiers = Modifiers {
            shift: true,
            alt: true,
            ctrl: true,
        };
        assert_eq!(encode(press).unwrap(), b"\x1b[<28;1;1M");
    }

    #[test]
    fn tracking_level_gates_events() {
        let mut click = event(Mode::Click, Some(Button::Left));
        click.motion = true;
        assert!(encode(click).is_none());

        let mut drag_without_button = event(Mode::Drag, None);
        drag_without_button.motion = true;
        assert!(encode(drag_without_button).is_none());

        let mut off = event(Mode::Off, Some(Button::Left));
        off.mode = Mode::Off;
        assert!(encode(off).is_none());
    }

    #[test]
    fn legacy_encoding_uses_x10_offsets_and_release_button() {
        let mut press = event(Mode::Click, Some(Button::Left));
        press.encoding = Encoding::Legacy;
        assert_eq!(encode(press).unwrap(), b"\x1b[M !!");

        press.pressed = false;
        assert_eq!(encode(press).unwrap(), b"\x1b[M#!!");

        let mut drag = event(Mode::Drag, Some(Button::Left));
        drag.encoding = Encoding::Legacy;
        drag.column = 1;
        drag.row = 1;
        drag.motion = true;
        assert_eq!(encode(drag).unwrap(), b"\x1b[M@\"\"");
    }

    #[test]
    fn legacy_coordinates_beyond_223_are_dropped() {
        let mut press = event(Mode::Click, Some(Button::Left));
        press.encoding = Encoding::Legacy;
        press.column = 223;
        assert!(encode(press).is_none());

        press.encoding = Encoding::Sgr;
        press.column = 500;
        press.row = 500;
        assert!(encode(press).is_some());
    }
}
