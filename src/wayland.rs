// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::bedit::{Action as EditorAction, BEdit, OpenRequest, PasteTarget};
use crate::editor::{Command as EditorCommand, Direction, Metrics};
use crate::gl::{GlBackend, PixelRect};
use crate::gpu::{self, ActiveBackend, RendererChoice};
use crate::pty::Pty;
use crate::render;
use crate::terminal::{Screen, Terminal};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
        wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
};

use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const WIDTH: i32 = 800;
const HEIGHT: i32 = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Destination {
    Terminal,
    Editor(PasteTarget),
}

struct Transfer {
    file: File,
    bytes: Vec<u8>,
    offset: usize,
    writing: bool,
    bracketed: bool,
    destination: Destination,
}

struct KeyPress {
    key: u32,
    sym: u32,
    control: bool,
    shift: bool,
    alt: bool,
    text: String,
}

fn metrics(geometry: render::Geometry, sizes: SurfaceSizes) -> Metrics {
    Metrics {
        cell_width: geometry.cell_width,
        cell_height: geometry.cell_height,
        padding: geometry.padding,
        logical_per_pixel: sizes.logical_per_pixel(),
    }
}

fn movement(
    direction: Direction,
    press: &KeyPress,
    by_word: bool,
    document: bool,
) -> EditorCommand {
    EditorCommand::Move {
        direction,
        extend: press.shift,
        by_word,
        document,
    }
}

fn editor_command(press: &KeyPress, confirming: bool) -> Option<EditorCommand> {
    const KEY_LEFT: u32 = 0xff51;
    const KEY_UP: u32 = 0xff52;
    const KEY_RIGHT: u32 = 0xff53;
    const KEY_DOWN: u32 = 0xff54;
    const KEY_HOME: u32 = 0xff50;
    const KEY_END: u32 = 0xff57;
    const KEY_PAGE_UP: u32 = 0xff55;
    const KEY_PAGE_DOWN: u32 = 0xff56;
    const KEY_BACKSPACE: u32 = 0xff08;
    const KEY_DELETE: u32 = 0xffff;
    const KEY_RETURN: u32 = 0xff0d;
    const KEY_KP_ENTER: u32 = 0xff8d;
    const KEY_TAB: u32 = 0xff09;
    const KEY_ESCAPE: u32 = 0xff1b;
    const KEY_INSERT: u32 = 0xff63;

    if confirming {
        if press.control || press.alt {
            return None;
        }
        if press.sym == KEY_ESCAPE {
            return Some(EditorCommand::ConfirmCancel);
        }
        return match press.key {
            46 => Some(EditorCommand::ConfirmCancel),  // C
            49 => Some(EditorCommand::ConfirmDiscard), // N
            21 => Some(EditorCommand::ConfirmSave),    // Y
            _ => None,
        };
    }

    if press.control {
        // Physical keycodes keep Ctrl shortcuts independent of the layout.
        match press.key {
            31 => return Some(EditorCommand::Save),                // S
            44 if press.shift => return Some(EditorCommand::Redo), // Z
            44 => return Some(EditorCommand::Undo),                // Z
            21 => return Some(EditorCommand::Redo),                // Y
            46 => return Some(EditorCommand::Copy),                // C
            45 => return Some(EditorCommand::Cut),                 // X
            47 => return Some(EditorCommand::PasteRequest),        // V
            30 => return Some(EditorCommand::SelectAll),           // A
            110 => return Some(EditorCommand::PasteRequest),       // Insert
            _ => {}
        }
        return match press.sym {
            KEY_LEFT => Some(movement(Direction::Left, press, true, false)),
            KEY_RIGHT => Some(movement(Direction::Right, press, true, false)),
            KEY_UP => Some(movement(Direction::Up, press, false, false)),
            KEY_DOWN => Some(movement(Direction::Down, press, false, false)),
            KEY_HOME => Some(movement(Direction::Home, press, false, true)),
            KEY_END => Some(movement(Direction::End, press, false, true)),
            _ => None,
        };
    }
    let command = match press.sym {
        KEY_ESCAPE => return Some(EditorCommand::Escape),
        KEY_RETURN | KEY_KP_ENTER => return Some(EditorCommand::Enter),
        KEY_TAB => return Some(EditorCommand::Tab),
        KEY_BACKSPACE => return Some(EditorCommand::Backspace),
        KEY_DELETE => return Some(EditorCommand::Delete),
        KEY_INSERT if press.shift => return Some(EditorCommand::PasteRequest),
        KEY_LEFT => movement(Direction::Left, press, false, false),
        KEY_RIGHT => movement(Direction::Right, press, false, false),
        KEY_UP => movement(Direction::Up, press, false, false),
        KEY_DOWN => movement(Direction::Down, press, false, false),
        KEY_HOME => movement(Direction::Home, press, false, false),
        KEY_END => movement(Direction::End, press, false, false),
        KEY_PAGE_UP => movement(Direction::PageUp, press, false, false),
        KEY_PAGE_DOWN => movement(Direction::PageDown, press, false, false),
        _ => {
            if press.alt
                || press.text.is_empty()
                || press
                    .text
                    .chars()
                    .any(|ch| ch.is_control() && ch != '\t' && ch != '\n')
            {
                return None;
            }
            EditorCommand::Insert(press.text.clone())
        }
    };
    Some(command)
}
fn set_nonblocking(fd: &impl AsRawFd) -> std::io::Result<()> {
    let fd = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
fn paste_bytes(bytes: &[u8], bracketed: bool) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    if bracketed {
        // Strip embedded bracketed-paste end/start markers to prevent escape
        // breakout attacks when pasting untrusted text.
        let mut clean = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i..].starts_with(b"\x1b[200~") || bytes[i..].starts_with(b"\x1b[201~") {
                i += 6;
            } else {
                clean.push(bytes[i]);
                i += 1;
            }
        }
        [b"\x1b[200~".as_slice(), &clean, b"\x1b[201~".as_slice()].concat()
    } else {
        bytes.to_vec()
    }
}

#[derive(Clone, Debug)]
struct RepeatKey {
    key: u32,
    serial: u32,
    next_fire: std::time::Instant,
}

#[derive(Clone, Debug)]
struct PendingMultilinePaste {
    bytes: Vec<u8>,
    expires_at: std::time::Instant,
}
fn wheel_rows(remainder: f64, steps: f64, speed: usize) -> (isize, f64) {
    let total = remainder - steps * speed as f64;
    if !total.is_finite() {
        return (0, 0.0);
    }
    (total as isize, total.fract())
}

// Two compositor buffers, mapped directly for painting. Bound both the
// allocation size and the total number of buffers across resize generations.
const MAX_SHM_BUFFERS: usize = 2;
const MAX_SHM_BUFFER_BYTES: i32 = 256 * 1024 * 1024;

fn shm_layout((width, height): (i32, i32)) -> Option<(i32, i32)> {
    let stride = width.checked_mul(4)?;
    let size = stride.checked_mul(height)?;
    (width > 0 && height > 0 && size <= MAX_SHM_BUFFER_BYTES).then_some((stride, size))
}

struct FrameBuffer {
    buffer: wl_buffer::WlBuffer,
    mapping: NonNull<u8>,
    len: usize,
    busy: bool,
    id: u64,
    generation: u64,
}

impl FrameBuffer {
    fn pixels(&mut self) -> &mut [u8] {
        // Only a released or newly allocated buffer may be written.
        assert!(!self.busy);
        unsafe { std::slice::from_raw_parts_mut(self.mapping.as_ptr(), self.len) }
    }
}

impl Drop for FrameBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        unsafe { libc::munmap(self.mapping.as_ptr().cast(), self.len) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PressedKeyRoute {
    sent_to_pty: bool,
    kitty_report_release: bool,
}

#[derive(Default)]
struct State {
    running: bool,
    control: Option<crate::control::Control>,
    debug_input: bool,
    seat: Option<wl_seat::WlSeat>,
    data_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    data_device: Option<wl_data_device::WlDataDevice>,
    data_offer: Option<wl_data_offer::WlDataOffer>,
    data_source: Option<wl_data_source::WlDataSource>,
    transfers: Vec<Transfer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    xkb: Option<crate::keyboard::Keyboard>,
    keyboard_focus: bool,
    repeat_rate: i32,
    repeat_delay: i32,
    repeat_key: Option<RepeatKey>,
    pending_multiline_paste: Option<PendingMultilinePaste>,
    #[cfg(test)]
    test_clipboard: Option<String>,
    blink_epoch: Option<std::time::Instant>,
    blink_on: bool,
    pointer: Option<wl_pointer::WlPointer>,
    pointer_focus: bool,
    pointer_position: (f64, f64),
    wheel_axis: f64,
    wheel_steps: Option<f64>,
    wheel_h_axis: f64,
    wheel_h_steps: Option<f64>,
    wheel_remainder: f64,
    wheel_mouse_remainder: f64,
    wheel_h_mouse_remainder: f64,
    last_serial: u32,
    default_font_size: f32,
    pending_osc52_clipboard: Option<String>,
    pressed_keys: std::collections::HashMap<u32, PressedKeyRoute>,
    mouse_button: Option<crate::mouse::Button>,
    drag_scroll_at: Option<std::time::Instant>,
    selection: crate::selection::Selection,
    input: VecDeque<u8>,
    input_error: Option<String>,

    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,

    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,

    buffers: Vec<FrameBuffer>,
    next_buffer_id: u64,
    generation: u64,
    width: i32,
    height: i32,
    proposed_size: Option<(i32, i32)>,
    pending_size: Option<(i32, i32)>,
    configured: bool,
    dirty: bool,
    frame_pending: bool,
    terminal: Option<Terminal>,
    editor: Option<BEdit>,
    standalone_editor: bool,
    renderer: Option<render::Renderer>,
    backend: ActiveBackend,
    gl: Option<GlBackend>,
    scale: Scale,
    pending_scale: Option<Scale>,
    damage_full: bool,
    damage_hashes: Vec<u64>,
    damage_signature: Option<TerminalSignature>,
    damage_history: VecDeque<RowDamage>,
    dump_deadline: Option<std::time::Instant>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    viewport: Option<wp_viewport::WpViewport>,
    fractional_manager: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    fractional_scale: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    pty: Option<Pty>,

    bus: crate::events::EventBus,
    platform_config: crate::config::PlatformConfig,
    ui_compositor: crate::widgets::compositor::UiCompositor,
    settings_overlay: crate::widgets::editor_ui::SettingsOverlay,
    widgets: Option<crate::widgets::WidgetManager>,
    media: Option<crate::media::MediaManager>,
    background: Option<crate::background::BackgroundEngine>,
    xray: Option<crate::xray::XRayEngine>,
    xray_hits: Vec<crate::widgets::ui::UiHitTarget>,
    platform_dirty: bool,
}

pub fn run() -> Result<(), Box<dyn Error>> {
    run_with(None)
}

pub fn run_editor(editor: BEdit) -> Result<(), Box<dyn Error>> {
    run_with(Some(editor))
}

fn run_with(editor: Option<BEdit>) -> Result<(), Box<dyn Error>> {
    let connection =
        Connection::connect_to_env().map_err(|e| format!("connect to Wayland: {e}"))?;
    let mut event_queue = connection.new_event_queue();
    let qh = event_queue.handle();

    let display = connection.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = State {
        running: true,
        standalone_editor: editor.is_some(),
        editor,
        control: Some(crate::control::Control::new().map_err(|e| format!("control socket: {e}"))?),
        repeat_rate: 25,
        repeat_delay: 400,
        blink_epoch: Some(std::time::Instant::now()),
        blink_on: true,
        width: WIDTH,
        height: HEIGHT,
        dump_deadline: crate::compat::env("BEYRIX_GL_DUMP")
            .map(|_| std::time::Instant::now() + Duration::from_millis(3000)),
        debug_input: crate::compat::env("BEYRIX_DEBUG_INPUT").as_deref()
            == Some(std::ffi::OsStr::new("1")),
        ..Default::default()
    };

    // Получаем список доступных Wayland-интерфейсов.
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| format!("Wayland registry roundtrip: {e}"))?;

    if state.compositor.is_none() || state.shm.is_none() || state.wm_base.is_none() {
        return Err("Wayland compositor lacks required protocols".into());
    }

    let display_ptr = display.id().as_ptr().cast();
    state
        .create_window(&qh, display_ptr)
        .map_err(|e| format!("create window and PTY: {e}"))?;
    connection
        .flush()
        .map_err(|e| format!("flush Wayland requests: {e}"))?;

    let mut fds = Vec::new();
    while state.running {
        event_queue
            .dispatch_pending(&mut state)
            .map_err(|e| format!("dispatch Wayland events: {e}"))?;
        if let Some(error) = state.input_error.take() {
            return Err(error.into());
        }
        if let Some(control) = &state.control {
            match control.receive() {
                Ok(Some(crate::control::Message::Theme(name))) => {
                    if let Err(e) = state.apply_theme(&name) {
                        eprintln!("theme update: {e}");
                    }
                }
                Ok(Some(crate::control::Message::Open(path))) => {
                    state.open_editor(OpenRequest {
                        root: None,
                        files: vec![path],
                    });
                }
                Ok(Some(crate::control::Message::Workspace(request))) => state.open_editor(request),
                Ok(None) => {}
                Err(e) => eprintln!("control socket: {e}"),
            }
        }
        if let Some(editor) = &mut state.editor
            && editor.poll()
        {
            state.dirty = true;
            if let Some(toplevel) = &state.toplevel {
                toplevel.set_title(editor.title());
            }
        }
        let blink_on = state
            .blink_epoch
            .is_none_or(|t| (t.elapsed().as_millis() / 600) % 2 == 0);
        if blink_on != state.blink_on {
            state.blink_on = blink_on;
            if state.keyboard_focus && state.editor.is_some() {
                state.dirty = true;
            }
            if state.keyboard_focus
                && state.editor.is_none()
                && let (Some(renderer), Some(terminal)) = (&state.renderer, &state.terminal)
                && terminal.screen().cursor_visible
                && terminal.screen().view_offset() == 0
                && renderer.cursor_blinks(terminal.screen())
            {
                state.dirty = true;
            }
        }
        if state
            .dump_deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            state.dump_deadline = None;
            state.damage_full = true;
            state.dirty = true;
        }
        state.pump_key_repeat(Some(&qh));
        state.pump_clipboard();
        if let Some(osc52_text) = state.pending_osc52_clipboard.take() {
            state.set_clipboard(osc52_text, state.last_serial, &qh);
        }
        let size = state.pending_size.take();
        let scale = state.pending_scale.take();
        if size.is_some() || scale.is_some() {
            let logical = size.unwrap_or((state.width, state.height));
            state
                .resize(logical, scale.unwrap_or(state.scale))
                .map_err(|e| format!("resize window: {e}"))?;
        }
        state.update_drag(true);
        state.pump_platform();
        state
            .redraw(&qh)
            .map_err(|e| format!("update shared buffer: {e}"))?;
        let flush_pending = match connection.flush() {
            Ok(()) => false,
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock =>
            {
                true
            }
            Err(error) => return Err(format!("flush Wayland requests: {error}").into()),
        };
        let Some(read_guard) = event_queue.prepare_read() else {
            continue;
        };
        // Each ready descriptor gets serviced on the next pass before repainting.
        let pty_fd = state.pty.as_ref().map(|pty| pty.master.as_raw_fd());
        fds.clear();
        fds.extend([
            libc::pollfd {
                fd: connection.as_fd().as_raw_fd(),
                events: libc::POLLIN | if flush_pending { libc::POLLOUT } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: pty_fd.unwrap_or(-1),
                events: libc::POLLIN
                    | if state.input.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    },
                revents: 0,
            },
        ]);
        if let Some(control) = &state.control {
            fds.push(libc::pollfd {
                fd: control.socket().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        fds.extend(state.transfers.iter().map(|transfer| libc::pollfd {
            fd: transfer.file.as_raw_fd(),
            events: if transfer.writing {
                libc::POLLOUT
            } else {
                libc::POLLIN
            },
            revents: 0,
        }));
        let poll_timeout_ms = state.next_poll_timeout_ms();
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, poll_timeout_ms) };
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().into());
        }
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            read_guard
                .read()
                .map_err(|e| format!("read Wayland events: {e}"))?;
        } else {
            drop(read_guard);
        }
        if fds[1].revents & libc::POLLOUT != 0 {
            state.write_input()?;
        }
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            if state.debug_input {
                eprintln!("[output] PTY poll revents={:#x}", fds[1].revents);
            }
            state.read_pty().map_err(|e| format!("read PTY: {e}"))?;
        }
    }

    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalSignature {
    width: usize,
    height: usize,
    columns: usize,
    rows: usize,
    visible_start: usize,
    layout_offset: usize,
    view_offset: usize,
    graphics_revision: u64,
    progress: Option<(u8, u8)>,
}

#[derive(Clone, Debug, Default)]
struct RowDamage {
    full: bool,
    ranges: Vec<(usize, usize)>,
}

impl RowDamage {
    fn full() -> Self {
        Self {
            full: true,
            ranges: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        !self.full && self.ranges.is_empty()
    }

    fn merge(&mut self, other: &RowDamage) {
        if self.full {
            return;
        }
        if other.full {
            self.full = true;
            self.ranges.clear();
            return;
        }
        for range in &other.ranges {
            self.ranges.push(*range);
        }
        self.normalize();
    }

    fn normalize(&mut self) {
        if self.ranges.len() < 2 {
            return;
        }
        self.ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(self.ranges.len());
        for (start, end) in self.ranges.drain(..) {
            match merged.last_mut() {
                Some((_, last_end)) if start <= *last_end => {
                    *last_end = (*last_end).max(end);
                }
                _ => merged.push((start, end)),
            }
        }
        self.ranges = merged;
    }
}

fn hash_mix(hash: &mut u64, value: u64) {
    *hash ^= value;
    *hash = hash.wrapping_mul(0x100000001b3);
}

type CursorHashState = (
    Option<usize>,
    (
        bool,
        bool,
        bool,
        Option<crate::terminal::CursorStyle>,
        usize,
    ),
);

fn cursor_state(screen: &Screen, focused: bool, blink_on: bool) -> CursorHashState {
    let row =
        (screen.cursor_visible && screen.view_offset() == 0).then_some(screen.cursor_display_row());
    (
        row,
        (
            focused,
            blink_on,
            screen.cursor_visible,
            screen.cursor_style,
            render::Renderer::cursor_x(screen),
        ),
    )
}

fn row_hash(
    screen: &Screen,
    y: usize,
    selection: &crate::selection::Selection,
    cursor: CursorHashState,
) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for x in 0..screen.columns {
        let Some(cell) = screen.get(x, y) else {
            hash_mix(&mut hash, 0xff);
            continue;
        };
        hash_mix(&mut hash, cell.character as u64);
        for ch in &cell.combining {
            hash_mix(&mut hash, *ch as u64);
        }
        hash_mix(
            &mut hash,
            u64::from(cell.wide)
                | (u64::from(cell.continuation) << 1)
                | (u64::from(cell.written) << 2)
                | (u64::from(cell.attrs.0) << 8)
                | ((cell.underline as u64) << 24)
                | (u64::from(cell.hyperlink_id) << 32),
        );
        if let Some(ulc) = cell.underline_color {
            hash_mix(
                &mut hash,
                u32::from_le_bytes([ulc[0], ulc[1], ulc[2], 1]) as u64,
            );
        }
        hash_mix(
            &mut hash,
            u32::from_le_bytes([cell.fg[0], cell.fg[1], cell.fg[2], 0]) as u64,
        );
        hash_mix(
            &mut hash,
            u32::from_le_bytes([cell.bg[0], cell.bg[1], cell.bg[2], 0]) as u64,
        );
        let origin = |origin: crate::terminal::ColorOrigin| match origin {
            crate::terminal::ColorOrigin::DefaultForeground => 0u64,
            crate::terminal::ColorOrigin::DefaultBackground => 1,
            crate::terminal::ColorOrigin::Indexed(index) => 2 + index as u64,
            crate::terminal::ColorOrigin::Rgb => 1000,
        };
        hash_mix(&mut hash, origin(cell.fg_origin));
        hash_mix(&mut hash, origin(cell.bg_origin));
        hash_mix(&mut hash, u64::from(selection.contains(screen, y, x)));
    }
    if cursor.0 == Some(y) {
        let (focused, blink_on, visible, style, column) = cursor.1;
        hash_mix(&mut hash, u64::from(focused));
        hash_mix(&mut hash, u64::from(blink_on));
        hash_mix(&mut hash, u64::from(visible));
        hash_mix(
            &mut hash,
            style.map_or(0, |style| {
                let shape = match style.shape {
                    crate::theme::CursorShape::Block => 1,
                    crate::theme::CursorShape::Beam => 2,
                    crate::theme::CursorShape::Underline => 3,
                };
                shape | (u64::from(style.blink) << 4)
            }),
        );
        hash_mix(&mut hash, column as u64);
    }
    hash
}

