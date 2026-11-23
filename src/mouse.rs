// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mouse reporting requested by terminal applications.
//!
//! Applications enable DEC private modes 1000, 1002 or 1003 and expect the
//! terminal to translate pointer events into escape sequences on the PTY.
//! Mode 1006 selects the SGR encoding; without it the legacy X10 encoding is
//! used. Beyrix keeps reporting in the terminal core so the Wayland layer only
//! has to turn pointer coordinates into cells.

/// Tracking level selected by the application.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Off,
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
            1000 => Some(Self::Click),
            1002 => Some(Self::Drag),
            1003 => Some(Self::Motion),
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
    pub sgr: bool,
    /// `None` only for motion without a pressed button.
    pub button: Option<Button>,
    /// Zero-based cell coordinates.
    pub column: usize,
    pub row: usize,
    pub pressed: bool,
    pub motion: bool,
    pub modifiers: Modifiers,
}

/// Encode one mouse event for the application.
///
/// Motion must be sent with `pressed` set, because SGR uses `M` for motion and
/// `m` exclusively for releases. `None` means the event is not part of the
/// selected tracking level.
pub fn encode(event: Event) -> Option<Vec<u8>> {
    let Event {
        mode,
        sgr,
        button,
        column,
        row,
        pressed,
        motion,
        modifiers,
    } = event;
    if mode == Mode::Off || (motion && mode == Mode::Click) {
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

    if sgr {
        let separator = if pressed || motion { 'M' } else { 'm' };
        return Some(format!("\x1b[<{code};{};{}{separator}", column + 1, row + 1).into_bytes());
    }

    // The legacy encoding stores button, column and row in single bytes. It
    // cannot represent coordinates past 223, so such events are dropped
    // instead of being reported at the wrong cell. Releases use button 3.
    let legacy_button = if button.is_some() && !pressed && !motion {
        3
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(mode: Mode, button: Option<Button>) -> Event {
        Event {
            mode,
            sgr: true,
            button,
            column: 0,
            row: 0,
            pressed: true,
            motion: false,
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn dec_modes_map_to_tracking_levels() {
        assert_eq!(Mode::from_decimal(1000), Some(Mode::Click));
        assert_eq!(Mode::from_decimal(1002), Some(Mode::Drag));
        assert_eq!(Mode::from_decimal(1003), Some(Mode::Motion));
        assert_eq!(Mode::from_decimal(1006), None);
        assert_eq!(Mode::default(), Mode::Off);
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

        let mut drag = event(Mode::Drag, Some(Button::Left));
        drag.motion = true;
        assert_eq!(encode(drag).unwrap(), b"\x1b[<32;1;1M");

        let mut motion = event(Mode::Motion, None);
        motion.motion = true;
        assert_eq!(encode(motion).unwrap(), b"\x1b[<35;1;1M");
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
        press.sgr = false;
        assert_eq!(encode(press).unwrap(), b"\x1b[M !!");

        press.pressed = false;
        assert_eq!(encode(press).unwrap(), b"\x1b[M#!!");

        let mut drag = event(Mode::Drag, Some(Button::Left));
        drag.sgr = false;
        drag.column = 1;
        drag.row = 1;
        drag.motion = true;
        assert_eq!(encode(drag).unwrap(), b"\x1b[M@\"\"");
    }

    #[test]
    fn legacy_coordinates_beyond_223_are_dropped() {
        let mut press = event(Mode::Click, Some(Button::Left));
        press.sgr = false;
        press.column = 223;
        assert!(encode(press).is_none());

        press.sgr = true;
        press.column = 500;
        press.row = 500;
        assert!(encode(press).is_some());
    }
}