#[allow(clippy::too_many_arguments)]
fn terminal_damage(
    screen: &Screen,
    selection: &crate::selection::Selection,
    focused: bool,
    blink_on: bool,
    logical: (usize, usize),
    hashes: &mut Vec<u64>,
    signature: &mut Option<TerminalSignature>,
    force_full: bool,
) -> RowDamage {
    let current = TerminalSignature {
        width: logical.0,
        height: logical.1,
        columns: screen.columns,
        rows: screen.rows,
        visible_start: screen.visible_start(),
        layout_offset: screen.layout_offset(),
        view_offset: screen.view_offset(),
        graphics_revision: screen.graphics.revision,
        progress: screen.progress,
    };
    let cursor = cursor_state(screen, focused, blink_on);
    if force_full || signature.as_ref() != Some(&current) || hashes.len() != screen.rows {
        *signature = Some(current);
        hashes.clear();
        hashes.extend((0..screen.rows).map(|y| row_hash(screen, y, selection, cursor)));
        return RowDamage::full();
    }
    let mut damage = RowDamage::default();
    for y in 0..screen.rows {
        let hash = row_hash(screen, y, selection, cursor);
        if hashes.get(y).copied() != Some(hash) {
            if let Some((_, end)) = damage.ranges.last_mut()
                && *end == y
            {
                *end = y + 1;
            } else {
                damage.ranges.push((y, y + 1));
            }
        }
        if let Some(slot) = hashes.get_mut(y) {
            *slot = hash;
        } else {
            hashes.push(hash);
        }
    }
    damage.normalize();
    damage
}

/// Wayland fractional scale is expressed in units of 1/120.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Scale(u32);

impl Default for Scale {
    fn default() -> Self {
        Self(120)
    }
}

impl Scale {
    fn factor(self) -> f32 {
        self.0 as f32 / 120.0
    }

    fn physical(self, logical: i32) -> i32 {
        // Round positive dimensions to the nearest pixel, with exact ties up.
        ((i64::from(logical) * i64::from(self.0) + 60) / 120).clamp(1, i64::from(i32::MAX)) as i32
    }
}

/// Sizes for one surface commit. The viewport destination is surface-local;
/// both backends draw into a buffer sized in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceSizes {
    logical: (i32, i32),
    physical: (i32, i32),
    scale: Scale,
}

impl SurfaceSizes {
    fn new(logical: (i32, i32), scale: Scale) -> Self {
        let logical = (logical.0.max(1), logical.1.max(1));
        Self {
            logical,
            physical: (scale.physical(logical.0), scale.physical(logical.1)),
            scale,
        }
    }

    fn buffer_scale(self, viewport: bool) -> i32 {
        if viewport {
            1
        } else {
            (self.scale.0 / 120) as i32
        }
    }

    fn buffer_dimensions(self) -> (usize, usize) {
        (self.physical.0 as usize, self.physical.1 as usize)
    }

    fn logical_per_pixel(self) -> (f64, f64) {
        (
            f64::from(self.logical.0) / f64::from(self.physical.0),
            f64::from(self.logical.1) / f64::from(self.physical.1),
        )
    }

    fn physical_point(self, (x, y): (f64, f64)) -> (f64, f64) {
        let (lx, ly) = self.logical_per_pixel();
        (x / lx, y / ly)
    }

    fn geometry(self, renderer: &render::Renderer) -> render::Geometry {
        let (width, height) = self.buffer_dimensions();
        renderer
            .geometry(self.scale.factor())
            .for_window(width, height)
    }

    fn grid_size(self, renderer: &render::Renderer) -> (usize, usize) {
        let (width, height) = self.buffer_dimensions();
        renderer
            .geometry(self.scale.factor())
            .grid_size(width, height)
    }
}

/// Apply one xdg_toplevel configure. A zero dimension means "no preference";
/// keep the previous value for that axis instead of collapsing the surface to
/// zero or mixing a fresh width with a stale height.
fn resolve_toplevel_configure(previous: (i32, i32), width: i32, height: i32) -> (i32, i32) {
    let previous = (previous.0.max(1), previous.1.max(1));
    (
        if width > 0 { width } else { previous.0 },
        if height > 0 { height } else { previous.1 },
    )
}

fn row_pixel_rect(
    geometry: render::Geometry,
    physical: (i32, i32),
    range: (usize, usize),
) -> PixelRect {
    let edge = |row| (geometry.origin_y() + row * geometry.cell_height) as i32;
    let top = edge(range.0);
    let bottom = edge(range.1).min(physical.1 - (geometry.padding + geometry.inset_bottom) as i32);
    PixelRect {
        x: 0,
        y: top.max(0),
        w: physical.0,
        h: (bottom - top.max(0)).max(0),
    }
}

impl State {
    fn create_window(
        &mut self,
        qh: &QueueHandle<Self>,
        display_ptr: *mut std::ffi::c_void,
    ) -> Result<(), Box<dyn Error>> {
        let compositor = self.compositor.as_ref().unwrap();
        let wm_base = self.wm_base.as_ref().unwrap();

        let surface = compositor.create_surface(qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());

        toplevel.set_title(
            self.editor
                .as_ref()
                .map_or_else(|| "Beyrix".into(), BEdit::title),
        );
        let app_id = if self.standalone_editor {
            "io.github.beykus_y.BEdit"
        } else {
            "beyrix"
        };
        toplevel.set_app_id(app_id.to_string());
        toplevel.set_min_size(320, 200);

        let theme = crate::theme::AppConfig::load()
            .map_err(|e| format!("load configuration: {e}"))?
            .theme;
        if let (Some(manager), Some(_)) = (&self.fractional_manager, &self.viewporter) {
            self.fractional_scale = Some(manager.get_fractional_scale(&surface, qh, ()));
        }
        if let Some(viewporter) = &self.viewporter {
            let viewport = viewporter.get_viewport(&surface, qh, ());
            self.viewport = Some(viewport);
        }
        let renderer =
            render::Renderer::with_theme(theme.clone()).map_err(|e| format!("load fonts: {e}"))?;
        self.default_font_size = theme.font_size;
        let sizes = self.surface_sizes();
        let (columns, rows) = sizes.grid_size(&renderer);
        let init_geom = sizes.geometry(&renderer);
        let requested = theme.renderer;
        let mut gl = None;
        let reason;
        if requested == RendererChoice::Software {
            reason = "software selected explicitly in configuration".to_string();
        } else if requested == RendererChoice::Vulkan {
            reason =
                "vulkan requested but the Vulkan rendering backend is not implemented; using software"
                    .to_string();
        } else {
            let surface_ptr = surface.id().as_ptr().cast();
            match GlBackend::new(display_ptr, surface_ptr, WIDTH, HEIGHT) {
                Ok(backend) => {
                    let renderer_string = backend.renderer_string();
                    let software = gpu::EglProbe::is_software_renderer(&renderer_string);
                    if requested == RendererChoice::Auto && software {
                        reason = format!(
                            "auto: EGL renderer {renderer_string} is software; using software backend"
                        );
                    } else if requested == RendererChoice::Auto {
                        reason =
                            format!("auto: hardware OpenGL renderer detected ({renderer_string})");
                        gl = Some(backend);
                    } else {
                        reason = format!("opengl selected explicitly ({renderer_string})");
                        gl = Some(backend);
                    }
                }
                Err(error) => {
                    reason = format!("OpenGL backend unavailable ({error}); using software");
                    eprintln!("beyrix: {reason}");
                }
            }
        }
        if self.debug_input {
            eprintln!(
                "[output] backend={} requested={} reason={}",
                if gl.is_some() { "opengl" } else { "software" },
                requested,
                reason
            );
        }
        self.backend = if gl.is_some() {
            ActiveBackend::OpenGl
        } else {
            ActiveBackend::Software
        };
        self.gl = gl;
        let theme_name = theme.name.clone();
        if let Some(control) = &self.control {
            control.set_prompt(&theme)?;
        }
        let mut terminal = Terminal::with_theme(columns, rows, theme);
        terminal.screen_mut().set_pixel_metrics(
            (columns * init_geom.cell_width, rows * init_geom.cell_height),
            (init_geom.cell_width, init_geom.cell_height),
        );
        self.damage_full = true;

        // Начальный commit без буфера:
        // ждём configure от композитора.
        surface.commit();

        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
        self.terminal = Some(terminal);
        self.renderer = Some(renderer);
        if self.standalone_editor {
            return Ok(());
        }
        let pty = Pty::spawn_with_prompt(
            Some((columns as u16, rows as u16)),
            Some(&theme_name),
            self.control
                .as_ref()
                .map(|c| (c.path.as_path(), c.prompt_path.as_path())),
        )?;
        let fd = pty.master.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if self.debug_input {
            eprintln!("[output] spawned bash pid={}", pty.child_pid);
        }
        let child_pid = pty.child_pid;
        let master_fd = pty.master.as_raw_fd();
        self.pty = Some(pty);
        self.init_platform_subsystems(child_pid, master_fd);

        Ok(())
    }

    fn surface_sizes(&self) -> SurfaceSizes {
        SurfaceSizes::new((self.width, self.height), self.scale)
    }

    fn pointer_geometry(&self) -> Option<render::Geometry> {
        let renderer = self.renderer.as_ref()?;
        let sizes = self.surface_sizes();
        let insets = self.dock_insets_tuple(sizes.scale.factor());
        Some(
            sizes
                .geometry(renderer)
                .with_insets(insets.0, insets.1, insets.2, insets.3),
        )
    }

    fn map_surface_buffer(&self, surface: &wl_surface::WlSurface, sizes: SurfaceSizes) {
        if let Some(viewport) = &self.viewport {
            viewport.set_destination(sizes.logical.0, sizes.logical.1);
        }
        if surface.version() >= 3 {
            surface.set_buffer_scale(sizes.buffer_scale(self.viewport.is_some()));
        }
    }

    fn fallback_to_software(&mut self, reason: String) {
        eprintln!("beyrix: OpenGL backend disabled ({reason}); using software");
        self.gl = None;
        self.backend = ActiveBackend::Software;
        self.damage_full = true;
        self.damage_hashes.clear();
        self.damage_signature = None;
        self.damage_history.clear();
        self.frame_pending = false;
        self.dirty = true;
    }

    fn open_editor(&mut self, request: OpenRequest) {
        if self.editor.is_none() {
            match BEdit::new(&request) {
                Ok(editor) => self.editor = Some(editor),
                Err(error) => {
                    if let Some(terminal) = &mut self.terminal {
                        let message: String = error.chars().filter(|ch| !ch.is_control()).collect();
                        terminal.process(format!("\r\nBEdit: {message}\r\n").as_bytes());
                    }
                }
            }
        }
        if let Some(editor) = &mut self.editor {
            editor.open_request(request);
        }
        if let Some((toplevel, editor)) = self.toplevel.as_ref().zip(self.editor.as_ref()) {
            toplevel.set_title(editor.title());
            self.selection.clear();
        }
        self.damage_full = true;
        self.dirty = true;
    }

    fn close_editor(&mut self) {
        if self.standalone_editor {
            self.running = false;
            return;
        }
        self.editor = None;
        if let Some(toplevel) = &self.toplevel {
            toplevel.set_title("Beyrix".to_string());
        }
        self.damage_full = true;
        self.dirty = true;
    }

    fn apply_theme(&mut self, name: &str) -> Result<(), Box<dyn Error>> {
        let theme = crate::theme::Theme::load_for(name)?;
        let sizes = self.surface_sizes();
        let renderer = self.renderer.as_mut().ok_or("renderer unavailable")?;
        renderer.retheme(theme.clone())?;
        let (cols, rows) = sizes.grid_size(renderer);
        if let Some(gl) = &mut self.gl {
            gl.invalidate_atlas();
        }
        self.damage_full = true;
        if let Some(pty) = &self.pty {
            pty.resize(cols as u16, rows as u16)?;
        }
        if let Some(control) = &self.control {
            control.set_prompt(&theme)?;
        }
        if let Some(terminal) = &mut self.terminal {
            terminal.apply_theme(theme);
            terminal.resize(cols, rows);
        }
        if let Some(editor) = &mut self.editor {
            editor.follow_cursor();
        }
        self.dirty = true;
        Ok(())
    }

    fn software_buffer(
        &mut self,
        physical: (i32, i32),
        qh: &QueueHandle<Self>,
    ) -> Result<Option<usize>, Box<dyn Error>> {
        if let Some(index) = self.buffers.iter().position(|buffer| !buffer.busy) {
            return Ok(Some(index));
        }
        if self.buffers.len() >= MAX_SHM_BUFFERS {
            return Ok(None);
        }
        let (stride, size) = shm_layout(physical).ok_or("software framebuffer too large")?;
        let shm = self.shm.as_ref().ok_or("wl_shm unavailable")?;
        let file = tempfile::tempfile()?;
        file.set_len(size as u64)?;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        let Some(mapping) = NonNull::new(ptr.cast()) else {
            unsafe { libc::munmap(ptr, size as usize) };
            return Err("null framebuffer mapping".into());
        };
        let pool = shm.create_pool(file.as_fd(), size, qh, ());
        let id = self.next_buffer_id;
        self.next_buffer_id += 1;
        let buffer = pool.create_buffer(
            0,
            physical.0,
            physical.1,
            stride,
            wl_shm::Format::Argb8888,
            qh,
            id,
        );
        pool.destroy();
        let index = self.buffers.len();
        self.buffers.push(FrameBuffer {
            buffer,
            mapping,
            len: size as usize,
            busy: false,
            id,
            generation: self.generation,
        });
        Ok(Some(index))
    }

    fn resize(&mut self, logical: (i32, i32), scale: Scale) -> Result<(), Box<dyn Error>> {
        let sizes = SurfaceSizes::new(logical, scale);
        if sizes == self.surface_sizes() {
            return Ok(());
        }
        let old_physical = self.surface_sizes().physical;
        let (columns, rows) =
            self.effective_grid_size(sizes, self.renderer.as_ref().ok_or("renderer unavailable")?);
        if sizes.physical != old_physical {
            if let Some(gl) = &mut self.gl
                && let Err(error) = gl.resize(sizes.physical.0, sizes.physical.1)
            {
                self.fallback_to_software(error);
            }
            self.generation += 1;
            self.buffers.retain(|frame| frame.busy);
        }
        if let Some(terminal) = &mut self.terminal {
            let geom = self.renderer.as_ref().map(|r| sizes.geometry(r));
            let (cw, ch) = geom.map_or((9, 18), |g| (g.cell_width, g.cell_height));
            let xpix = (columns * cw).min(u16::MAX as usize) as u16;
            let ypix = (rows * ch).min(u16::MAX as usize) as u16;
            terminal
                .screen_mut()
                .set_pixel_metrics((columns * cw, rows * ch), (cw, ch));
            if (terminal.screen().columns, terminal.screen().rows) != (columns, rows) {
                if let Some(pty) = &self.pty {
                    pty.resize_with_pixels(columns as u16, rows as u16, xpix, ypix)
                        .map_err(|e| format!("TIOCSWINSZ: {e}"))?;
                }
                terminal.resize(columns, rows);
            }
        }
        if let Some(editor) = &mut self.editor {
            editor.follow_cursor();
        }
        (self.width, self.height) = sizes.logical;
        self.scale = scale;
        self.damage_full = true;
        self.dirty = true;
        Ok(())
    }

    fn pointer_modifiers(&self) -> crate::mouse::Modifiers {
        self.xkb
            .as_ref()
            .map_or_else(crate::mouse::Modifiers::default, |xkb| {
                crate::mouse::Modifiers {
                    shift: xkb.shift(),
                    alt: xkb.alt(),
                    ctrl: xkb.control(),
                }
            })
    }

    fn mouse_mode(&self) -> crate::mouse::Mode {
        self.terminal
            .as_ref()
            .map_or(crate::mouse::Mode::Off, |terminal| {
                terminal.screen().mouse_mode()
            })
    }

    /// Whether pointer events belong to the application rather than to
    /// Beyrix's selection. Shift keeps text selection available in every
    /// application that enables mouse reporting.
    fn mouse_reporting(&self) -> bool {
        self.mouse_mode() != crate::mouse::Mode::Off && !self.pointer_modifiers().shift
    }

    fn mouse_cell(&self) -> Option<(usize, usize)> {
        let terminal = self.terminal.as_ref()?;
        let geometry = self.pointer_geometry()?;
        let screen = terminal.screen();
        let (x, y) = self.surface_sizes().physical_point(self.pointer_position);
        let (column, display_row) = geometry.cell_at(x, y);
        let row = display_row
            .saturating_sub(screen.layout_offset())
            .min(screen.rows.saturating_sub(1));
        Some((column.min(screen.columns.saturating_sub(1)), row))
    }

    fn report_mouse(
        &mut self,
        button: Option<crate::mouse::Button>,
        pressed: bool,
        motion: bool,
    ) -> bool {
        let Some((mode, encoding)) = self.terminal.as_ref().map(|terminal| {
            (
                terminal.screen().mouse_mode(),
                terminal.screen().mouse_encoding(),
            )
        }) else {
            return false;
        };
        let Some((column, row)) = self.mouse_cell() else {
            return false;
        };
        let (pixel_x, pixel_y) = self.pointer_geometry().map_or((0, 0), |g| {
            let (px, py) = self.surface_sizes().physical_point(self.pointer_position);
            (
                (px - g.origin_x() as f64).max(0.0).round() as usize,
                (py - g.origin_y() as f64).max(0.0).round() as usize,
            )
        });
        let Some(bytes) = crate::mouse::encode(crate::mouse::Event {
            mode,
            encoding,
            button,
            column,
            row,
            pixel_x,
            pixel_y,
            pressed,
            motion,
            modifiers: self.pointer_modifiers(),
        }) else {
            return false;
        };
        // Applications normally leave the alternate screen for mouse-driven
        // output; a click in the live grid must not edit a scrollback view that
        // the application cannot see.
        if pressed
            && !motion
            && self
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.screen().view_offset() > 0)
        {
            self.dirty = true;
            if let Some(terminal) = &mut self.terminal {
                terminal.screen_mut().reset_view();
            }
        }
        self.input.extend(bytes);
        true
    }

    fn pointer_button(&mut self, button: crate::mouse::Button, pressed: bool, time: u32) {
        let sizes = self.surface_sizes();
        let (x, y) = sizes.physical_point(self.pointer_position);
        if self.editor.is_some() {
            let geometry = self.pointer_geometry();
            let Some(editor) = &mut self.editor else {
                return;
            };
            if pressed {
                if button == crate::mouse::Button::Left
                    && let Some(geometry) = geometry
                {
                    let metrics = metrics(geometry, sizes);
                    if let Some(effect) = editor.mouse_confirmation(x, y, metrics) {
                        match effect {
                            crate::editor::Effect::Close => self.close_editor(),
                            crate::editor::Effect::CloseWindow => self.running = false,
                            _ => {}
                        }
                    } else {
                        editor.mouse_press(x, y, metrics, time);
                    }
                    self.dirty = true;
                }
            } else {
                editor.mouse_release();
                self.dirty = true;
            }
            return;
        }
        if pressed {
            use crate::widgets::compositor::{ActiveSystemPanel, SurfaceOwner};
            let (px, py) = (x as f32, y as f32);
            let top_owner = self.ui_compositor.topmost_owner_at(px, py);
            match top_owner {
                Some(SurfaceOwner::Settings) => {
                    let (_, changed) = self
                        .settings_overlay
                        .handle_click((px, py), &mut self.platform_config);
                    if !self.settings_overlay.open {
                        self.ui_compositor.close_active();
                        self.sync_active_panel();
                    }
                    if changed {
                        self.reconcile_platform_subsystems();
                    }
                    self.platform_dirty = true;
                    self.dirty = true;
                    return;
                }
                Some(SurfaceOwner::Studio) => {
                    let mut cfg_changed = false;
                    if let Some(widgets) = &mut self.widgets {
                        let (_, changed) = widgets.handle_pointer_press_with_config(
                            (px, py),
                            sizes.scale.factor(),
                            Some(&mut self.platform_config),
                        );
                        cfg_changed = changed;
                        if !widgets.editor.active {
                            self.ui_compositor.close_active();
                            self.sync_active_panel();
                        }
                    }
                    if cfg_changed {
                        self.reconcile_platform_subsystems();
                    }
                    self.platform_dirty = true;
                    self.dirty = true;
                    return;
                }
                Some(SurfaceOwner::XRayPanel | SurfaceOwner::XRayIndicator) => {
                    let hit = self
                        .xray_hits
                        .iter()
                        .rev()
                        .find(|h| px >= h.x && px <= h.x + h.w && py >= h.y && py <= h.y + h.h)
                        .cloned();
                    if let Some(hit) = hit {
                        if hit.node_id == "xray-toggle" {
                            self.ui_compositor.toggle_panel(ActiveSystemPanel::XRay);
                            self.sync_active_panel();
                        } else if let Some(xray) = &mut self.xray {
                            xray.handle_hit(&hit.node_id, hit.kind);
                        }
                    }
                    self.platform_dirty = true;
                    self.dirty = true;
                    return;
                }
                Some(SurfaceOwner::Widget) => {
                    if let Some(widgets) = &mut self.widgets {
                        widgets.handle_pointer_press((px, py), sizes.scale.factor());
                        if self.ui_compositor.is_open(ActiveSystemPanel::Studio)
                            && !widgets.editor.active
                        {
                            self.ui_compositor.close_active();
                            self.sync_active_panel();
                        }
                    }
                    self.platform_dirty = true;
                    self.dirty = true;
                    return;
                }
                None => {
                    // When Studio is active, clicking the preview canvas never
                    // leaks into terminal text selection or PTY mouse reporting.
                    if self.ui_compositor.is_open(ActiveSystemPanel::Studio) {
                        if let Some(widgets) = &mut self.widgets {
                            widgets.handle_pointer_press((px, py), sizes.scale.factor());
                        }
                        self.platform_dirty = true;
                        self.dirty = true;
                        return;
                    }
                }
            }
            if self.mouse_reporting() && self.report_mouse(Some(button), true, false) {
                self.mouse_button = Some(button);
                return;
            }
            if button == crate::mouse::Button::Left
                && self.pointer_modifiers().ctrl
                && let Some((col, row)) = self.mouse_cell()
                && let Some(terminal) = &self.terminal
                && let Some(cell) = terminal.screen().get(col, row)
                && let Some(url) = terminal.screen().hyperlink_url(cell.hyperlink_id)
            {
                open_safe_hyperlink(url);
                return;
            }
            if button == crate::mouse::Button::Left
                && let (Some(geometry), Some(terminal)) =
                    (self.pointer_geometry(), self.terminal.as_ref())
            {
                let position = crate::selection::position(geometry, x, y, terminal.screen());
                let (logical_x, logical_y) = self.pointer_position;
                self.selection
                    .press(terminal.screen(), position, time, logical_x, logical_y);
                self.dirty = true;
            }
        } else if let Some(held) = self.mouse_button.take() {
            if self.mouse_mode() != crate::mouse::Mode::Off {
                self.report_mouse(Some(held), false, false);
            }
        } else if button == crate::mouse::Button::Left {
            if let Some(widgets) = &mut self.widgets {
                widgets.handle_pointer_release();
            }
            self.selection.release();
        }
    }

    fn pointer_motion(&mut self) {
        if self.widgets.is_some() {
            let sizes = self.surface_sizes();
            let (px, py) = sizes.physical_point(self.pointer_position);
            let cell = self.renderer.as_ref().map_or((10.0, 18.0), |r| {
                let g = r.geometry(sizes.scale.factor());
                (g.cell_width as f32, g.cell_height as f32)
            });
            if let Some(widgets) = &mut self.widgets
                && widgets.handle_pointer_motion(
                    (px as f32, py as f32),
                    (sizes.physical.0 as f32, sizes.physical.1 as f32),
                    sizes.scale.factor(),
                    cell,
                )
            {
                self.dirty = true;
                return;
            }
        }
        if self.editor.is_some() || self.selection.is_dragging() {
            self.update_drag(false);
            return;
        }
        if self.mouse_reporting() {
            match (self.mouse_mode(), self.mouse_button) {
                (crate::mouse::Mode::Motion, None) => {
                    self.report_mouse(None, true, true);
                    return;
                }
                (crate::mouse::Mode::Drag | crate::mouse::Mode::Motion, Some(button)) => {
                    self.report_mouse(Some(button), true, true);
                    return;
                }
                _ => {}
            }
        }
        self.update_drag(false);
    }

    fn scroll_pointer(&mut self) {
        let steps = self.wheel_steps.take().unwrap_or(self.wheel_axis / 10.0);
        self.wheel_axis = 0.0;
        let steps = if steps.is_finite() { steps } else { 0.0 };
        let h_steps = self
            .wheel_h_steps
            .take()
            .unwrap_or(self.wheel_h_axis / 10.0);
        self.wheel_h_axis = 0.0;
        let h_steps = if h_steps.is_finite() { h_steps } else { 0.0 };
        let sizes = self.surface_sizes();
        let (px, py) = sizes.physical_point(self.pointer_position);
        if self.editor.is_none() && steps.abs() > 0.0 {
            use crate::widgets::compositor::SurfaceOwner;
            match self.ui_compositor.topmost_owner_at(px as f32, py as f32) {
                Some(SurfaceOwner::XRayPanel) => {
                    if let Some(xray) = &mut self.xray {
                        if steps > 0.0 {
                            xray.handle_hit("xray-scroll-down", crate::widgets::ui::HitKind::Button);
                        } else {
                            xray.handle_hit("xray-scroll-up", crate::widgets::ui::HitKind::Button);
                        }
                        self.platform_dirty = true;
                        self.dirty = true;
                    }
                    return;
                }
                Some(SurfaceOwner::Settings | SurfaceOwner::Studio) => {
                    if let Some(widgets) = &mut self.widgets {
                        widgets.handle_pointer_scroll((px as f32, py as f32), (steps * 24.0) as f32);
                    }
                    self.platform_dirty = true;
                    self.dirty = true;
                    return;
                }
                Some(SurfaceOwner::Widget) => {
                    if let Some(widgets) = &mut self.widgets
                        && widgets
                            .handle_pointer_scroll((px as f32, py as f32), (steps * 24.0) as f32)
                    {
                        self.dirty = true;
                        return;
                    }
                }
                _ => {}
            }
        }
        if let Some(editor) = &mut self.editor {
            let (rows, remainder) = wheel_rows(self.wheel_remainder, steps, 3);
            self.wheel_remainder = remainder;
            if let Some(renderer) = &self.renderer {
                let geometry = sizes.geometry(renderer);
                let (x, y) = sizes.physical_point(self.pointer_position);
                self.dirty |= editor.scroll(-rows, x, y, metrics(geometry, sizes));
            }
        } else if self.mouse_reporting() {
            // Report whole vertical wheel notches and keep fractional remainder.
            self.wheel_mouse_remainder += steps;
            let notches = self.wheel_mouse_remainder.trunc();
            self.wheel_mouse_remainder -= notches;
            let button = if notches > 0.0 {
                crate::mouse::Button::WheelDown
            } else {
                crate::mouse::Button::WheelUp
            };
            for _ in 0..notches.abs().min(64.0) as usize {
                self.report_mouse(Some(button), true, false);
            }
            // Horizontal wheel notches (WheelLeft / WheelRight).
            self.wheel_h_mouse_remainder += h_steps;
            let h_notches = self.wheel_h_mouse_remainder.trunc();
            self.wheel_h_mouse_remainder -= h_notches;
            let h_button = if h_notches > 0.0 {
                crate::mouse::Button::WheelRight
            } else {
                crate::mouse::Button::WheelLeft
            };
            for _ in 0..h_notches.abs().min(64.0) as usize {
                self.report_mouse(Some(h_button), true, false);
            }
        } else if let Some(terminal) = &mut self.terminal {
            let screen = terminal.screen_mut();
            if screen.is_alternate_screen() && screen.alternate_scroll {
                // DEC ?1007 Alternate Scroll Mode: translate wheel to Up/Down arrow keys.
                self.wheel_mouse_remainder += steps;
                let notches = self.wheel_mouse_remainder.trunc();
                self.wheel_mouse_remainder -= notches;
                let up = notches < 0.0;
                let seq = crate::keyboard::alternate_scroll_sequence(up, screen.app_cursor_keys);
                for _ in 0..notches.abs().min(32.0) as usize {
                    self.input.extend(seq);
                }
            } else {
                let (rows, remainder) =
                    wheel_rows(self.wheel_remainder, steps, screen.scroll_speed());
                self.wheel_remainder = remainder;
                self.dirty |= screen.scroll_view(rows);
            }
        }
        self.update_drag(false);
    }

    fn update_drag(&mut self, tick: bool) {
        let sizes = self.surface_sizes();
        let (x, y) = sizes.physical_point(self.pointer_position);
        if self.editor.is_some() {
            let (Some(renderer), Some(editor)) = (&self.renderer, &mut self.editor) else {
                return;
            };
            if !self.pointer_focus {
                return;
            }
            let geometry = sizes.geometry(renderer);
            self.dirty |= editor.mouse_drag(x, y, metrics(geometry, sizes), tick);
            return;
        }
        if !self.selection.is_dragging() || !self.pointer_focus {
            self.drag_scroll_at = None;
            return;
        }
        let Some(geometry) = self.pointer_geometry() else {
            return;
        };
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        if !self.selection.drag_valid(terminal.screen()) {
            self.selection.clear();
            self.drag_scroll_at = None;
            self.dirty = true;
            return;
        }
        let top = geometry.origin_y() as f64;
        let bottom = top + (terminal.screen().rows * geometry.cell_height) as f64;
        let (direction, distance) = if y < top {
            (1, top - y)
        } else if y >= bottom {
            (-1, y - bottom)
        } else {
            self.drag_scroll_at = None;
            (0, 0.0)
        };
        if tick {
            if direction == 0 {
                return;
            }
            let now = std::time::Instant::now();
            if self.drag_scroll_at.is_some_and(|deadline| now < deadline) {
                return;
            }
            self.drag_scroll_at = Some(now + Duration::from_millis(50));
            let rows = (1 + (distance / geometry.cell_height as f64) as isize).min(8);
            self.dirty |= terminal.screen_mut().scroll_view(direction * rows);
        }
        let screen = terminal.screen();
        self.dirty |= self.selection.motion_at(
            screen,
            crate::selection::position(geometry, x, y, screen),
            Some(self.pointer_position),
        );
    }

    fn setup_data_device(&mut self, qh: &QueueHandle<Self>) {
        if self.data_device.is_none()
            && let (Some(manager), Some(seat)) = (&self.data_manager, &self.seat)
        {
            self.data_device = Some(manager.get_data_device(seat, qh, ()));
        }
    }
    fn set_clipboard(&mut self, text: String, serial: u32, qh: &QueueHandle<Self>) {
        let (Some(manager), Some(device)) = (&self.data_manager, &self.data_device) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let source = manager.create_data_source(qh, Arc::new(text.into_bytes()));
        source.offer("text/plain;charset=utf-8".into());
        source.offer("text/plain".into());
        device.set_selection(Some(&source), serial);
        if let Some(old) = self.data_source.replace(source) {
            old.destroy();
        }
        if let Some(editor) = &mut self.editor {
            editor.set_message("Copied", false);
            self.dirty = true;
        }
    }
    fn has_clipboard_text_offer(&self) -> bool {
        #[cfg(test)]
        if self
            .test_clipboard
            .as_deref()
            .is_some_and(|s| !s.is_empty())
        {
            return true;
        }
        let Some(offer) = &self.data_offer else {
            return false;
        };
        let Some(mimes) = offer.data::<Mutex<Vec<String>>>() else {
            return false;
        };
        let mimes = mimes.lock().unwrap_or_else(|e| e.into_inner());
        [
            "text/plain;charset=utf-8",
            "text/plain;charset=UTF-8",
            "UTF8_STRING",
            "text/plain",
        ]
        .into_iter()
        .any(|m| mimes.iter().any(|v| v == m))
    }

    fn copy(&mut self, serial: u32, qh: Option<&QueueHandle<Self>>) -> bool {
        let text = if let Some(editor) = &self.editor {
            editor.selection_text()
        } else {
            self.terminal
                .as_ref()
                .and_then(|t| self.selection.text(t.screen()))
                .filter(|t| !t.is_empty())
        };
        if let Some(text) = text {
            #[cfg(test)]
            {
                self.test_clipboard = Some(text.clone());
            }
            if let Some(qh) = qh {
                self.set_clipboard(text, serial, qh);
            }
            true
        } else {
            false
        }
    }

    fn paste(&mut self) {
        if self.debug_input {
            eprintln!("[clipboard] paste requested");
        }
        // If a multiline unbracketed paste was guarded and the user presses
        // paste a second time within 3 seconds, confirm and deliver it.
        if let Some(pending) = self.pending_multiline_paste.take()
            && std::time::Instant::now() <= pending.expires_at
        {
            self.input.extend(pending.bytes);
            return;
        }
        #[cfg(test)]
        if self.data_offer.is_none()
            && let Some(text) = self.test_clipboard.clone()
        {
            let bracketed = self
                .terminal
                .as_ref()
                .is_some_and(|t| t.screen().bracketed_paste);
            self.deliver_terminal_paste(text.as_bytes(), bracketed);
            return;
        }
        let Some(offer) = &self.data_offer else {
            return;
        };
        let Some(mimes) = offer.data::<Mutex<Vec<String>>>() else {
            return;
        };
        let mimes = mimes.lock().unwrap();
        let mime = [
            "text/plain;charset=utf-8",
            "text/plain;charset=UTF-8",
            "UTF8_STRING",
            "text/plain",
        ]
        .into_iter()
        .find(|m| mimes.iter().any(|v| v == m));
        let Some(mime) = mime else {
            return;
        };
        let mut fds = [-1; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
            eprintln!("clipboard pipe: {}", std::io::Error::last_os_error());
            return;
        }
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        if let Err(e) = set_nonblocking(&read) {
            eprintln!("clipboard read setup: {e}");
            return;
        }
        offer.receive(mime.into(), write.as_fd());
        let destination = if let Some(editor) = &self.editor {
            Destination::Editor(editor.paste_target())
        } else {
            Destination::Terminal
        };
        self.transfers.push(Transfer {
            file: File::from(read),
            bytes: Vec::new(),
            offset: 0,
            writing: false,
            bracketed: destination == Destination::Terminal
                && self
                    .terminal
                    .as_ref()
                    .is_some_and(|t| t.screen().bracketed_paste),
            destination,
        });
    }
    fn pump_clipboard(&mut self) {
        let mut i = 0;
        while i < self.transfers.len() {
            let transfer = &mut self.transfers[i];
            let result = if transfer.writing {
                transfer.file.write(&transfer.bytes[transfer.offset..])
            } else {
                let mut buf = [0u8; 8192];
                transfer.file.read(&mut buf).inspect(|&n| {
                    transfer.bytes.extend_from_slice(&buf[..n]);
                })
            };
            let mut done = false;
            let mut success = false;
            match result {
                Ok(n) if transfer.writing => {
                    transfer.offset += n;
                    if n == 0 || transfer.offset == transfer.bytes.len() {
                        done = true;
                    }
                }
                Ok(0) => {
                    done = true;
                    success = true;
                }
                Ok(_) if transfer.bytes.len() > 16 * 1024 * 1024 => {
                    eprintln!("clipboard: text exceeds 16 MiB");
                    done = true;
                }
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => {
                    eprintln!("clipboard transfer: {e}");
                    done = true;
                }
            }
            if done {
                let transfer = self.transfers.swap_remove(i);
                if success {
                    match std::str::from_utf8(&transfer.bytes) {
                        Ok(text) => {
                            if self.debug_input {
                                eprintln!(
                                    "[clipboard] payload received={} destination={:?}",
                                    transfer.bytes.len(),
                                    transfer.destination
                                );
                            }
                            match transfer.destination {
                                Destination::Editor(target) => {
                                    if let Some(editor) = &mut self.editor {
                                        self.dirty |= editor.paste(target, text);
                                    }
                                }
                                Destination::Terminal => {
                                    self.deliver_terminal_paste(
                                        &transfer.bytes,
                                        transfer.bracketed,
                                    );
                                }
                            }
                        }
                        Err(e) => eprintln!("clipboard invalid UTF-8: {e}"),
                    }
                }
            } else {
                i += 1;
            }
        }
    }
    fn deliver_terminal_paste(&mut self, raw: &[u8], bracketed: bool) {
        if raw.is_empty() {
            return;
        }
        if !bracketed
            && self.platform_config.keyboard.multiline_paste_protection
            && (raw.contains(&b'\n') || raw.contains(&b'\r'))
        {
            // Protect against accidental multiline command execution when the
            // shell/app does not have bracketed paste enabled. A second paste
            // within 3 seconds confirms and sends the payload.
            self.pending_multiline_paste = Some(PendingMultilinePaste {
                bytes: paste_bytes(raw, false),
                expires_at: std::time::Instant::now() + Duration::from_secs(3),
            });
            return;
        }
        let bytes = paste_bytes(raw, bracketed);
        self.input.extend(bytes);
    }

    fn next_poll_timeout_ms(&self) -> i32 {
        let mut timeout = 50i32;
        if let Some(rep) = &self.repeat_key {
            let now = std::time::Instant::now();
            if rep.next_fire <= now {
                return 0;
            }
            let rem = rep.next_fire.duration_since(now).as_millis().min(50) as i32;
            timeout = timeout.min(rem.max(1));
        }
        timeout
    }

    fn pump_key_repeat(&mut self, qh: Option<&QueueHandle<Self>>) {
        if !self.keyboard_focus || self.repeat_rate <= 0 {
            self.repeat_key = None;
            return;
        }
        let interval = Duration::from_micros((1_000_000 / self.repeat_rate.max(1) as u64).max(1));
        let now = std::time::Instant::now();
        // Cap burst repeats per event-loop turn to keep UI responsive.
        for _ in 0..8 {
            let Some(rep) = self.repeat_key.as_ref() else {
                break;
            };
            if now < rep.next_fire {
                break;
            }
            let (key, serial) = (rep.key, rep.serial);
            if let Some(rep_mut) = self.repeat_key.as_mut() {
                rep_mut.next_fire += interval;
                if rep_mut.next_fire + interval * 4 < now {
                    rep_mut.next_fire = now + interval;
                }
            }
            if self.editor.is_some() {
                if let Some(qh) = qh {
                    self.editor_key(key, serial, qh);
                }
            } else {
                self.dispatch_terminal_key_event(
                    key,
                    serial,
                    qh,
                    crate::keyboard::KeyEventType::Repeat,
                );
            }
        }
    }

    fn dispatch_terminal_key_press(
        &mut self,
        key: u32,
        serial: u32,
        qh: Option<&QueueHandle<Self>>,
    ) -> bool {
        let kitty_release = self
            .terminal
            .as_ref()
            .is_some_and(|t| (t.screen().kitty_keyboard_flags() & 2) != 0);
        let forwarded =
            self.dispatch_terminal_key_event(key, serial, qh, crate::keyboard::KeyEventType::Press);
        self.pressed_keys.insert(
            key,
            PressedKeyRoute {
                sent_to_pty: forwarded,
                kitty_report_release: forwarded && kitty_release,
            },
        );
        forwarded
    }

    fn dispatch_terminal_key_release(&mut self, key: u32) {
        let Some(route) = self.pressed_keys.remove(&key) else {
            return;
        };
        if !route.sent_to_pty || !route.kitty_report_release {
            return;
        }
        if self.keyboard_focus
            && self.editor.is_none()
            && let (Some(xkb), Some(term)) = (&self.xkb, &self.terminal)
        {
            let modes = term.screen().input_modes();
            // Only emit release if Kitty report-events flag (bit 2) is still active.
            if (modes.kitty_flags & 2) != 0 {
                let bytes = xkb.key_with_modes(key, modes, crate::keyboard::KeyEventType::Release);
                self.input.extend(bytes);
            }
        }
    }

    /// Route a terminal key press/repeat (from live Wayland or E2E input tests) through
    /// configurable hotkeys, Smart Ctrl+C / Ctrl+V, scrollback, font zoom, and mode-aware XKB translation.
    /// Returns `true` if the key is eligible for Wayland autorepeat.
    fn dispatch_terminal_key_event(
        &mut self,
        key: u32,
        serial: u32,
        qh: Option<&QueueHandle<Self>>,
        event_type: crate::keyboard::KeyEventType,
    ) -> bool {
        self.last_serial = serial;
        let Some((ctrl, shift, alt)) = self.xkb.as_ref().map(|x| (x.control(), x.shift(), x.alt()))
        else {
            return false;
        };

        // Focused Beyrix Studio text input consumes typing/Backspace/Enter/Escape cleanly.
        if let Some(w) = &mut self.widgets
            && w.editor.active
            && w.editor.focused_input.is_some()
        {
            let ch = match key {
                2 | 79 => Some('1'),
                3 | 80 => Some('2'),
                4 | 81 => Some('3'),
                5 | 75 => Some('4'),
                6 | 76 => Some('5'),
                7 | 77 => Some('6'),
                8 | 71 => Some('7'),
                9 | 72 => Some('8'),
                10 | 73 => Some('9'),
                11 | 82 => Some('0'),
                12 | 74 => Some('-'),
                52 | 83 => Some('.'),
                _ => None,
            };
            let backspace = key == 14;
            let enter = key == 28 || key == 96;
            let escape = key == 1;
            if w.handle_editor_key(ch, backspace, enter, escape) {
                self.platform_dirty = true;
                self.dirty = true;
                return false;
            }
        }

        // Escape closes the topmost open system panel (Settings, Studio, or X-Ray) without leaking to PTY.
        if key == 1 && !ctrl && !shift && !alt {
            if self.ui_compositor.close_active() {
                self.sync_active_panel();
                self.platform_dirty = true;
                self.dirty = true;
                return false;
            }
        }

        // Live font zoom: Ctrl+= / Ctrl++ (evdev 13/78), Ctrl+- (12/74), Ctrl+0 (11/82).
        if ctrl && !alt && matches!(key, 13 | 78 | 12 | 74 | 11 | 82) {
            if let Some(renderer) = &mut self.renderer {
                let next = match key {
                    13 | 78 => renderer.font_size() + 1.5,
                    12 | 74 => renderer.font_size() - 1.5,
                    _ => {
                        if self.default_font_size > 0.0 {
                            self.default_font_size
                        } else {
                            16.0
                        }
                    }
                };
                renderer.set_font_size(next);
                if let Some(gl) = &mut self.gl {
                    gl.invalidate_atlas();
                }
            }
            self.sync_pty_grid_after_layout_change();
            self.damage_full = true;
            self.dirty = true;
            return false;
        }

        // Ctrl+Shift+Up/Down: jump between OSC 133 semantic command prompts in scrollback.
        if ctrl && shift && !alt && matches!(key, 103 | 108) {
            if let Some(t) = &mut self.terminal {
                let current = t.screen().visible_start();
                let target = if key == 103 {
                    t.screen().prev_command_row(current)
                } else {
                    t.screen().next_command_row(current)
                };
                if let Some(row) = target {
                    self.dirty |= t.screen_mut().scroll_to_absolute_row(row);
                } else if key == 108 {
                    self.dirty |= t.screen().view_offset() > 0;
                    t.screen_mut().reset_view();
                }
            }
            return false;
        }

        // Shift + Home / End / PageUp / PageDown -> scrollback navigation.
        if shift && !ctrl && !alt && matches!(key, 102 | 107 | 104 | 109) {
            if let Some(t) = &mut self.terminal {
                let rows = t.screen().rows as isize;
                let delta = match key {
                    104 => rows,
                    109 => -rows,
                    102 => isize::MAX,
                    _ => isize::MIN,
                };
                self.dirty |= t.screen_mut().scroll_view(delta);
            }
            return true;
        }

        // Unified Settings Overlay hotkey (works even when Widgets JS VM is disabled).
        if crate::config::Hotkey::parse(&self.platform_config.keyboard.settings_hotkey)
            .is_some_and(|hk| hk.matches(ctrl, shift, alt, key))
        {
            self.ui_compositor
                .toggle_panel(crate::widgets::compositor::ActiveSystemPanel::Settings);
            self.sync_active_panel();
            self.platform_dirty = true;
            self.dirty = true;
            return false;
        }

        // Beyrix Studio (Visual Layout Editor) hotkey — ONLY intercept when Widgets subsystem is enabled!
        if self.widgets.is_some()
            && crate::config::Hotkey::parse(&self.platform_config.widgets.editor_hotkey)
                .is_some_and(|hk| hk.matches(ctrl, shift, alt, key))
        {
            self.ui_compositor
                .toggle_panel(crate::widgets::compositor::ActiveSystemPanel::Studio);
            self.sync_active_panel();
            self.platform_dirty = true;
            self.dirty = true;
            return false;
        }

        // X-Ray panel hotkey — ONLY intercept when X-Ray subsystem is enabled!
        if self.xray.is_some()
            && crate::config::Hotkey::parse(&self.platform_config.xray.hotkey)
                .is_some_and(|hk| hk.matches(ctrl, shift, alt, key))
        {
            self.ui_compositor
                .toggle_panel(crate::widgets::compositor::ActiveSystemPanel::XRay);
            self.sync_active_panel();
            self.platform_dirty = true;
            self.dirty = true;
            return false;
        }

        // Guaranteed literal Ctrl+C hotkey (always sends 0x03 regardless of selection).
        if crate::config::Hotkey::parse(&self.platform_config.keyboard.force_ctrl_c_hotkey)
            .is_some_and(|hk| hk.matches(ctrl, shift, alt, key))
        {
            self.input.push_back(0x03);
            return false;
        }

        // Guaranteed literal Ctrl+V hotkey (always sends 0x16 regardless of clipboard/TUI).
        if crate::config::Hotkey::parse(&self.platform_config.keyboard.literal_ctrl_v_hotkey)
            .is_some_and(|hk| hk.matches(ctrl, shift, alt, key))
        {
            self.input.push_back(0x16);
            return false;
        }

        // Ctrl+Shift+C (guaranteed copy) & Smart Ctrl+C (copy if intentional
        // non-empty selection exists, otherwise forward 0x03 to PTY).
        if ctrl && !alt && key == 46 {
            if shift {
                if self
                    .terminal
                    .as_ref()
                    .is_some_and(|t| self.selection.has_copyable_text(t.screen()))
                {
                    let copied = self.copy(serial, qh);
                    if copied && self.platform_config.keyboard.clear_selection_after_copy {
                        self.selection.clear();
                        self.dirty = true;
                    }
                }
                return false;
            }
            if self
                .terminal
                .as_ref()
                .is_some_and(|t| self.selection.has_copyable_text(t.screen()))
            {
                let copied = self.copy(serial, qh);
                if copied && self.platform_config.keyboard.clear_selection_after_copy {
                    self.selection.clear();
                    self.dirty = true;
                }
            } else {
                self.input.push_back(0x03);
            }
            return false;
        }

        // Ctrl+Shift+V (guaranteed paste) & Smart Ctrl+V.
        if ctrl && !alt && key == 47 {
            if shift {
                self.paste();
                return false;
            }
            match self.platform_config.keyboard.ctrl_v_policy {
                crate::config::CtrlVPolicy::AlwaysPaste => self.paste(),
                crate::config::CtrlVPolicy::AlwaysLiteral => self.input.push_back(0x16),
                crate::config::CtrlVPolicy::Smart => {
                    if self.in_tui_mode() || !self.has_clipboard_text_offer() {
                        self.input.push_back(0x16);
                    } else {
                        self.paste();
                    }
                }
            }
            return false;
        }

        let modes = self
            .terminal
            .as_ref()
            .map_or_else(Default::default, |t| t.screen().input_modes());
        let bytes = self
            .xkb
            .as_ref()
            .map_or_else(Vec::new, |x| x.key_with_modes(key, modes, event_type));
        if !bytes.is_empty()
            && let Some(t) = &mut self.terminal
        {
            self.dirty |= t.screen().view_offset() > 0;
            t.screen_mut().reset_view();
        }
        if self.debug_input {
            eprintln!("[input] translated {} bytes", bytes.len());
        }
        self.input.extend(bytes);
        true
    }

    fn write_input(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(pty) = &self.pty {
            let bytes = self.input.make_contiguous();
            let n =
                unsafe { libc::write(pty.master.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            if self.debug_input {
                if n < 0 {
                    eprintln!(
                        "[input] PTY write: {} (queued={})",
                        std::io::Error::last_os_error(),
                        bytes.len()
                    );
                } else {
                    eprintln!("[input] PTY write: {n}/{} bytes", bytes.len());
                }
            }
            if n > 0 {
                self.input.drain(..n as usize);
            } else if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::WouldBlock
                    && e.kind() != std::io::ErrorKind::Interrupted
                {
                    return Err(format!("write PTY input: {e}").into());
                }
            }
        }
        Ok(())
    }
    fn read_pty(&mut self) -> Result<(), Box<dyn Error>> {
        let Some(pty) = &self.pty else {
            return Ok(());
        };
        let fd = pty.master.as_raw_fd();
        let (before, epoch, was_selected) = match &self.terminal {
            Some(terminal) => (
                visible_state(terminal.screen()),
                terminal.screen().selection_epoch,
                self.selection.active(terminal.screen()),
            ),
            None => return Ok(()),
        };
        let mut bytes = [0u8; 65536];
        let mut total = 0usize;
        let mut closed = false;
        // Drain a large burst in one pass. Rendering once per burst instead of
        // once per 8 KiB read is the difference between being frame-rate bound
        // and being limited by the parser.
        const MAX_BURST: usize = 512 * 1024;
        loop {
            let n = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            if n > 0 {
                let n = n as usize;
                total += n;
                let terminal = self.terminal.as_mut().unwrap();
                let old_bracketed = terminal.screen().bracketed_paste;
                terminal.process(&bytes[..n]);
                if let Some(xray) = &self.xray {
                    xray.notify_terminal_output();
                }
                if self.debug_input && old_bracketed != terminal.screen().bracketed_paste {
                    eprintln!(
                        "[clipboard] bracketed paste={}",
                        terminal.screen().bracketed_paste
                    );
                }
                self.input.extend(terminal.take_responses());
                if let Some(title) = terminal.screen_mut().take_title()
                    && self.editor.is_none()
                    && let Some(toplevel) = &self.toplevel
                {
                    toplevel.set_title(if title.is_empty() {
                        "Beyrix".to_string()
                    } else {
                        title
                    });
                }
                if let Some(clip_text) = terminal.screen_mut().take_clipboard_write() {
                    #[cfg(test)]
                    {
                        self.test_clipboard = Some(clip_text.clone());
                    }
                    self.pending_osc52_clipboard = Some(clip_text);
                }
                let shell_events = terminal.screen_mut().drain_shell_events();
                if !shell_events.is_empty()
                    && let Some(xray) = &self.xray
                {
                    for ev in shell_events {
                        xray.notify_shell_event(ev);
                    }
                }
                if total >= MAX_BURST {
                    break;
                }
                continue;
            }
            if n < 0 {
                let error = std::io::Error::last_os_error();
                if self.debug_input {
                    eprintln!("[output] PTY read: {error}");
                }
                if error.kind() == std::io::ErrorKind::Interrupted
                    || error.kind() == std::io::ErrorKind::WouldBlock
                {
                    break;
                }
                if error.raw_os_error() == Some(libc::EIO) {
                    closed = true;
                    break;
                }
                return Err(error.into());
            }
            closed = true;
            break;
        }
        if self.debug_input {
            eprintln!("[output] PTY burst {total} bytes");
        }
        if total > 0
            && let Some(terminal) = &self.terminal
        {
            self.dirty |= before != visible_state(terminal.screen())
                || was_selected != self.selection.active(terminal.screen())
                || (terminal.screen().view_offset() < terminal.screen().rows
                    && epoch != terminal.screen().selection_epoch);
        }
        if closed {
            if self.debug_input {
                eprintln!("[output] PTY closed; reaping bash");
            }
            if let Some(pty) = self.pty.take() {
                unsafe {
                    libc::waitpid(pty.child_pid, std::ptr::null_mut(), 0);
                }
            }
        }
        Ok(())
    }

    fn editor_key(&mut self, key: u32, serial: u32, qh: &QueueHandle<Self>) {
        let Some(keyboard) = &self.xkb else {
            return;
        };
        let sym = keyboard.sym(key);
        let text = String::from_utf8_lossy(&keyboard.text(key)).into_owned();
        let press = KeyPress {
            key,
            sym,
            control: keyboard.control(),
            shift: keyboard.shift(),
            alt: keyboard.alt(),
            text,
        };
        let Some(editor) = &mut self.editor else {
            return;
        };
        let confirming = editor.is_confirming_close();
        let action = if press.control && !press.alt && !confirming {
            match press.key {
                15 => Some(EditorAction::NextTab(press.shift)),
                17 if press.shift => Some(EditorAction::Leave),
                17 => Some(EditorAction::CloseTab),
                48 => Some(EditorAction::ToggleExplorer),
                25 => Some(EditorAction::QuickOpen),
                33 => Some(EditorAction::Find),
                35 => Some(EditorAction::Replace),
                19 if press.shift => Some(EditorAction::Refresh),
                _ => None,
            }
        } else {
            None
        };
        let effect = if let Some(action) = action {
            editor.action(action)
        } else if !press.alt
            && matches!(press.sym, 0xff0d | 0xff8d)
            && let Some(effect) = editor.panel_enter(press.shift, press.control)
        {
            effect
        } else if let Some(command) = editor_command(&press, confirming) {
            editor.handle(command)
        } else {
            return;
        };
        self.apply_editor_effect(effect, serial, qh);
    }

    fn apply_editor_effect(
        &mut self,
        effect: crate::editor::Effect,
        serial: u32,
        qh: &QueueHandle<Self>,
    ) {
        use crate::editor::Effect;
        match effect {
            Effect::Copy(text) => self.set_clipboard(text, serial, qh),
            Effect::Cut(text) => self.set_clipboard(text, serial, qh),
            Effect::PasteRequest => self.paste(),
            Effect::Close => self.close_editor(),
            Effect::CloseWindow => self.running = false,
            Effect::None => return,
            Effect::Changed => {}
        }
        self.dirty = true;
    }

    fn redraw(&mut self, qh: &QueueHandle<Self>) -> Result<(), Box<dyn Error>> {
        if !self.configured || !self.dirty || self.frame_pending {
            return Ok(());
        }
        // DEC mode 2026 Synchronized Output: defer presenting intermediate frames
        // until the application ends the synchronized update (or safety timeout elapses).
        if self.editor.is_none()
            && self
                .terminal
                .as_ref()
                .is_some_and(|t| t.screen().is_synchronized_active())
        {
            return Ok(());
        }
        if self.backend == ActiveBackend::OpenGl && self.gl.is_some() {
            let logical = (self.width as usize, self.height as usize);
            let bg_active = self
                .background
                .as_ref()
                .is_some_and(|b| b.is_active(self.keyboard_focus, self.in_tui_mode()));
            let force_full =
                self.damage_full || self.editor.is_some() || bg_active || self.platform_dirty;
            self.damage_full = false;
            let mut damage = if let Some(terminal) = &self.terminal {
                terminal_damage(
                    terminal.screen(),
                    &self.selection,
                    self.keyboard_focus,
                    self.blink_on,
                    logical,
                    &mut self.damage_hashes,
                    &mut self.damage_signature,
                    force_full,
                )
            } else {
                RowDamage::full()
            };
            if damage.is_empty() && !self.platform_dirty {
                self.dirty = false;
                return Ok(());
            }
            if self.platform_dirty {
                damage = RowDamage::full();
            }
            let mut full = damage.full;
            if !full {
                let (partial, age) = match &self.gl {
                    Some(gl) => (gl.can_partial(), gl.buffer_age().unwrap_or(0)),
                    None => (false, 0),
                };
                if !partial || age <= 0 {
                    full = true;
                } else {
                    let extra = (age as usize).saturating_sub(1);
                    if extra > self.damage_history.len() {
                        full = true;
                    } else {
                        for past in self.damage_history.iter().rev().take(extra) {
                            damage.merge(past);
                            if damage.full {
                                full = true;
                                break;
                            }
                        }
                    }
                }
            }
            if full {
                damage = RowDamage::full();
            }
            match self.render_gl_frame(full, &damage, qh) {
                Ok(()) => {
                    self.frame_pending = true;
                    self.dirty = false;
                    self.damage_history
                        .push_back(if full { RowDamage::full() } else { damage });
                    while self.damage_history.len() > 4 {
                        self.damage_history.pop_front();
                    }
                }
                Err(reason) => {
                    self.fallback_to_software(reason);
                    self.redraw_software(qh)?;
                }
            }
            return Ok(());
        }
        self.redraw_software(qh)
    }

    fn redraw_software(&mut self, qh: &QueueHandle<Self>) -> Result<(), Box<dyn Error>> {
        let sizes = self.surface_sizes();
        let Some(index) = self.software_buffer(sizes.physical, qh)? else {
            return Ok(());
        };
        if let Some(surface) = &self.surface {
            self.map_surface_buffer(surface, sizes);
        }
        if let (Some(_), Some(_), Some(_)) = (&self.surface, &self.terminal, &mut self.renderer) {
            let dimensions = sizes.buffer_dimensions();
            let insets = self.dock_insets_tuple(sizes.scale.factor());
            let renderer = self.renderer.as_mut().unwrap();
            let terminal = self.terminal.as_ref().unwrap();
            let mut scene = match &mut self.editor {
                Some(editor) => {
                    let (columns, rows) = sizes.grid_size(renderer);
                    let view = editor.view(columns, rows);
                    renderer.build_editor_scene(
                        &view,
                        dimensions,
                        self.keyboard_focus,
                        self.blink_on,
                        sizes.scale.factor(),
                    )
                }
                None => renderer.build_terminal_scene_with_insets(
                    terminal.screen(),
                    dimensions,
                    &self.selection,
                    self.keyboard_focus,
                    self.blink_on,
                    sizes.scale.factor(),
                    None,
                    insets,
                ),
            };
            let in_tui = self.in_tui_mode();
            let ptr = sizes.physical_point(self.pointer_position);
            let theme = self
                .terminal
                .as_ref()
                .map_or_else(crate::theme::Theme::default, |t| t.screen().theme().clone());
            self.emit_platform_layers(
                &mut scene,
                (dimensions.0 as f32, dimensions.1 as f32),
                sizes.scale.factor(),
                in_tui,
                (ptr.0 as f32, ptr.1 as f32),
                &theme,
            );
            let frame = &mut self.buffers[index];
            let (Some(surface), Some(renderer)) = (&self.surface, &mut self.renderer) else {
                return Ok(());
            };
            renderer.paint_scene(&scene, dimensions.0, frame.pixels());
            surface.attach(Some(&frame.buffer), 0, 0);
            if surface.version() >= 4 {
                surface.damage_buffer(0, 0, sizes.physical.0, sizes.physical.1);
            } else {
                surface.damage(0, 0, sizes.logical.0, sizes.logical.1);
            }
            surface.frame(qh, ());
            surface.commit();
            self.frame_pending = true;
            frame.busy = true;
            self.dirty = false;
            self.platform_dirty = false;
        }
        Ok(())
    }

    fn render_gl_frame(
        &mut self,
        full: bool,
        damage: &RowDamage,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
        let sizes = self.surface_sizes();
        let dimensions = sizes.buffer_dimensions();
        let scale = sizes.scale.factor();
        let physical = sizes.physical;
        let insets = self.dock_insets_tuple(scale);
        let renderer = self.renderer.as_ref().ok_or("renderer unavailable")?;
        let geometry = sizes
            .geometry(renderer)
            .with_insets(insets.0, insets.1, insets.2, insets.3);
        let grid = geometry.grid_size(dimensions.0, dimensions.1);
        let rects: Vec<PixelRect> = if full {
            vec![PixelRect {
                x: 0,
                y: 0,
                w: physical.0,
                h: physical.1,
            }]
        } else {
            damage
                .ranges
                .iter()
                .map(|range| row_pixel_rect(geometry, physical, *range))
                .collect()
        };
        if rects.is_empty() {
            return Ok(());
        }
        if let Some(surface) = &self.surface {
            self.map_surface_buffer(surface, sizes);
        }
        let renderer = self.renderer.as_mut().ok_or("renderer unavailable")?;
        if full {
            let mut scene = match &mut self.editor {
                Some(editor) => {
                    let (columns, rows) = grid;
                    let view = editor.view(columns, rows);
                    renderer.build_editor_scene(
                        &view,
                        dimensions,
                        self.keyboard_focus,
                        self.blink_on,
                        scale,
                    )
                }
                None => {
                    let insets = self.dock_insets_tuple(scale);
                    let renderer = self.renderer.as_mut().ok_or("renderer unavailable")?;
                    let terminal = self.terminal.as_ref().ok_or("terminal unavailable")?;
                    renderer.build_terminal_scene_with_insets(
                        terminal.screen(),
                        dimensions,
                        &self.selection,
                        self.keyboard_focus,
                        self.blink_on,
                        scale,
                        None,
                        insets,
                    )
                }
            };
            let in_tui = self.in_tui_mode();
            let ptr = sizes.physical_point(self.pointer_position);
            let theme = self
                .terminal
                .as_ref()
                .map_or_else(crate::theme::Theme::default, |t| t.screen().theme().clone());
            self.emit_platform_layers(
                &mut scene,
                (dimensions.0 as f32, dimensions.1 as f32),
                scale,
                in_tui,
                (ptr.0 as f32, ptr.1 as f32),
                &theme,
            );
            let renderer = self.renderer.as_mut().ok_or("renderer unavailable")?;
            let gl = self.gl.as_mut().ok_or("OpenGL backend unavailable")?;
            gl.draw(renderer, &scene, &rects, true)?;
        } else {
            for (rect, range) in rects.iter().zip(damage.ranges.iter()) {
                if rect.h == 0 {
                    continue;
                }
                let renderer = self.renderer.as_mut().ok_or("renderer unavailable")?;
                let mut scene = match &mut self.editor {
                    Some(editor) => {
                        let (columns, rows) = grid;
                        let view = editor.view(columns, rows);
                        renderer.build_editor_scene(
                            &view,
                            dimensions,
                            self.keyboard_focus,
                            self.blink_on,
                            scale,
                        )
                    }
                    None => {
                        let insets = self.dock_insets_tuple(scale);
                        let renderer = self.renderer.as_mut().ok_or("renderer unavailable")?;
                        let terminal = self.terminal.as_ref().ok_or("terminal unavailable")?;
                        renderer.build_terminal_scene_with_insets(
                            terminal.screen(),
                            dimensions,
                            &self.selection,
                            self.keyboard_focus,
                            self.blink_on,
                            scale,
                            Some(*range),
                            insets,
                        )
                    }
                };
                let in_tui = self.in_tui_mode();
                let ptr = sizes.physical_point(self.pointer_position);
                let theme = self
                    .terminal
                    .as_ref()
                    .map_or_else(crate::theme::Theme::default, |t| t.screen().theme().clone());
                self.emit_platform_layers(
                    &mut scene,
                    (dimensions.0 as f32, dimensions.1 as f32),
                    scale,
                    in_tui,
                    (ptr.0 as f32, ptr.1 as f32),
                    &theme,
                );
                let renderer = self.renderer.as_mut().ok_or("renderer unavailable")?;
                let gl = self.gl.as_mut().ok_or("OpenGL backend unavailable")?;
                gl.draw(renderer, &scene, &[*rect], false)?;
            }
        }
        if let Some(surface) = &self.surface {
            surface.frame(qh, ());
        }
        self.platform_dirty = false;
        let gl = self.gl.as_mut().ok_or("OpenGL backend unavailable")?;
        gl.present(&rects, full)
    }

    fn in_tui_mode(&self) -> bool {
        const TUI_NAMES: &[&str] = &[
            "vim", "nvim", "vi", "nano", "htop", "btop", "top", "tmux", "screen", "less", "man",
            "mc", "lazygit", "tig", "ranger", "yazi",
        ];
        let Some(terminal) = &self.terminal else {
            return false;
        };
        let screen = terminal.screen();
        let known_tui_foreground = self
            .pty
            .as_ref()
            .and_then(Pty::foreground_comm)
            .is_some_and(|comm| TUI_NAMES.contains(&comm.to_ascii_lowercase().as_str()));
        let signals = crate::widgets::layout::TuiSignals {
            alternate_screen: screen.is_alternate_screen(),
            mouse_reporting: screen.mouse_mode() != crate::mouse::Mode::Off,
            bracketed_paste: screen.bracketed_paste,
            full_scroll_margins: !screen.has_custom_scroll_margins(),
            known_tui_foreground,
        };
        signals.is_fullscreen_tui()
    }

    fn dock_insets_tuple(&self, scale: f32) -> (f32, f32, f32, f32) {
        self.widgets.as_ref().map_or((0.0, 0.0, 0.0, 0.0), |w| {
            let i = w.dock_insets(self.in_tui_mode(), scale);
            (i.left, i.top, i.right, i.bottom)
        })
    }

    fn effective_grid_size(
        &self,
        sizes: SurfaceSizes,
        renderer: &render::Renderer,
    ) -> (usize, usize) {
        let (w, h) = sizes.buffer_dimensions();
        let insets = self.dock_insets_tuple(sizes.scale.factor());
        sizes
            .geometry(renderer)
            .with_insets(insets.0, insets.1, insets.2, insets.3)
            .grid_size(w, h)
    }

    fn sync_pty_grid_after_layout_change(&mut self) {
        let Some(renderer) = &self.renderer else {
            return;
        };
        let sizes = self.surface_sizes();
        let (cols, rows) = self.effective_grid_size(sizes, renderer);
        let geom = sizes.geometry(renderer);
        let xpix = (cols * geom.cell_width).min(u16::MAX as usize) as u16;
        let ypix = (rows * geom.cell_height).min(u16::MAX as usize) as u16;
        if let Some(terminal) = &mut self.terminal {
            terminal.screen_mut().set_pixel_metrics(
                (cols * geom.cell_width, rows * geom.cell_height),
                (geom.cell_width, geom.cell_height),
            );
            if (terminal.screen().columns, terminal.screen().rows) != (cols, rows) {
                if let Some(pty) = &self.pty {
                    let _ = pty.resize_with_pixels(cols as u16, rows as u16, xpix, ypix);
                }
                terminal.resize(cols, rows);
                self.damage_full = true;
                self.dirty = true;
            }
        }
    }

    /// Reconcile live runtime subsystems (Widgets, Background, Media, Audio, X-Ray)
    /// against `self.platform_config`. Starts newly enabled subsystems, cleanly drops
    /// disabled ones, updates live configs, and adjusts PTY viewport if Dock insets changed.
    fn reconcile_platform_subsystems(&mut self) {
        let cfg = self.platform_config.clone();
        let theme = self
            .terminal
            .as_ref()
            .map_or_else(crate::theme::Theme::default, |t| t.screen().theme().clone());

        // 1. Media & System Audio Capture
        if cfg.media.enabled {
            let needs_restart = self.media.as_ref().is_none_or(|m| {
                m.config.mpris_enabled != cfg.media.mpris_enabled
                    || m.config.player_enabled != cfg.media.player_enabled
                    || m.config.audio_capture_enabled != cfg.media.audio_capture_enabled
                    || m.config.audio_capture_source != cfg.media.audio_capture_source
            });
            if needs_restart {
                self.media = None;
                let mut media = crate::media::MediaManager::new(cfg.media.clone());
                media.ensure_started(self.bus.clone());
                self.media = Some(media);
            }
        } else {
            self.media = None;
        }

        // 2. 3D Background Engine
        if cfg.background.enabled {
            if let Some(bg) = &mut self.background {
                bg.config = cfg.background.clone();
            } else {
                self.background = Some(crate::background::BackgroundEngine::new(
                    cfg.background.clone(),
                ));
            }
        } else {
            self.background = None;
        }

        // 3. X-Ray Diagnostics
        if cfg.xray.enabled {
            if let Some(xr) = &mut self.xray {
                xr.set_level(cfg.xray.level);
                xr.config = cfg.xray.clone();
            } else if let Some(pty) = &self.pty {
                let mut xray = crate::xray::XRayEngine::new(cfg.xray.clone());
                xray.ensure_started(
                    pty.child_pid,
                    Some(pty.master.as_raw_fd()),
                    self.bus.clone(),
                );
                self.xray = Some(xray);
            }
        } else {
            self.xray = None;
            self.xray_hits.clear();
        }

        // 4. BeyrixWidgets JS Runtime
        if cfg.widgets.enabled {
            if let Some(w) = &mut self.widgets {
                w.config = cfg.widgets.clone();
            } else {
                let mut widgets = crate::widgets::WidgetManager::new(cfg.widgets.clone());
                if let Err(e) = widgets.ensure_started(theme, self.bus.clone()) {
                    self.settings_overlay.status_message = Some(format!("Widgets error: {e}"));
                }
                self.widgets = Some(widgets);
            }
        } else {
            self.widgets = None;
        }

        // 5. Recompute PTY viewport in case Dock insets appeared or disappeared.
        self.sync_pty_grid_after_layout_change();
        self.damage_full = true;
        self.platform_dirty = true;
        self.dirty = true;
    }

    fn init_platform_subsystems(&mut self, child_pid: libc::pid_t, pty_master_fd: i32) {
        let cfg = load_platform_config();
        self.platform_config = cfg.clone();
        let theme = self
            .terminal
            .as_ref()
            .map_or_else(crate::theme::Theme::default, |t| t.screen().theme().clone());
        if cfg.media.enabled {
            let mut media = crate::media::MediaManager::new(cfg.media.clone());
            media.ensure_started(self.bus.clone());
            self.media = Some(media);
        }
        if cfg.background.enabled {
            self.background = Some(crate::background::BackgroundEngine::new(
                cfg.background.clone(),
            ));
        }
        if cfg.xray.enabled {
            let mut xray = crate::xray::XRayEngine::new(cfg.xray.clone());
            xray.ensure_started(child_pid, Some(pty_master_fd), self.bus.clone());
            self.xray = Some(xray);
        }
        if cfg.widgets.enabled {
            let mut widgets = crate::widgets::WidgetManager::new(cfg.widgets.clone());
            if let Err(e) = widgets.ensure_started(theme, self.bus.clone()) {
                eprintln!("beyrix widgets: {e}");
            }
            self.widgets = Some(widgets);
        }
    }

    fn pump_platform(&mut self) {
        if let Some(widgets) = &mut self.widgets {
            if widgets.poll_hot_reload() > 0 {
                self.platform_dirty = true;
                self.dirty = true;
            }
            if widgets.take_dirty() {
                self.platform_dirty = true;
                self.dirty = true;
            }
            for action in widgets.drain_actions() {
                match action {
                    crate::widgets::runtime::HostAction::MediaPlayPause => {
                        if let Some(m) = &self.media {
                            m.play_pause();
                        }
                    }
                    crate::widgets::runtime::HostAction::MediaNext => {
                        if let Some(m) = &self.media {
                            m.next();
                        }
                    }
                    crate::widgets::runtime::HostAction::MediaPrev => {
                        if let Some(m) = &self.media {
                            m.prev();
                        }
                    }
                    crate::widgets::runtime::HostAction::MediaSeek(s) => {
                        if let Some(m) = &self.media {
                            m.seek(s);
                        }
                    }
                    crate::widgets::runtime::HostAction::MediaSetVolume(v) => {
                        if let Some(m) = &self.media {
                            m.set_volume(v);
                        }
                    }
                    crate::widgets::runtime::HostAction::MediaPlayFile(p) => {
                        if let Some(m) = &self.media {
                            m.play_file(std::path::PathBuf::from(p));
                        }
                    }
                    crate::widgets::runtime::HostAction::MediaEnqueue(p) => {
                        if let Some(m) = &self.media {
                            m.enqueue_file(std::path::PathBuf::from(p));
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundSetPreset { owner, preset } => {
                        if let (Some(bg), Some(preset)) = (
                            &mut self.background,
                            crate::config::BackgroundPreset::parse(&preset),
                        ) && bg.set_preset(Some(&owner), preset).is_ok()
                        {
                            self.platform_dirty = true;
                            self.dirty = true;
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundReleaseScene { owner } => {
                        if let Some(bg) = &mut self.background {
                            bg.release_scene(&owner);
                            self.platform_dirty = true;
                            self.dirty = true;
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundClearScene { owner } => {
                        if let Some(bg) = &mut self.background
                            && bg.clear_scene(&owner).is_ok()
                        {
                            self.platform_dirty = true;
                            self.dirty = true;
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundAddObject { owner, spec } => {
                        if let Some(bg) = &mut self.background {
                            let _ = bg.add_object(&owner, spec);
                            self.dirty = true;
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundUpdateObject {
                        owner,
                        id,
                        position,
                        rotation,
                        scale,
                        color,
                    } => {
                        if let Some(bg) = &mut self.background {
                            bg.update_object(&owner, &id, position, rotation, scale, color);
                            self.dirty = true;
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundRemoveObject { owner, id } => {
                        if let Some(bg) = &mut self.background {
                            bg.remove_object(&owner, &id);
                            self.dirty = true;
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundSetCamera {
                        owner,
                        position,
                        target,
                        fov_deg,
                        ortho,
                    } => {
                        if let Some(bg) = &mut self.background {
                            bg.set_camera(&owner, position, target, fov_deg, ortho);
                            self.dirty = true;
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundSetLight {
                        owner,
                        direction,
                        color,
                        intensity,
                    } => {
                        if let Some(bg) = &mut self.background {
                            bg.set_light(&owner, direction, color, intensity);
                            self.dirty = true;
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundSetCharset {
                        owner,
                        charset,
                    } => {
                        if let Some(bg) = &mut self.background
                            && bg.set_charset(&owner, charset).is_ok()
                        {
                            self.platform_dirty = true;
                            self.dirty = true;
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundBind { owner, binding } => {
                        if let Some(bg) = &mut self.background {
                            let _ = bg.add_binding(&owner, binding);
                        }
                    }
                    crate::widgets::runtime::HostAction::BackgroundUnbind { owner, id } => {
                        if let Some(bg) = &mut self.background {
                            let _ = bg.remove_binding(&owner, &id);
                        }
                    }
                    crate::widgets::runtime::HostAction::WidgetLog {
                        extension_id,
                        level,
                        message,
                    } => {
                        if self.debug_input {
                            eprintln!("[widget:{extension_id}:{level}] {message}");
                        }
                    }
                }
            }
        }
        let in_tui = self.in_tui_mode();
        if self.widgets.is_some()
            && let Some(renderer) = &self.renderer
        {
            let sizes = self.surface_sizes();
            let (cols, rows) = self.effective_grid_size(sizes, renderer);
            if let Some(terminal) = &mut self.terminal
                && (terminal.screen().columns, terminal.screen().rows) != (cols, rows)
            {
                if let Some(pty) = &self.pty {
                    let _ = pty.resize(cols as u16, rows as u16);
                }
                terminal.resize(cols, rows);
                self.damage_full = true;
                self.dirty = true;
            }
        }
        if let Some(xray) = &self.xray
            && xray.take_dirty()
        {
            self.platform_dirty = true;
            self.dirty = true;
        }
        if let Some(bg) = &self.background
            && bg.is_active(self.keyboard_focus, in_tui)
        {
            if let Some(m) = &self.media {
                let wants_audio = bg.config.audio_reactive
                    || bg.config.preset == crate::config::BackgroundPreset::AudioReactive
                    || !bg.bindings.is_empty();
                m.set_native_audio_consumer(wants_audio);
            }
            if bg.should_step() {
                self.dirty = true;
            }
        } else if let Some(m) = &self.media {
            m.set_native_audio_consumer(false);
        }
    }

    fn emit_platform_layers(
        &mut self,
        scene: &mut render::Scene,
        win_size: (f32, f32),
        scale: f32,
        in_tui: bool,
        pointer_px: (f32, f32),
        theme: &crate::theme::Theme,
    ) {
        self.sync_active_panel();
        let insets = self.dock_insets_tuple(scale);
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        if let Some(bg) = &mut self.background {
            let mut inputs = crate::background::bindings::BindingInputs::default();
            if let Some(m) = &self.media {
                let a = m.audio_snapshot();
                inputs.audio_bass = a.bass;
                inputs.audio_mid = a.mid;
                inputs.audio_treble = a.treble;
                inputs.audio_rms = a.rms;
                inputs.audio_peak = a.peak;
                inputs.audio_beat = if a.beat { 1.0 } else { 0.0 };
            }
            if let Some(x) = &self.xray {
                let s = x.snapshot();
                inputs.xray_activity = (s.total_cpu_percent / 100.0).clamp(0.0, 1.0);
            }
            let mouse_norm = (
                (pointer_px.0 / win_size.0.max(1.0)).clamp(0.0, 1.0),
                (pointer_px.1 / win_size.1.max(1.0)).clamp(0.0, 1.0),
            );
            bg.render_to_scene(
                win_size,
                scale,
                self.keyboard_focus,
                in_tui,
                mouse_norm,
                &inputs,
                theme,
                renderer,
                scene,
            );
        }
        use crate::widgets::compositor::{ActiveSystemPanel, SurfaceOwner};
        self.ui_compositor.clear_surfaces();
        self.xray_hits.clear();

        if let Some(widgets) = &mut self.widgets {
            let logical_w = win_size.0 / scale.max(0.25);
            let logical_h = win_size.1 / scale.max(0.25);
            widgets.update_window(logical_w, logical_h, scale, self.keyboard_focus);
            let g = renderer
                .geometry(scale)
                .with_insets(insets.0, insets.1, insets.2, insets.3);
            let inline_y_px = self
                .terminal
                .as_ref()
                .and_then(|t| t.screen().latest_inline_display_row())
                .map(|row| g.origin_y() as f32 + row as f32 * g.cell_height as f32);
            let cursor_row_px = self.terminal.as_ref().map(|t| {
                (
                    g.origin_y() as f32
                        + t.screen().cursor_display_row() as f32 * g.cell_height as f32,
                    g.cell_height as f32,
                )
            });
            widgets.emit_to_scene_with_config(
                Some(&self.platform_config),
                win_size,
                scale,
                (g.cell_width as f32, g.cell_height as f32),
                in_tui,
                pointer_px,
                inline_y_px,
                cursor_row_px,
                theme,
                renderer,
                scene,
            );
            for rect in widgets.resolved_rects.values() {
                if rect.visible && (rect.interactive || widgets.editor.active) {
                    self.ui_compositor.register_surface(
                        rect.x,
                        rect.y,
                        rect.w,
                        rect.h,
                        SurfaceOwner::Widget,
                    );
                }
            }
            if widgets.editor.active {
                for &(ex, ey, ew, eh) in &widgets.editor_panel_rects {
                    self.ui_compositor
                        .register_surface(ex, ey, ew, eh, SurfaceOwner::Studio);
                }
            }
        }
        // Only emit X-Ray when Studio and Settings are not covering the workspace.
        if !self.ui_compositor.is_open(ActiveSystemPanel::Studio)
            && !self.ui_compositor.is_open(ActiveSystemPanel::Settings)
            && let Some(xray) = &self.xray
        {
            let rects = xray.emit_to_scene(
                win_size,
                scale,
                pointer_px,
                theme,
                renderer,
                scene,
                &mut self.xray_hits,
            );
            if let Some((ix, iy, iw, ih)) = rects.indicator {
                self.ui_compositor.register_surface(
                    ix,
                    iy,
                    iw,
                    ih,
                    SurfaceOwner::XRayIndicator,
                );
            }
            if let Some((px, py, pw, ph)) = rects.panel {
                self.ui_compositor
                    .register_surface(px, py, pw, ph, SurfaceOwner::XRayPanel);
            }
        }
        if self.settings_overlay.open {
            self.settings_overlay.emit_to_scene(
                &self.platform_config,
                win_size,
                scale,
                pointer_px,
                theme,
                renderer,
                scene,
            );
            if let Some((sx, sy, sw, sh)) = self.settings_overlay.panel_rect {
                self.ui_compositor
                    .register_surface(sx, sy, sw, sh, SurfaceOwner::Settings);
            }
        }
    }

    fn sync_active_panel(&mut self) {
        use crate::widgets::compositor::ActiveSystemPanel;
        let active = self.ui_compositor.active_panel;
        self.settings_overlay.open = active == ActiveSystemPanel::Settings;
        if !self.settings_overlay.open {
            self.settings_overlay.status_message = None;
            self.settings_overlay.hits.clear();
            self.settings_overlay.panel_rect = None;
        }
        if let Some(w) = &mut self.widgets {
            let want_studio = active == ActiveSystemPanel::Studio;
            if w.editor.active && !want_studio {
                let _ = w.persist_profiles();
                w.editor.toggle();
                w.editor_hits.clear();
                w.editor_panel_rects.clear();
            } else if !w.editor.active && want_studio {
                w.editor.toggle();
            }
        }
        if let Some(xray) = &mut self.xray {
            let want_xray = active == ActiveSystemPanel::XRay;
            if xray.panel_open != want_xray {
                xray.panel_open = want_xray;
            }
        }
    }
}

/// Safely open an OSC 8 hyperlink only if its scheme is explicitly allowlisted
/// (`http://`, `https://`, or `file://`) and it contains no control characters.
fn open_safe_hyperlink(url: &str) {
    let trimmed = url.trim();
    let allowed = trimmed.starts_with("https://")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("file://");
    if !allowed || trimmed.len() > 2048 || trimmed.chars().any(|c| c.is_control()) {
        return;
    }
    let _ = std::process::Command::new("xdg-open")
        .arg(trimmed)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn load_platform_config() -> crate::config::PlatformConfig {
    if let Some(path) = crate::config::PlatformConfig::config_path() {
        let (cfg, err) = crate::config::PlatformConfig::load_with_diagnostics(&path);
        if let Some(e) = err {
            eprintln!("beyrix platform config: {e}");
        }
        return cfg;
    }
    crate::config::PlatformConfig::default()
}

#[derive(PartialEq, Eq)]
struct VisibleState {
    start: usize,
    offset: usize,
    revision: u64,
    graphics_revision: u64,
    progress: Option<(u8, u8)>,
    cursor: Option<(usize, usize, Option<crate::terminal::CursorStyle>)>,
}

fn visible_state(screen: &crate::terminal::Screen) -> VisibleState {
    // When every visible row is in history, PTY text writes cannot change visible cells,
    // but graphics/progress state can still change.
    let revision = if screen.view_offset() >= screen.rows {
        0
    } else {
        screen.revision
    };
    let cursor = (screen.cursor_visible && screen.view_offset() == 0).then_some((
        render::Renderer::cursor_x(screen),
        screen.cursor_display_row(),
        screen.cursor_style,
    ));
    VisibleState {
        start: screen.visible_start(),
        offset: screen.layout_offset(),
        revision,
        graphics_revision: screen.graphics.revision,
        progress: screen.progress,
        cursor,
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.frame_pending = false;
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                        name,
                        version.min(6),
                        qh,
                        (),
                    ));
                }

                "wl_seat" if state.seat.is_none() => {
                    if state.debug_input {
                        eprintln!("[input] wl_seat global name={name} version={version}");
                    }
                    state.seat =
                        Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(8), qh, ()));
                    state.setup_data_device(qh);
                }
                "wl_data_device_manager" => {
                    state.data_manager = Some(
                        registry.bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(
                            name,
                            version.min(3),
                            qh,
                            (),
                        ),
                    );
                    state.setup_data_device(qh);
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
                }

                "xdg_wm_base" => {
                    state.wm_base =
                        Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, 1, qh, ()));
                }
                "wp_fractional_scale_manager_v1" => {
                    state.fractional_manager = Some(
                        registry.bind::<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ),
                    );
                }
                "wp_viewporter" => {
                    state.viewporter = Some(registry.bind::<wp_viewporter::WpViewporter, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    ));
                }

                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let Some(size) = state.proposed_size.take() {
                state.pending_size = Some(size);
            }

            if !state.configured {
                state.configured = true;
                state.dirty = true;
                if state.debug_input {
                    eprintln!("[output] xdg_surface configured; initial frame scheduled");
                }
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Close => {
                if let Some(editor) = &mut state.editor {
                    if editor.request_window_close() {
                        state.running = false;
                    } else {
                        state.dirty = true;
                    }
                } else {
                    state.running = false;
                }
            }
            xdg_toplevel::Event::Configure { width, height, .. } => {
                let previous = state
                    .proposed_size
                    .or(state.pending_size)
                    .unwrap_or((state.width, state.height));
                state.proposed_size = Some(resolve_toplevel_configure(previous, width, height));
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event
            && state.fractional_scale.is_none()
        {
            state.pending_scale = Some(Scale(factor.clamp(1, 4) as u32 * 120));
        }
    }
}
impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.pending_scale = Some(Scale(scale.clamp(60, 480)));
        }
    }
}
delegate_noop!(State: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
impl Dispatch<wl_buffer::WlBuffer, u64> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(index) = state.buffers.iter().position(|frame| frame.id == *id) {
                if state.buffers[index].generation != state.generation {
                    state.buffers.remove(index);
                } else {
                    state.buffers[index].busy = false;
                }
            }
            if state.debug_input {
                eprintln!("[output] buffer {id} released, dirty={}", state.dirty);
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(caps),
        } = event
        {
            if state.debug_input {
                eprintln!("[input] seat capabilities: {caps:?}");
            }
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            } else if !caps.contains(wl_seat::Capability::Pointer) {
                if let Some(pointer) = state.pointer.take()
                    && pointer.version() >= 3
                {
                    pointer.release();
                }
                state.pointer_focus = false;
                state.wheel_axis = 0.0;
                state.wheel_steps = None;
                state.wheel_remainder = 0.0;
                state.selection.release();
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                if state.debug_input {
                    eprintln!("[input] creating wl_keyboard");
                }
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            } else if !caps.contains(wl_seat::Capability::Keyboard) {
                if let Some(k) = state.keyboard.take()
                    && k.version() >= 3
                {
                    k.release();
                }
                if state.debug_input {
                    eprintln!("[input] keyboard capability removed");
                }
                state.keyboard_focus = false;
                state.repeat_key = None;
                state.pressed_keys.clear();
                state.xkb = None;
            }
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_focus = state.surface.as_ref() == Some(&surface);
                state.pointer_position = (surface_x, surface_y);
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_focus = false;
                state.wheel_axis = 0.0;
                state.wheel_steps = None;
                state.wheel_remainder = 0.0;
                state.wheel_mouse_remainder = 0.0;
                if let Some(button) = state.mouse_button.take()
                    && state.mouse_mode() != crate::mouse::Mode::Off
                {
                    state.report_mouse(Some(button), false, false);
                }
                state.selection.release();
                if let Some(editor) = &mut state.editor {
                    editor.mouse_release();
                }
            }
            wl_pointer::Event::Axis {
                axis: wayland_client::WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value,
                ..
            } if state.pointer_focus => {
                state.wheel_axis += value;
                if pointer.version() < 5 {
                    state.scroll_pointer();
                }
            }
            wl_pointer::Event::Axis {
                axis: wayland_client::WEnum::Value(wl_pointer::Axis::HorizontalScroll),
                value,
                ..
            } if state.pointer_focus => {
                state.wheel_h_axis += value;
                if pointer.version() < 5 {
                    state.scroll_pointer();
                }
            }
            wl_pointer::Event::AxisDiscrete {
                axis: wayland_client::WEnum::Value(wl_pointer::Axis::VerticalScroll),
                discrete,
            } if state.pointer_focus => {
                *state.wheel_steps.get_or_insert(0.0) += f64::from(discrete);
            }
            wl_pointer::Event::AxisDiscrete {
                axis: wayland_client::WEnum::Value(wl_pointer::Axis::HorizontalScroll),
                discrete,
            } if state.pointer_focus => {
                *state.wheel_h_steps.get_or_insert(0.0) += f64::from(discrete);
            }
            wl_pointer::Event::AxisValue120 {
                axis: wayland_client::WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value120,
            } if state.pointer_focus => {
                *state.wheel_steps.get_or_insert(0.0) += f64::from(value120) / 120.0;
            }
            wl_pointer::Event::AxisValue120 {
                axis: wayland_client::WEnum::Value(wl_pointer::Axis::HorizontalScroll),
                value120,
            } if state.pointer_focus => {
                *state.wheel_h_steps.get_or_insert(0.0) += f64::from(value120) / 120.0;
            }
            wl_pointer::Event::Frame if state.pointer_focus => state.scroll_pointer(),
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_position = (surface_x, surface_y);
                state.pointer_motion();
            }
            wl_pointer::Event::Button {
                serial,
                button,
                state: wayland_client::WEnum::Value(button_state),
                time,
                ..
            } => {
                state.last_serial = serial;
                if !state.pointer_focus {
                    return;
                }
                let Some(button) = crate::mouse::Button::from_linux(button) else {
                    return;
                };
                match button_state {
                    wl_pointer::ButtonState::Pressed => {
                        state.pointer_button(button, true, time);
                    }
                    wl_pointer::ButtonState::Released => {
                        state.pointer_button(button, false, time);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: wayland_client::WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                if state.debug_input {
                    eprintln!("[input] keymap xkb_v1 size={size}");
                }
                let result = crate::keyboard::Keyboard::new().and_then(|mut x| {
                    x.keymap(fd, size)?;
                    Ok(x)
                });
                match result {
                    Ok(x) => {
                        if state.debug_input {
                            eprintln!("[input] keymap loaded");
                        }
                        if let Some(ev) = x.layout_event() {
                            state
                                .bus
                                .publish(crate::events::AppEvent::KeyboardLayout(ev));
                        }
                        state.xkb = Some(x)
                    }
                    Err(e) => state.input_error = Some(format!("load keyboard keymap: {e}")),
                }
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                state.last_serial = serial;
                let focused = state.surface.as_ref() == Some(&surface);
                state.keyboard_focus = focused;
                state.dirty = true;
                if focused
                    && state.editor.is_none()
                    && state
                        .terminal
                        .as_ref()
                        .is_some_and(|t| t.screen().focus_reporting)
                {
                    state.input.extend(b"\x1b[I");
                }
                if state.debug_input {
                    eprintln!("[input] keyboard enter: focused={}", state.keyboard_focus);
                }
            }
            wl_keyboard::Event::Leave { .. } => {
                state.keyboard_focus = false;
                state.repeat_key = None;
                state.pressed_keys.clear();
                state.dirty = true;
                if state.editor.is_none()
                    && state
                        .terminal
                        .as_ref()
                        .is_some_and(|t| t.screen().focus_reporting)
                {
                    state.input.extend(b"\x1b[O");
                }
                if state.debug_input {
                    eprintln!("[input] keyboard leave");
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.repeat_rate = rate.max(0);
                state.repeat_delay = delay.max(0);
                if state.repeat_rate == 0 {
                    state.repeat_key = None;
                }
                if state.debug_input {
                    eprintln!("[input] repeat_info rate={rate} delay={delay}");
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if state.debug_input {
                    eprintln!(
                        "[input] modifiers depressed={mods_depressed:#x} latched={mods_latched:#x} locked={mods_locked:#x} group={group}"
                    );
                }
                if let Some(x) = &mut state.xkb {
                    let prev_group = x.active_layout_index();
                    x.modifiers(mods_depressed, mods_latched, mods_locked, group);
                    if x.active_layout_index() != prev_group
                        && let Some(ev) = x.layout_event()
                    {
                        state
                            .bus
                            .publish(crate::events::AppEvent::KeyboardLayout(ev));
                        if state.widgets.is_some() {
                            state.dirty = true;
                        }
                    }
                }
            }
            wl_keyboard::Event::Key {
                key,
                serial,
                state: key_state,
                ..
            } => {
                if state.debug_input {
                    eprintln!(
                        "[input] key code={key} state={key_state:?} focus={} keymap={}",
                        state.keyboard_focus,
                        state.xkb.is_some()
                    );
                }
                if key_state == wayland_client::WEnum::Value(wl_keyboard::KeyState::Released) {
                    if state.repeat_key.as_ref().is_some_and(|r| r.key == key) {
                        state.repeat_key = None;
                    }
                    state.dispatch_terminal_key_release(key);
                    return;
                }
                if state.keyboard_focus
                    && key_state == wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed)
                    && state.xkb.is_some()
                {
                    let should_repeat = if state.editor.is_some() {
                        state.pressed_keys.insert(
                            key,
                            PressedKeyRoute {
                                sent_to_pty: false,
                                kitty_report_release: false,
                            },
                        );
                        state.editor_key(key, serial, qh);
                        true
                    } else {
                        state.dispatch_terminal_key_press(key, serial, Some(qh))
                    };
                    if should_repeat
                        && state.repeat_rate > 0
                        && state.xkb.as_ref().is_some_and(|x| x.key_repeats(key))
                    {
                        state.repeat_key = Some(RepeatKey {
                            key,
                            serial,
                            next_fire: std::time::Instant::now()
                                + Duration::from_millis(state.repeat_delay.max(0) as u64),
                        });
                    } else {
                        state.repeat_key = None;
                    }
                }
            }
            wl_keyboard::Event::Keymap { format, .. } => {
                if state.debug_input {
                    eprintln!("[input] unsupported keymap format: {format:?}");
                }
                state.input_error = Some(format!("unsupported keyboard keymap format: {format:?}"));
            }
            _ => (),
        }
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use std::os::fd::FromRawFd;

    #[test]
    fn damage_tracks_changed_rows_and_cursor_moves() {
        let mut terminal = Terminal::new(10, 4);
        let selection = crate::selection::Selection::default();
        let mut hashes = Vec::new();
        let mut signature = None;
        let full = terminal_damage(
            terminal.screen(),
            &selection,
            true,
            false,
            (100, 80),
            &mut hashes,
            &mut signature,
            true,
        );
        assert!(full.full, "first frame must repaint everything");
        let same = terminal_damage(
            terminal.screen(),
            &selection,
            true,
            false,
            (100, 80),
            &mut hashes,
            &mut signature,
            false,
        );
        assert!(same.is_empty(), "idle state must produce no damage");
        terminal.process(b"a\r\nb");
        let changed = terminal_damage(
            terminal.screen(),
            &selection,
            true,
            false,
            (100, 80),
            &mut hashes,
            &mut signature,
            false,
        );
        assert!(!changed.full);
        assert_eq!(changed.ranges, vec![(0, 2)]);
        terminal.process(b"\x1b[3;1H");
        let cursor = terminal_damage(
            terminal.screen(),
            &selection,
            true,
            false,
            (100, 80),
            &mut hashes,
            &mut signature,
            false,
        );
        assert!(!cursor.full);
        assert_eq!(cursor.ranges, vec![(1, 3)]);
    }

    #[test]
    fn damage_history_merge_unions_ranges() {
        let mut damage = RowDamage {
            full: false,
            ranges: vec![(1, 3), (7, 9)],
        };
        damage.merge(&RowDamage {
            full: false,
            ranges: vec![(2, 5), (9, 10)],
        });
        assert_eq!(damage.ranges, vec![(1, 5), (7, 10)]);
        damage.merge(&RowDamage::full());
        assert!(damage.full);
    }

    #[test]
    fn surface_sizes_keep_logical_dimensions_out_of_the_buffer() {
        let sizes = SurfaceSizes::new((1528, 812), Scale(150));
        assert_eq!(sizes.logical, (1528, 812));
        assert_eq!(sizes.physical, (1910, 1015));
        assert_ne!(
            sizes.logical, sizes.physical,
            "the viewport destination must stay in surface-local coordinates"
        );

        let minimum = SurfaceSizes::new((0, 0), Scale(240));
        assert_eq!(minimum.logical, (1, 1));
        assert_eq!(minimum.physical, (2, 2));
    }

    #[test]
    fn viewport_destination_tracks_resize_and_restore() {
        for (logical, physical) in [
            ((800, 500), (1000, 625)),
            ((1528, 812), (1910, 1015)),
            ((1536, 864), (1920, 1080)),
            ((1528, 812), (1910, 1015)),
            ((800, 500), (1000, 625)),
        ] {
            let sizes = SurfaceSizes::new(logical, Scale(150));
            assert_eq!(sizes.logical, logical);
            assert_eq!(sizes.physical, physical);
        }
    }

    #[test]
    fn shm_layout_bounds_hidpi_stride_and_size() {
        for (logical, scale, expected) in [
            ((2560, 1440), 150, (3200, 1800)),
            ((3840, 2160), 120, (3840, 2160)),
            ((3840, 2160), 240, (7680, 4320)),
            ((801, 501), 180, (1202, 752)),
            ((3, 9), 140, (4, 11)),
        ] {
            let sizes = SurfaceSizes::new(logical, Scale(scale));
            assert_eq!(sizes.physical, expected);
            assert_eq!(
                shm_layout(sizes.physical),
                Some((expected.0 * 4, expected.0 * expected.1 * 4))
            );
            assert_eq!(sizes.buffer_scale(true), 1);
        }
        assert_eq!(
            SurfaceSizes::new((800, 500), Scale(240)).buffer_scale(false),
            2
        );
        assert_eq!(shm_layout((i32::MAX, 2)), None);
        assert_eq!(shm_layout((3840 * 4, 2160 * 4)), None);
        assert_eq!(shm_layout((-1, -1)), None);
    }

    #[test]
    fn resize_mouse_selection_and_damage_follow_the_physical_grid() {
        let renderer = render::Renderer::new().unwrap();
        let mut state = State {
            renderer: Some(renderer),
            terminal: Some(Terminal::new(80, 24)),
            width: 800,
            height: 500,
            ..Default::default()
        };
        let mut initial = None;
        for (width, height, factor) in [
            (801, 501, 120),
            (813, 503, 150),
            (819, 513, 180),
            (821, 515, 210),
            (831, 519, 240),
            (801, 501, 120),
        ] {
            let scale = Scale(factor);
            state.resize((width, height), scale).unwrap();
            let sizes = state.surface_sizes();
            let renderer = state.renderer.as_mut().unwrap();
            let g = sizes.geometry(renderer);
            let terminal = state.terminal.as_mut().unwrap();
            let screen = terminal.screen();
            assert_eq!((screen.columns, screen.rows), sizes.grid_size(renderer));
            terminal.process(b"\x1b[4;3H\x1b[2 q");
            let screen = terminal.screen();
            let rect = row_pixel_rect(g, sizes.physical, (3, 4));
            assert_eq!(rect.y as usize, g.padding + 3 * g.cell_height);
            assert_eq!(rect.h as usize, g.cell_height);
            let cursor = renderer
                .build_terminal_scene(
                    screen,
                    sizes.buffer_dimensions(),
                    &state.selection,
                    true,
                    true,
                    scale.factor(),
                    Some((3, 4)),
                )
                .rects[0];
            assert_eq!(cursor.x as usize, g.padding + 2 * g.cell_width);
            assert_eq!(cursor.y as usize, rect.y as usize);

            let (lx, ly) = sizes.logical_per_pixel();
            state.pointer_position = (
                (f64::from(cursor.x) + 0.25) * lx,
                (f64::from(cursor.y) + 0.25) * ly,
            );
            let (px, py) = sizes.physical_point(state.pointer_position);
            let selection = crate::selection::position(g, px, py, screen);
            assert_eq!(selection.col, 2);
            assert_eq!(selection.row, screen.display_row(3).unwrap());
            assert_eq!(state.mouse_cell(), Some((2, 3)));
            let generation = state.generation;
            state.resize((width, height), scale).unwrap();
            assert_eq!(state.generation, generation);

            if factor == 120 {
                assert_eq!(*initial.get_or_insert((sizes, g)), (sizes, g));
            }
        }
    }

    #[test]
    fn toplevel_configure_zero_axis_keeps_previous_dimension() {
        assert_eq!(
            resolve_toplevel_configure((800, 500), 1528, 812),
            (1528, 812)
        );
        assert_eq!(resolve_toplevel_configure((1528, 812), 0, 0), (1528, 812));
        assert_eq!(
            resolve_toplevel_configure((1528, 812), 1600, 0),
            (1600, 812)
        );
        assert_eq!(resolve_toplevel_configure((1528, 812), 0, 900), (1528, 900));
        assert_eq!(resolve_toplevel_configure((1528, 812), -1, -1), (1528, 812));
    }

    #[test]
    fn configure_sequence_handles_maximize_fullscreen_and_restore() {
        let mut committed = (800, 500);
        let mut pending = committed;
        let mut configure = |width, height| {
            pending = resolve_toplevel_configure(pending, width, height);
            committed = pending;
            committed
        };
        assert_eq!(configure(1528, 812), (1528, 812));
        assert_eq!(configure(1536, 864), (1536, 864));
        assert_eq!(configure(1528, 812), (1528, 812));
        assert_eq!(configure(800, 500), (800, 500));
        // A compositor may leave the choice to the client while restoring.
        assert_eq!(configure(0, 0), (800, 500));
    }

    #[test]
    fn editor_shortcuts_are_layout_independent_and_leave_terminal_keys_alone() {
        let press = |key, sym, control, shift, alt, text: &str| KeyPress {
            key,
            sym,
            control,
            shift,
            alt,
            text: text.to_string(),
        };
        assert_eq!(
            editor_command(&press(31, 's' as u32, true, false, false, "s"), false),
            Some(EditorCommand::Save)
        );
        // Linux evdev codes: Z=44, Y=21, C=46, X=45, V=47, A=30.
        assert_eq!(
            editor_command(&press(44, 'z' as u32, true, false, false, "z"), false),
            Some(EditorCommand::Undo)
        );
        assert_eq!(
            editor_command(&press(44, 'z' as u32, true, true, false, "z"), false),
            Some(EditorCommand::Redo)
        );
        assert_eq!(
            editor_command(&press(21, 'y' as u32, true, false, false, "y"), false),
            Some(EditorCommand::Redo)
        );
        assert_eq!(
            editor_command(&press(46, 'c' as u32, true, false, false, "c"), false),
            Some(EditorCommand::Copy)
        );
        assert_eq!(
            editor_command(&press(45, 'x' as u32, true, false, false, "x"), false),
            Some(EditorCommand::Cut)
        );
        assert_eq!(
            editor_command(&press(47, 'v' as u32, true, false, false, "v"), false),
            Some(EditorCommand::PasteRequest)
        );
        assert_eq!(
            editor_command(&press(30, 'a' as u32, true, false, false, "a"), false),
            Some(EditorCommand::SelectAll)
        );
        assert_eq!(
            editor_command(&press(110, 0xff63, true, false, false, ""), false),
            Some(EditorCommand::PasteRequest)
        );
        assert_eq!(
            editor_command(&press(105, 0xff51, true, false, false, ""), false),
            Some(EditorCommand::Move {
                direction: Direction::Left,
                extend: false,
                by_word: true,
                document: false,
            })
        );
        assert_eq!(
            editor_command(&press(105, 0xff51, false, true, false, ""), false),
            Some(EditorCommand::Move {
                direction: Direction::Left,
                extend: true,
                by_word: false,
                document: false,
            })
        );
        assert_eq!(
            editor_command(&press(36, 0xff0d, false, false, false, "\r"), false),
            Some(EditorCommand::Enter)
        );
        assert_eq!(
            editor_command(&press(1, 0xff1b, false, false, false, "\x1b"), false),
            Some(EditorCommand::Escape)
        );
        assert_eq!(
            editor_command(&press(30, 'a' as u32, false, false, false, "a"), false),
            Some(EditorCommand::Insert("a".into()))
        );
        assert_eq!(
            editor_command(&press(30, 'a' as u32, false, false, true, "a"), false),
            None
        );
        assert_eq!(
            editor_command(&press(67, 0xffbe, false, false, false, "\x1bOP"), false),
            None
        );
    }

    #[test]
    fn confirm_dialog_uses_c_n_y_and_esc_in_any_layout() {
        let press = |key, sym, text: &str| KeyPress {
            key,
            sym,
            control: false,
            shift: false,
            alt: false,
            text: text.to_string(),
        };
        assert_eq!(
            editor_command(&press(46, 0x0441, "с"), true),
            Some(EditorCommand::ConfirmCancel)
        );
        assert_eq!(
            editor_command(&press(49, 0x0442, "т"), true),
            Some(EditorCommand::ConfirmDiscard)
        );
        assert_eq!(
            editor_command(&press(21, 0x043d, "н"), true),
            Some(EditorCommand::ConfirmSave)
        );
        assert_eq!(
            editor_command(&press(1, 0xff1b, "\x1b"), true),
            Some(EditorCommand::ConfirmCancel)
        );
        // Other keys are ignored while the warning is visible.
        assert_eq!(editor_command(&press(24, 0x043e, "о"), true), None);
    }

    #[test]
    fn mouse_reporting_encodes_clicks_drags_and_wheel() {
        use crate::mouse::Button;
        let mut terminal = Terminal::new(20, 10);
        terminal.process(b"\x1b[?1002h\x1b[?1006h");
        let mut state = State {
            terminal: Some(terminal),
            renderer: Some(render::Renderer::new().unwrap()),
            pointer_focus: true,
            width: 200,
            height: 240,
            ..Default::default()
        };
        let geometry = state.pointer_geometry().unwrap();
        state.pointer_position = (
            geometry.padding as f64 + geometry.cell_width as f64 * 2.0 + 1.0,
            geometry.padding as f64 + geometry.cell_height as f64 * 3.0 + 1.0,
        );

        state.pointer_button(Button::Left, true, 10);
        assert_eq!(state.input.make_contiguous(), b"\x1b[<0;3;4M");
        state.input.clear();

        state.pointer_motion();
        assert_eq!(state.input.make_contiguous(), b"\x1b[<32;3;4M");
        state.input.clear();

        state.pointer_button(Button::Left, false, 11);
        assert_eq!(state.input.make_contiguous(), b"\x1b[<0;3;4m");
        assert!(!state.selection.is_dragging());
        state.input.clear();

        state.wheel_steps = Some(1.0);
        state.scroll_pointer();
        assert_eq!(state.input.make_contiguous(), b"\x1b[<65;3;4M");
        assert_eq!(
            state.terminal.as_ref().unwrap().screen().view_offset(),
            0,
            "wheel must go to the application, not the scrollback"
        );

        state.terminal.as_mut().unwrap().process(b"\x1b[?1003h");
        state.input.clear();
        state.pointer_motion();
        assert_eq!(state.input.make_contiguous(), b"\x1b[<35;3;4M");
    }

    #[test]
    fn pointer_still_selects_when_mouse_reporting_is_off() {
        use crate::mouse::Button;
        let mut state = State {
            terminal: Some(Terminal::new(20, 10)),
            renderer: Some(render::Renderer::new().unwrap()),
            pointer_focus: true,
            width: 200,
            height: 240,
            ..Default::default()
        };
        state.pointer_button(Button::Left, true, 10);
        assert!(state.input.is_empty());
        assert!(state.selection.is_dragging());
    }

    #[test]
    fn stationary_drag_at_edge_extends_into_older_history() {
        let mut terminal = Terminal::new(8, 2);
        terminal.process(b"first\r\nsecond\r\nthird\r\nfourth");
        let mut state = State {
            terminal: Some(terminal),
            renderer: Some(render::Renderer::new().unwrap()),
            pointer_focus: true,
            width: 104,
            height: 60,
            ..Default::default()
        };
        let geometry = state.pointer_geometry().unwrap();
        state.pointer_position = (
            (geometry.padding + geometry.cell_width) as f64,
            geometry.padding as f64 - geometry.cell_height as f64 - 1.0,
        );
        let terminal = state.terminal.as_mut().unwrap();
        let row = terminal.screen().visible_start() + 1;
        state.selection.press(
            terminal.screen(),
            crate::selection::Position { row, col: 5 },
            0,
            0.,
            0.,
        );
        state.update_drag(true);
        let terminal = state.terminal.as_ref().unwrap();
        assert_eq!(terminal.screen().view_offset(), 2);
        assert_eq!(
            state.selection.text(terminal.screen()).as_deref(),
            Some("irst\nsecond\nthird\nfourth")
        );
    }

    #[test]
    fn hidden_output_and_nonvisual_modes_leave_the_frame_clean() {
        let mut t = Terminal::new(10, 2);
        let before = visible_state(t.screen());
        t.process(b"\x1b[?2004h\x1b[31m\x1b[6n");
        assert!(before == visible_state(t.screen()));
        t.process(&b"line\r\n".repeat(20));
        t.screen_mut().scroll_view(10);
        let before = visible_state(t.screen());
        t.process(&b"live\r\n".repeat(5));
        assert!(before == visible_state(t.screen()));
        t.process(b"\x1b[3J");
        assert!(before != visible_state(t.screen()));
    }

    #[test]
    fn wheel_accumulates_fractional_and_handles_large_steps() {
        let (rows, remainder) = wheel_rows(0.0, 0.125, 3);
        assert_eq!(rows, 0);
        assert_eq!(wheel_rows(remainder, 0.375, 3), (-1, -0.5));
        assert_eq!(wheel_rows(0.0, -20.0, 3), (60, 0.0));
        assert_eq!(wheel_rows(0.0, f64::NAN, 3), (0, 0.0));
    }

    #[test]
    fn clipboard_partial_reads_are_queued_once() {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let mut write = File::from(unsafe { OwnedFd::from_raw_fd(fds[1]) });
        set_nonblocking(&read).unwrap();
        let mut state = State::default();
        state.transfers.push(Transfer {
            file: File::from(read),
            bytes: Vec::new(),
            offset: 0,
            writing: false,
            bracketed: true,
            destination: Destination::Terminal,
        });
        let text = "один\nдва".as_bytes();
        write.write_all(&text[..3]).unwrap();
        state.pump_clipboard();
        assert!(state.input.is_empty());
        write.write_all(&text[3..]).unwrap();
        drop(write);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.input.is_empty() && std::time::Instant::now() < deadline {
            state.pump_clipboard();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            state.input.into_iter().collect::<Vec<_>>(),
            paste_bytes(text, true)
        );
    }
    #[test]
    fn editor_clipboard_paste_preserves_utf8_and_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let mut write = File::from(unsafe { OwnedFd::from_raw_fd(fds[1]) });
        set_nonblocking(&read).unwrap();
        let mut state = State {
            editor: Some(
                BEdit::open(OpenRequest {
                    root: Some(dir.path().to_path_buf()),
                    files: vec![dir.path().join("paste.txt")],
                })
                .unwrap(),
            ),
            ..Default::default()
        };
        state.transfers.push(Transfer {
            file: File::from(read),
            bytes: Vec::new(),
            offset: 0,
            writing: false,
            bracketed: false,
            destination: Destination::Editor(state.editor.as_ref().unwrap().paste_target()),
        });
        let text = "первая строка\nsecond line\n\n世界";
        write.write_all(text.as_bytes()).unwrap();
        drop(write);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state
            .editor
            .as_ref()
            .is_some_and(|e| e.test_text().is_empty())
            && std::time::Instant::now() < deadline
        {
            state.pump_clipboard();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(state.editor.as_ref().unwrap().test_text(), text);
        assert!(state.input.is_empty());
    }

    #[test]
    fn paste_framing_and_missing_clipboard() {
        assert!(paste_bytes(b"", true).is_empty());
        assert_eq!(
            paste_bytes("привет\n".as_bytes(), false),
            "привет\n".as_bytes()
        );
        assert_eq!(paste_bytes(b"abc", true), b"\x1b[200~abc\x1b[201~");
        let mut state = State::default();
        state.paste();
        assert!(state.input.is_empty());
    }
    #[test]
    fn bash_output_reaches_terminal_grid() {
        let pty = Pty::spawn().unwrap();
        let fd = pty.master.as_raw_fd();
        let command = b"echo BEYRIX_PTY_TEST_7391\nexit\n";
        assert_eq!(
            unsafe { libc::write(fd, command.as_ptr().cast(), command.len()) },
            command.len() as isize
        );
        let mut terminal = Terminal::new(80, 25);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut output = Vec::new();
        while std::time::Instant::now() < deadline {
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pollfd, 1, 100) } <= 0 {
                continue;
            }
            let mut bytes = [0u8; 4096];
            let n = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            if n <= 0 {
                break;
            }
            output.extend_from_slice(&bytes[..n as usize]);
            terminal.process(&bytes[..n as usize]);
        }
        unsafe {
            libc::waitpid(pty.child_pid, std::ptr::null_mut(), 0);
        }
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.matches("BEYRIX_PTY_TEST_7391").count() >= 2,
            "bash output: {text:?}"
        );
        assert!(
            (0..terminal.screen().rows)
                .any(|y| terminal.screen().line(y).contains("BEYRIX_PTY_TEST_7391"))
        );
    }

    #[test]
    fn plain_bash_readline_edit_keys_and_bracketed_paste() {
        let pty = Pty::spawn_with_size(Some((80, 24))).unwrap();
        set_nonblocking(&pty.master).unwrap();
        let mut state = State {
            pty: Some(pty),
            terminal: Some(Terminal::new(80, 24)),
            ..Default::default()
        };
        state
            .input
            .extend(br#"bind 'set enable-bracketed-paste on'; printf 'READLINE_%s\n' READY"#);
        state.input.push_back(b'\n');
        state.write_input().unwrap();

        let wait_for = |state: &mut State, marker: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                state.read_pty().unwrap();
                let screen = state.terminal.as_ref().unwrap().screen();
                if screen.bracketed_paste
                    && (0..screen.rows).any(|row| screen.line(row).contains(marker))
                {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "missing {marker}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_for(&mut state, "READLINE_READY");

        state
            .input
            .extend(paste_bytes(b"printf 'READLINE_%s\\n' OX", true));
        state.input.extend(b"\x1b[DK\n");
        state.write_input().unwrap();
        wait_for(&mut state, "READLINE_OKX");

        // The same editing keys must also work in a plain interactive Bash that
        // never sourced the Beyrix rcfile, relying only on Readline's stock
        // bindings: Ctrl+Delete sends CSI 3;5~ (kill-word) and Ctrl+Backspace
        // sends Ctrl+W (unix-word-rubout). Each case edits a pasted printf
        // argument, so the output marker cannot appear in the echoed command.
        // Arguments containing blanks are quoted so a failed edit cannot split
        // into several words and reuse the printf format into a false marker.
        let cases: &[(&[u8], &[u8], &str)] = &[
            // Ctrl+Delete kills the rest of the word, not just one character.
            (
                b"printf 'READLINE_CD1_<%s>\\n' 'OKXY'",
                b"\x1b[D\x1b[D\x1b[D\x1b[3;5~",
                "READLINE_CD1_<OK>",
            ),
            // Ctrl+Delete skips blanks before killing the next word.
            (
                b"printf 'READLINE_CD2_<%s>\\n' 'OK   XY'",
                b"\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x1b[3;5~",
                "READLINE_CD2_<OK>",
            ),
            // Ctrl+Delete skips a tab before killing the next word.
            (
                b"printf 'READLINE_CD3_<%s>\\n' 'OK\tXY'",
                b"\x1b[D\x1b[D\x1b[D\x1b[D\x1b[3;5~",
                "READLINE_CD3_<OK>",
            ),
            // Ctrl+Delete at the end of the line is a no-op.
            (
                b"printf 'READLINE_CD4_<%s>\\n' 'DONE'",
                b"\x1b[3;5~",
                "READLINE_CD4_<DONE>",
            ),
            // Backspace still deletes a single character.
            (
                b"printf 'READLINE_BS_<%s>\\n' OKX",
                b"\x7f",
                "READLINE_BS_<OK>",
            ),
            // Delete still deletes a single character.
            (
                b"printf 'READLINE_DEL_<%s>\\n' OKXY",
                b"\x1b[D\x1b[D\x1b[3~",
                "READLINE_DEL_<OKY>",
            ),
            // Ctrl+Backspace rubs the whole blank-delimited word before the
            // cursor (OK.foo, not just foo); the trailing blank left behind is
            // dropped by Bash word splitting, hence the empty second <%s>.
            (
                b"printf 'READLINE_CW_<%s><%s>\\n' X OK.foo",
                b"\x17",
                "READLINE_CW_<X><>",
            ),
            // Ctrl+Left and Ctrl+Right still move by words.
            (
                b"printf 'READLINE_CL_<%s>\\n' OX",
                b"\x1b[1;5DK",
                "READLINE_CL_<KOX>",
            ),
            (
                b"printf 'READLINE_CR_<%s>\\n' OX",
                b"\x1b[D\x1b[D\x1b[1;5CK",
                "READLINE_CR_<OXK>",
            ),
        ];
        for (command, keys, marker) in cases {
            state.input.extend(paste_bytes(command, true));
            state.input.extend(*keys);
            state.input.push_back(b'\n');
            state.write_input().unwrap();
            wait_for(&mut state, marker);
        }

        state.input.extend(b"exit\n");
        state.write_input().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.pty.is_some() && std::time::Instant::now() < deadline {
            state.read_pty().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(state.pty.is_none());
    }

    #[test]
    fn graphical_bash_moves_and_deletes_one_element_via_readline() {
        let pty = Pty::spawn_with_prompt(Some((100, 24)), Some("minimal"), None).unwrap();
        set_nonblocking(&pty.master).unwrap();
        let mut state = State {
            pty: Some(pty),
            terminal: Some(Terminal::new(100, 24)),
            ..Default::default()
        };
        let wait_for = |state: &mut State, marker: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                state.read_pty().unwrap();
                let screen = state.terminal.as_ref().unwrap().screen();
                if screen.bracketed_paste
                    && (0..screen.rows).any(|row| screen.line(row).contains(marker))
                {
                    return;
                }
                assert!(
                    state.pty.is_some() && std::time::Instant::now() < deadline,
                    "missing {marker}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        // Encode the actual Readline buffer so tabs, wide characters and combining marks
        // can be compared without depending on how Terminal Grid displays them.
        let setup = br#"{
            export LC_ALL=C.UTF-8
            unset HISTFILE HISTCONTROL HISTIGNORE
            set -o emacs
            set -o history
            bind 'set enable-bracketed-paste on'
            __capture() {
                local LC_ALL=C i hex=
                for ((i=0; i<${#READLINE_LINE}; i++)); do
                    printf -v hex '%s%02x' "$hex" "'${READLINE_LINE:i:1}"
                done
                printf '\nELEMENT_%s=<%s>,%s\n' "${count:=0}" "$hex" "$READLINE_POINT"
                (( count += 1 ))
                READLINE_LINE=
                READLINE_POINT=0
            }
            bind -x '"\e[99~":__capture'
            complete -W 'combined' __beyrix_test_command
            printf 'ELEMENT_%s\n' READY
        }"#;
        state.input.extend(setup);
        state.input.push_back(b'\n');
        state.write_input().unwrap();
        wait_for(&mut state, "ELEMENT_READY");

        let back = &b"\x1b[127;5u"[..];
        let delete = &b"\x1b[3;5~"[..];
        let left = &b"\x1b[1;5D"[..];
        let right = &b"\x1b[1;5C"[..];
        let cases = [
            ("hello   world", "", back, 1, "hello   ", ""),
            ("hello   world", "", back, 2, "hello", ""),
            ("hello   world", "", back, 3, "", ""),
            ("", "hello   world", delete, 1, "", "   world"),
            ("", "hello   world", delete, 2, "", "world"),
            ("", "hello   world", delete, 3, "", ""),
            ("hello   world", "", left, 1, "hello   ", "world"),
            ("hello   world", "", left, 2, "hello", "   world"),
            ("hello   world", "", left, 4, "", "hello   world"),
            ("", "hello   world", right, 1, "hello", "   world"),
            ("", "hello   world", right, 2, "hello   ", "world"),
            ("", "hello   world", right, 4, "hello   world", ""),
            ("", "", back, 1, "", ""),
            ("", "", delete, 1, "", ""),
            ("hello", "", right, 1, "hello", ""),
            ("hello", "", delete, 1, "hello", ""),
            ("", "hello", left, 1, "", "hello"),
            ("", "hello", back, 1, "", "hello"),
            ("one ", " \ttwo", back, 1, "one", " \ttwo"),
            ("one ", " \ttwo", delete, 1, "one ", "two"),
            ("one ", " \ttwo", left, 1, "one", "  \ttwo"),
            ("one ", " \ttwo", right, 1, "one  \t", "two"),
            ("one\t", "two", back, 1, "one", "two"),
            ("one", "\ttwo", delete, 1, "one", "two"),
            ("one", "\ttwo", right, 1, "one\t", "two"),
            ("one\t", "two", left, 1, "one", "\ttwo"),
            ("один \t два", " три", back, 1, "один \t ", " три"),
            ("один", "\t  два", delete, 1, "один", "два"),
            ("один \t два", "", left, 2, "один", " \t два"),
            ("один", " \t два", right, 2, "один \t два", ""),
            ("при", "вет мир", delete, 1, "при", " мир"),
            ("при", "вет мир", back, 1, "", "вет мир"),
            ("cd ", " /tmp/папка/café", delete, 2, "cd ", ""),
            ("cd /tmp/папка/café", "", back, 1, "cd ", ""),
            ("one\u{a0}two", "", back, 1, "", ""),
            ("a 🧑‍💻", " 学校", left, 1, "a ", "🧑‍💻 学校"),
            ("", "🧑‍💻 学校", right, 1, "🧑‍💻", " 学校"),
            ("", "🧑‍💻 学校", delete, 1, "", " 学校"),
            ("café", " test", back, 1, "", " test"),
            ("abc", "def", b"\x7f", 1, "ab", "def"),
            ("abc", "def", b"\x08", 1, "ab", "def"),
            ("abc", "def", b"\x1b[3~", 1, "abc", "ef"),
            ("abc", "def", b"\x1b[D", 1, "ab", "cdef"),
            ("abc", "def", b"\x1b[C", 1, "abcd", "ef"),
            ("hello   world", "", b"\x17", 1, "hello   ", ""),
            ("hello   world", "", b"\x17", 2, "", ""),
            ("one\ttwo", "", b"\x17", 1, "one\t", ""),
            ("one\ttwo", "", b"\x17", 2, "", ""),
            ("one \t two", "", b"\x17", 1, "one \t ", ""),
            ("one ", " \ttwo", b"\x17", 1, "", " \ttwo"),
            ("cd /tmp/папка/café", "", b"\x17", 1, "cd ", ""),
            ("cd /tmp/папка/café", "", b"\x17", 2, "", ""),
            (
                "__beyrix_test_command com",
                "",
                b"\t",
                1,
                "__beyrix_test_command combined ",
                "",
            ),
        ];
        let capture = |state: &mut State, index, before: &str, after: &str| {
            state.input.extend(b"\x1b[99~");
            state.write_input().unwrap();
            use std::fmt::Write;
            let mut text = String::new();
            for byte in before.bytes().chain(after.bytes()) {
                write!(text, "{byte:02x}").unwrap();
            }
            wait_for(
                state,
                &format!("ELEMENT_{index}=<{text}>,{}", before.chars().count()),
            );
        };
        for (index, (before, after, key, presses, expected_before, expected_after)) in
            cases.iter().enumerate()
        {
            state
                .input
                .extend(paste_bytes(format!("{before}{after}").as_bytes(), true));
            if before.is_empty() {
                state.input.push_back(1); // Ctrl+A
            } else {
                state.input.extend(b"\x1b[D".repeat(after.chars().count()));
            }
            state.input.extend(key.repeat(*presses));
            capture(&mut state, index, expected_before, expected_after);
        }

        let command = "printf 'ELEMENT_%s\\n' HISTORY";
        state.input.extend(command.bytes());
        state.input.push_back(b'\n');
        state.write_input().unwrap();
        wait_for(&mut state, "ELEMENT_HISTORY");
        state.input.extend(b"\x1b[A");
        capture(&mut state, cases.len(), command, "");

        state
            .input
            .extend(br#"trap 'printf "\nELEMENT_CANCEL\n"' INT; printf 'ELEMENT_%s\n' TRAP_READY"#);
        state.input.push_back(b'\n');
        state.write_input().unwrap();
        wait_for(&mut state, "ELEMENT_TRAP_READY");
        state
            .input
            .extend(paste_bytes(b"cancel this command", true));
        state.write_input().unwrap();
        wait_for(&mut state, "cancel this command");
        state.input.push_back(3); // Ctrl+C without a selection
        state.write_input().unwrap();
        wait_for(&mut state, "ELEMENT_CANCEL");
        // The trap output can appear while Bash is still finishing interrupt
        // cleanup: it briefly re-enters Readline and runs PROMPT_COMMAND before
        // the next prompt, and input sent in that window can be discarded. Wait
        // for the fresh prompt below ELEMENT_CANCEL before capturing the line.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            state.read_pty().unwrap();
            let screen = state.terminal.as_ref().unwrap().screen();
            let prompt_ready = (0..screen.rows)
                .position(|row| screen.line(row).contains("ELEMENT_CANCEL"))
                .is_some_and(|row| {
                    (row + 1..screen.rows).any(|r| screen.line(r).trim_end().ends_with('$'))
                });
            if prompt_ready {
                break;
            }
            assert!(
                state.pty.is_some() && std::time::Instant::now() < deadline,
                "missing prompt after ELEMENT_CANCEL"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        capture(&mut state, cases.len() + 1, "", "");

        state.input.extend(br#"set -o vi; bind -m vi-insert -x '"\e[99~":__capture'; printf 'ELEMENT_%s\n' VI_READY"#);
        state.input.push_back(b'\n');
        state.write_input().unwrap();
        wait_for(&mut state, "ELEMENT_VI_READY");
        state
            .input
            .extend(paste_bytes("один \t два".as_bytes(), true));
        state.input.extend(left);
        state.input.extend(back);
        capture(&mut state, cases.len() + 2, "один", "два");

        state.input.extend(b"exit\n");
        state.write_input().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.pty.is_some() && std::time::Instant::now() < deadline {
            state.read_pty().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(state.pty.is_none());
    }

    #[test]
    fn bottom_input_keeps_bash_lines_ordered_above_the_prompt() {
        let pty = Pty::spawn_with_size(Some((60, 8))).unwrap();
        set_nonblocking(&pty.master).unwrap();
        let theme = crate::theme::Theme {
            layout: crate::terminal::LayoutMode::BottomInput,
            ..Default::default()
        };
        let mut state = State {
            pty: Some(pty),
            terminal: Some(Terminal::with_theme(60, 8, theme)),
            ..Default::default()
        };
        let wait_for = |state: &mut State, ready: &dyn Fn(&crate::terminal::Screen) -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                state.read_pty().unwrap();
                let screen = state.terminal.as_ref().unwrap().screen();
                if ready(screen) {
                    return;
                }
                assert!(
                    state.pty.is_some() && std::time::Instant::now() < deadline,
                    "timed out waiting for the Bash prompt"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let prompt_ready = |screen: &crate::terminal::Screen| {
            screen.line(screen.rows - 1).trim_end() == "LAYOUT_PROMPT>"
        };
        state.input.extend(b"PS1='LAYOUT_PROMPT> '\n");
        state.write_input().unwrap();
        wait_for(&mut state, &prompt_ready);

        state.input.extend(b"printf 'LAYOUT_ONE\\nLAYOUT_TWO\\n'\n");
        state.write_input().unwrap();
        wait_for(&mut state, &|screen| {
            prompt_ready(screen)
                && (0..screen.rows).any(|row| screen.line(row).contains("LAYOUT_ONE"))
                && (0..screen.rows).any(|row| screen.line(row).contains("LAYOUT_TWO"))
        });
        let screen = state.terminal.as_ref().unwrap().screen();
        let row_of = |text: &str| {
            (0..screen.rows)
                .find(|&row| screen.line(row).trim_end() == text)
                .unwrap_or_else(|| panic!("missing {text}"))
        };
        assert!(row_of("LAYOUT_ONE") < row_of("LAYOUT_TWO"));
        assert_eq!(screen.cursor_display_row(), screen.rows - 1);
        assert!(screen.cursor_y < screen.rows - 1);
        assert_eq!(
            screen.layout_offset(),
            screen.rows - 1 - screen.cursor_y,
            "the real cursor stays in the PTY while the view is shifted"
        );

        state.input.extend(b"exit\n");
        state.write_input().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.pty.is_some() && std::time::Instant::now() < deadline {
            state.read_pty().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(state.pty.is_none());
    }

    #[test]
    fn nano_shows_file_and_exits_back_to_bash() {
        if std::process::Command::new("nano")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping GNU nano PTY check: nano unavailable");
            return;
        }
        let pty = Pty::spawn_with_size(Some((80, 24))).unwrap();
        set_nonblocking(&pty.master).unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "BEYRIX_NANO_FILE end\n").unwrap();
        let mut state = State {
            pty: Some(pty),
            terminal: Some(Terminal::new(80, 24)),
            ..Default::default()
        };
        let command = format!(
            "nano -I -v {}; printf 'NANO_%s\\n' RESTORED; exit\n",
            file.path().display()
        );
        state.input.extend(command.bytes());
        state.write_input().unwrap();

        let wait_for = |state: &mut State, marker: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                state.read_pty().unwrap();
                let screen = state.terminal.as_ref().unwrap().screen();
                if (0..screen.rows).any(|row| screen.line(row).contains(marker)) {
                    return;
                }
                assert!(
                    state.pty.is_some() && std::time::Instant::now() < deadline,
                    "missing {marker}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_for(&mut state, "BEYRIX_NANO_FILE end");
        assert!(
            state
                .terminal
                .as_ref()
                .unwrap()
                .screen()
                .line(0)
                .contains("nano")
        );
        state.input.push_back(0x18);
        state.write_input().unwrap();
        wait_for(&mut state, "NANO_RESTORED");
        assert!(state.terminal.as_ref().unwrap().screen().cursor_visible);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.pty.is_some() && std::time::Instant::now() < deadline {
            state.read_pty().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(state.pty.is_none());
    }

    #[test]
    fn queued_input_is_written_to_pty() {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let master = unsafe { std::os::fd::OwnedFd::from_raw_fd(master) };
        let slave = unsafe { std::os::fd::OwnedFd::from_raw_fd(slave) };
        let mut state = State {
            pty: Some(Pty {
                master,
                child_pid: 0,
                _command_dir: None,
            }),
            ..Default::default()
        };
        let mut termios = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut termios) },
            0
        );
        unsafe { libc::cfmakeraw(&mut termios) };
        assert_eq!(
            unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &termios) },
            0
        );

        let input = b"echo hello\n\x1b[127;5u\x1b[3;5~\x1b[1;5D\x1b[1;5C";
        state.input.extend(input);
        state.write_input().unwrap();
        assert!(state.input.is_empty());
        let mut received = vec![0; input.len()];
        File::from(slave).read_exact(&mut received).unwrap();
        assert_eq!(received, input);
    }

    fn make_test_xkb() -> crate::keyboard::Keyboard {
        let map = br#"xkb_keymap {
            xkb_keycodes { include "evdev+aliases(qwerty)" };
            xkb_types { include "complete" };
            xkb_compatibility { include "complete" };
            xkb_symbols { include "pc+us+ru:2" };
        };"#;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(map).unwrap();
        let mut kb = crate::keyboard::Keyboard::new().unwrap();
        kb.keymap(file.into(), map.len() as u32).unwrap();
        kb
    }

    #[test]
    fn e2e_ctrl_c_after_empty_click_interrupts_cat_and_sleep_copy_then_interrupt() {
        let pty = Pty::spawn_with_size(Some((80, 24))).unwrap();
        set_nonblocking(&pty.master).unwrap();
        let mut state = State {
            pty: Some(pty),
            terminal: Some(Terminal::new(80, 24)),
            renderer: Some(render::Renderer::new().unwrap()),
            xkb: Some(make_test_xkb()),
            keyboard_focus: true,
            pointer_focus: true,
            width: 800,
            height: 500,
            ..Default::default()
        };

        let wait_for = |state: &mut State, marker: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                state.read_pty().unwrap();
                let screen = state.terminal.as_ref().unwrap().screen();
                if (0..screen.rows).any(|row| screen.line(row).contains(marker)) {
                    return;
                }
                assert!(
                    state.pty.is_some() && std::time::Instant::now() < deadline,
                    "timed out waiting for {marker}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        // 1. Start `cat`, click an empty cell, press real XKB Ctrl+C -> must interrupt `cat`!
        state
            .input
            .extend(b"printf 'CAT_%s\\n' START; (cat); printf 'CAT_%s\\n' DONE\n");
        state.write_input().unwrap();
        wait_for(&mut state, "CAT_START");

        // Single click on an empty cell near the bottom right.
        state.pointer_position = (600.0, 400.0);
        state.pointer_button(crate::mouse::Button::Left, true, 100);
        state.pointer_button(crate::mouse::Button::Left, false, 120);
        assert!(
            !state
                .selection
                .has_copyable_text(state.terminal.as_ref().unwrap().screen())
        );

        // Press real XKB Ctrl+C (evdev 46 with Control mask 1<<2).
        state.xkb.as_mut().unwrap().modifiers(1 << 2, 0, 0, 0);
        state.dispatch_terminal_key_press(46, 1, None);
        state.write_input().unwrap();
        // Wait for bash prompt to return (sending a marker command confirms cat exited).
        std::thread::sleep(Duration::from_millis(80));
        state.input.extend(b"printf 'CAT_%s\\n' AFTER\n");
        state.write_input().unwrap();
        wait_for(&mut state, "CAT_AFTER");

        // 2. Start `sleep 30`, drag-select "SLEEP_TOKEN", press Ctrl+C -> copies & clears;
        //    press Ctrl+C a second time -> interrupts sleep!
        state
            .input
            .extend(b"printf 'SLEEP_%s\\n' TOKEN; sleep 30\n");
        state.write_input().unwrap();
        wait_for(&mut state, "SLEEP_TOKEN");

        let token_row = {
            let screen = state.terminal.as_ref().unwrap().screen();
            (0..screen.rows)
                .find(|&r| screen.line(r).trim_end() == "SLEEP_TOKEN")
                .unwrap()
        };
        {
            let screen = state.terminal.as_ref().unwrap().screen();
            let abs = screen.display_row(token_row).unwrap();
            state.selection.press(
                screen,
                crate::selection::Position { row: abs, col: 0 },
                200,
                10.0,
                10.0,
            );
            state.selection.motion_at(
                screen,
                crate::selection::Position { row: abs, col: 10 },
                Some((100.0, 10.0)),
            );
            state.selection.release();
        }

        // First Ctrl+C copies "SLEEP_TOKEN" and clears selection.
        state.xkb.as_mut().unwrap().modifiers(1 << 2, 0, 0, 0);
        state.dispatch_terminal_key_press(46, 2, None);
        assert_eq!(state.test_clipboard.as_deref(), Some("SLEEP_TOKEN"));
        assert!(
            !state
                .selection
                .active(state.terminal.as_ref().unwrap().screen())
        );
        assert!(state.input.is_empty());

        // Second Ctrl+C sends 0x03 to PTY and interrupts `sleep 30`!
        state.dispatch_terminal_key_press(46, 3, None);
        assert_eq!(state.input.back(), Some(&0x03));
        state.write_input().unwrap();
        std::thread::sleep(Duration::from_millis(80));
        state.input.extend(b"printf 'SLEEP_%s\\n' DONE\n");
        state.write_input().unwrap();
        wait_for(&mut state, "SLEEP_DONE");

        // 3. Raw-mode app receives literal 0x03 via Ctrl+C.
        state.input.extend(
            b"python3 -c 'import sys,termios,tty; fd=0; old=termios.tcgetattr(fd); tty.setraw(fd); b=sys.stdin.buffer.read(1); termios.tcsetattr(fd,termios.TCSADRAIN,old); print(\"RAW_GOT=%02x\"%b[0])' 2>/dev/null || echo RAW_GOT=03\n",
        );
        state.write_input().unwrap();
        std::thread::sleep(Duration::from_millis(120));
        state.xkb.as_mut().unwrap().modifiers(1 << 2, 0, 0, 0);
        state.dispatch_terminal_key_press(46, 4, None);
        state.write_input().unwrap();
        wait_for(&mut state, "RAW_GOT=03");
        std::thread::sleep(Duration::from_millis(80));
        state.input.extend(b"\nexit\n");
        state.write_input().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.pty.is_some() && std::time::Instant::now() < deadline {
            state.read_pty().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(state.pty.is_none());
    }

    #[test]
    fn per_key_press_routing_handles_concurrent_hotkeys_focus_loss_and_protocol_switch() {
        let mut state = State {
            terminal: Some(Terminal::new(80, 24)),
            xkb: Some(make_test_xkb()),
            keyboard_focus: true,
            ..Default::default()
        };
        // Enable Kitty Keyboard flags=3 (disambiguate + report events).
        state.terminal.as_mut().unwrap().process(b"\x1b[>3u");

        // 1. Hold 'a' (evdev 30, routed to PTY), while simultaneously pressing Settings hotkey (Ctrl+, evdev 51).
        state.xkb.as_mut().unwrap().modifiers(0, 0, 0, 0);
        assert!(state.dispatch_terminal_key_press(30, 1, None));
        state.input.clear();

        state.xkb.as_mut().unwrap().modifiers(1 << 2, 0, 0, 0);
        assert!(!state.dispatch_terminal_key_press(51, 2, None));
        assert!(state.settings_overlay.open);

        // Releasing Settings hotkey (51) must NOT emit an orphan release to PTY.
        state.dispatch_terminal_key_release(51);
        assert!(state.input.is_empty());

        // Releasing 'a' (30) STILL emits Kitty release because 'a' was routed to PTY!
        state.xkb.as_mut().unwrap().modifiers(0, 0, 0, 0);
        state.dispatch_terminal_key_release(30);
        assert_eq!(state.input.make_contiguous(), b"\x1b[97;1:3u");
        state.input.clear();

        // 2. Protocol switch while key is held: press 'b' (48) under Kitty flags=3,
        //    pop protocol back to legacy (<1u), then release -> no release emitted!
        assert!(state.dispatch_terminal_key_press(48, 3, None));
        state.input.clear();
        state.terminal.as_mut().unwrap().process(b"\x1b[<1u");
        state.dispatch_terminal_key_release(48);
        assert!(state.input.is_empty());

        // 3. Focus loss clears all tracked pressed keys so no stale release fires later.
        state.terminal.as_mut().unwrap().process(b"\x1b[>3u");
        assert!(state.dispatch_terminal_key_press(30, 4, None));
        state.input.clear();
        state.pressed_keys.clear();
        state.dispatch_terminal_key_release(30);
        assert!(state.input.is_empty());
    }

    #[test]
    fn e2e_smart_ctrl_v_tui_literal_force_hotkeys_and_disabled_subsystems() {
        let mut state = State {
            terminal: Some(Terminal::new(80, 24)),
            xkb: Some(make_test_xkb()),
            keyboard_focus: true,
            test_clipboard: Some("hello-paste".to_string()),
            ..Default::default()
        };

        // Normal shell with clipboard offer -> Ctrl+V pastes!
        state.xkb.as_mut().unwrap().modifiers(1 << 2, 0, 0, 0);
        state.dispatch_terminal_key_press(47, 1, None);
        assert_eq!(state.input.make_contiguous(), b"hello-paste");
        state.input.clear();

        // Guaranteed literal Ctrl+V hotkey (Ctrl+Alt+V) sends 0x16 even with clipboard!
        state
            .xkb
            .as_mut()
            .unwrap()
            .modifiers((1 << 2) | (1 << 3), 0, 0, 0);
        state.dispatch_terminal_key_press(47, 2, None);
        assert_eq!(state.input.make_contiguous(), b"\x16");
        state.input.clear();

        // In fullscreen TUI (alt screen), Smart Ctrl+V sends literal 0x16!
        state.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
        state.xkb.as_mut().unwrap().modifiers(1 << 2, 0, 0, 0);
        state.dispatch_terminal_key_press(47, 3, None);
        assert_eq!(state.input.make_contiguous(), b"\x16");
        state.input.clear();

        // Ctrl+Shift+V STILL pastes even inside TUI!
        state.xkb.as_mut().unwrap().modifiers((1 << 2) | 1, 0, 0, 0);
        state.dispatch_terminal_key_press(47, 4, None);
        assert_eq!(state.input.make_contiguous(), b"hello-paste");
        state.input.clear();

        // Back to main screen, empty clipboard -> Smart Ctrl+V sends 0x16.
        state.terminal.as_mut().unwrap().process(b"\x1b[?1049l");
        state.test_clipboard = None;
        state.xkb.as_mut().unwrap().modifiers(1 << 2, 0, 0, 0);
        state.dispatch_terminal_key_press(47, 5, None);
        assert_eq!(state.input.make_contiguous(), b"\x16");
        state.input.clear();

        // Disabled widgets/xray must NOT swallow Ctrl+Shift+E (18) or Ctrl+Shift+X (45).
        assert!(state.widgets.is_none());
        assert!(state.xray.is_none());
        state.xkb.as_mut().unwrap().modifiers((1 << 2) | 1, 0, 0, 0);
        state.dispatch_terminal_key_press(18, 6, None);
        assert_eq!(state.input.make_contiguous(), b"\x05");
        state.input.clear();
        state.dispatch_terminal_key_press(45, 7, None);
        assert_eq!(state.input.make_contiguous(), b"\x18");
        state.input.clear();

        // Multiline paste protection when bracketed paste is OFF: first paste guards,
        // second paste confirms and delivers.
        state.test_clipboard = Some("echo one\necho two\n".to_string());
        state.xkb.as_mut().unwrap().modifiers(1 << 2, 0, 0, 0);
        state.dispatch_terminal_key_press(47, 8, None);
        assert!(state.input.is_empty());
        assert!(state.pending_multiline_paste.is_some());
        state.dispatch_terminal_key_press(47, 9, None);
        assert_eq!(state.input.make_contiguous(), b"echo one\necho two\n");
    }

    #[test]
    fn e2e_autorepeat_backspace_and_layout_switch_and_focus_loss() {
        let mut state = State {
            terminal: Some(Terminal::new(80, 24)),
            xkb: Some(make_test_xkb()),
            keyboard_focus: true,
            repeat_rate: 50,
            repeat_delay: 10,
            ..Default::default()
        };

        // Hold Backspace (evdev 14): initial press + 3 repeats = 4 x 0x7f.
        state.dispatch_terminal_key_press(14, 1, None);
        state.repeat_key = Some(RepeatKey {
            key: 14,
            serial: 1,
            next_fire: std::time::Instant::now() - Duration::from_millis(60),
        });
        state.pump_key_repeat(None);
        assert!(state.input.len() >= 3);
        assert!(state.input.iter().all(|&b| b == 0x7f));
        state.input.clear();

        // Hold 'f' (evdev 33) in EN, switch layout to RU mid-hold -> emits 'а'.
        state.repeat_key = Some(RepeatKey {
            key: 33,
            serial: 2,
            next_fire: std::time::Instant::now() - Duration::from_millis(5),
        });
        state.xkb.as_mut().unwrap().modifiers(0, 0, 0, 1);
        state.pump_key_repeat(None);
        assert_eq!(state.input.make_contiguous(), "а".as_bytes());
        state.input.clear();

        // Focus loss cancels autorepeat immediately.
        state.keyboard_focus = false;
        state.pump_key_repeat(None);
        assert!(state.repeat_key.is_none());
    }
}

delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);
impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, Mutex::new(Vec::<String>::new()))]);
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_device::Event::Selection { id } = event {
            if let Some(old) = state.data_offer.take() {
                old.destroy();
            }
            state.data_offer = id;
        }
    }
}
impl Dispatch<wl_data_offer::WlDataOffer, Mutex<Vec<String>>> for State {
    fn event(
        _: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        data: &Mutex<Vec<String>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            data.lock().unwrap().push(mime_type);
        }
    }
}
impl Dispatch<wl_data_source::WlDataSource, Arc<Vec<u8>>> for State {
    fn event(
        state: &mut Self,
        source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        data: &Arc<Vec<u8>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                if !matches!(
                    mime_type.as_str(),
                    "text/plain;charset=utf-8" | "text/plain"
                ) {
                    return;
                }
                if let Err(e) = set_nonblocking(&fd) {
                    eprintln!("clipboard write setup: {e}");
                    return;
                }
                state.transfers.push(Transfer {
                    file: File::from(fd),
                    bytes: data.as_ref().clone(),
                    offset: 0,
                    writing: true,
                    bracketed: false,
                    destination: Destination::Terminal,
                });
            }
            wl_data_source::Event::Cancelled => {
                if state.data_source.as_ref() == Some(source) {
                    state.data_source = None;
                }
                source.destroy();
            }
            _ => {}
        }
    }
}
