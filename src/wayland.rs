// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::editor::{Command as EditorCommand, Direction, Editor, Metrics};
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
    Editor,
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

fn metrics(geometry: render::Geometry) -> Metrics {
    Metrics {
        cell_width: geometry.cell_width,
        cell_height: geometry.cell_height,
        padding: geometry.padding,
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
        [b"\x1b[200~".as_slice(), bytes, b"\x1b[201~".as_slice()].concat()
    } else {
        bytes.to_vec()
    }
}
fn wheel_rows(remainder: f64, steps: f64, speed: usize) -> (isize, f64) {
    let total = remainder - steps * speed as f64;
    if !total.is_finite() {
        return (0, 0.0);
    }
    (total as isize, total.fract())
}

struct FrameBuffer {
    buffer: wl_buffer::WlBuffer,
    file: File,
    busy: bool,
    id: u64,
    generation: u64,
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
    blink_epoch: Option<std::time::Instant>,
    blink_on: bool,
    pointer: Option<wl_pointer::WlPointer>,
    pointer_focus: bool,
    pointer_position: (f64, f64),
    wheel_axis: f64,
    wheel_steps: Option<f64>,
    wheel_remainder: f64,
    wheel_mouse_remainder: f64,
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
    pixels: Vec<u8>,
    terminal: Option<Terminal>,
    editor: Option<Editor>,
    renderer: Option<render::Renderer>,
    backend: ActiveBackend,
    gl: Option<GlBackend>,
    scale: f32,
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
}

pub fn run() -> Result<(), Box<dyn Error>> {
    run_with(None)
}

pub fn run_editor(path: std::path::PathBuf) -> Result<(), Box<dyn Error>> {
    run_with(Some(path))
}

fn run_with(editor_path: Option<std::path::PathBuf>) -> Result<(), Box<dyn Error>> {
    let connection =
        Connection::connect_to_env().map_err(|e| format!("connect to Wayland: {e}"))?;
    let mut event_queue = connection.new_event_queue();
    let qh = event_queue.handle();

    let display = connection.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = State {
        running: true,
        control: Some(crate::control::Control::new().map_err(|e| format!("control socket: {e}"))?),
        blink_epoch: Some(std::time::Instant::now()),
        blink_on: true,
        width: WIDTH,
        height: HEIGHT,
        scale: 1.0,
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
    if let Some(path) = editor_path {
        state
            .open_editor(path)
            .map_err(|e| format!("open editor: {e}"))?;
    }
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
                    if let Err(e) = state.open_editor(path) {
                        eprintln!("open file: {e}");
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("control socket: {e}"),
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
        state.pump_clipboard();
        state.update_drag(true);
        if let Some((width, height)) = state.pending_size.take() {
            state
                .resize(width, height, &qh)
                .map_err(|e| format!("resize window: {e}"))?;
        }
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
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 50) };
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
                | (u64::from(cell.written) << 2),
        );
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

fn physical_size(logical: (i32, i32), scale: f32) -> (i32, i32) {
    (
        (logical.0 as f32 * scale).round().max(1.0) as i32,
        (logical.1 as f32 * scale).round().max(1.0) as i32,
    )
}

/// Sizes for one surface commit. `logical` is the size the compositor assigned
/// to the xdg surface; `physical` is the backing buffer size in pixels. The
/// viewport destination must always be logical, even when the buffer is scaled
/// for fractional output scaling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceSizes {
    logical: (i32, i32),
    physical: (i32, i32),
}

impl SurfaceSizes {
    fn new(logical: (i32, i32), scale: f32) -> Self {
        let logical = (logical.0.max(1), logical.1.max(1));
        Self {
            logical,
            physical: physical_size(logical, scale),
        }
    }

    fn viewport_destination(self) -> (i32, i32) {
        self.logical
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
    logical: (usize, usize),
    scale: f32,
    physical: (i32, i32),
    range: (usize, usize),
) -> PixelRect {
    let base = geometry.for_window(logical.0.max(1), logical.1.max(1));
    let cell_h = base.cell_height as f32 * scale;
    let padding = base.padding as f32 * scale;
    let top = (padding + range.0 as f32 * cell_h).floor() as i32;
    let bottom = ((padding + range.1 as f32 * cell_h).ceil() as i32).min(physical.1);
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

        toplevel.set_title("Beyrix".to_string());
        toplevel.set_app_id("beyrix".to_string());
        toplevel.set_min_size(320, 200);

        let theme = crate::theme::AppConfig::load()
            .map_err(|e| format!("load configuration: {e}"))?
            .theme;
        if let (Some(manager), Some(_)) = (&self.fractional_manager, &self.viewporter) {
            self.fractional_scale = Some(manager.get_fractional_scale(&surface, qh, ()));
        }
        if let Some(viewporter) = &self.viewporter {
            let viewport = viewporter.get_viewport(&surface, qh, ());
            let destination = SurfaceSizes::new((WIDTH, HEIGHT), self.scale).viewport_destination();
            viewport.set_destination(destination.0, destination.1);
            self.viewport = Some(viewport);
            if surface.version() >= 3 {
                surface.set_buffer_scale(1);
            }
        }
        let renderer =
            render::Renderer::with_theme(theme.clone()).map_err(|e| format!("load fonts: {e}"))?;
        let (columns, rows) = renderer.geometry.grid_size(WIDTH as usize, HEIGHT as usize);
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
        let terminal = Terminal::with_theme(columns, rows, theme);
        if self.backend == ActiveBackend::Software {
            self.create_buffers(WIDTH, HEIGHT, qh)?;
        }
        self.damage_full = true;

        // Начальный commit без буфера:
        // ждём configure от композитора.
        surface.commit();

        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
        self.terminal = Some(terminal);
        self.renderer = Some(renderer);
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
        self.pty = Some(pty);

        Ok(())
    }

    fn set_fractional_scale(&mut self, scale: u32, qh: &QueueHandle<Self>) {
        let scale = (scale as f32 / 120.0).clamp(0.5, 4.0);
        self.apply_scale(scale, qh);
    }

    fn set_preferred_scale(&mut self, factor: i32, qh: &QueueHandle<Self>) {
        if self.fractional_scale.is_some() {
            return;
        }
        let factor = factor.clamp(1, 4) as f32;
        self.apply_scale(factor, qh);
    }

    fn set_viewport_destination(&self, logical: (i32, i32)) {
        if let Some(viewport) = &self.viewport {
            // The destination is surface-local (logical) size. The backing
            // buffer may be larger for fractional scaling; it must never
            // become the viewport destination or the compositor will scale the
            // window back to the size used at startup.
            let destination = SurfaceSizes::new(logical, self.scale).viewport_destination();
            viewport.set_destination(destination.0, destination.1);
        }
    }

    fn apply_scale(&mut self, scale: f32, qh: &QueueHandle<Self>) {
        if !scale.is_finite() || (scale - self.scale).abs() < 0.001 {
            return;
        }
        self.scale = scale;
        if let Some(surface) = &self.surface
            && surface.version() >= 3
        {
            if self.viewport.is_some() {
                // With wp_viewport the buffer is scaled into the logical
                // destination, so the surface itself keeps buffer scale 1.
                surface.set_buffer_scale(1);
            } else if self.gl.is_some() {
                surface.set_buffer_scale(scale.round().clamp(1.0, 4.0) as i32);
            }
        }
        self.set_viewport_destination((self.width, self.height));
        if self.gl.is_some() {
            let sizes = SurfaceSizes::new((self.width, self.height), scale);
            let result = self
                .gl
                .as_mut()
                .map(|gl| gl.resize(sizes.physical.0, sizes.physical.1));
            if let Some(Err(error)) = result {
                eprintln!("beyrix: OpenGL resize failed ({error}); using software");
                self.gl = None;
                self.backend = ActiveBackend::Software;
                if let Err(error) = self.ensure_software_buffers(qh) {
                    eprintln!("beyrix: cannot create software buffers: {error}");
                    self.running = false;
                }
            }
        }
        self.damage_full = true;
        self.dirty = true;
    }

    fn ensure_software_buffers(&mut self, qh: &QueueHandle<Self>) -> Result<(), Box<dyn Error>> {
        if self
            .buffers
            .iter()
            .any(|buffer| buffer.generation == self.generation)
        {
            return Ok(());
        }
        self.create_buffers(self.width, self.height, qh)
    }

    fn fallback_to_software(
        &mut self,
        reason: String,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn Error>> {
        eprintln!("beyrix: OpenGL backend disabled ({reason}); using software");
        self.gl = None;
        self.backend = ActiveBackend::Software;
        self.damage_full = true;
        self.damage_hashes.clear();
        self.damage_signature = None;
        self.damage_history.clear();
        self.frame_pending = false;
        self.ensure_software_buffers(qh)?;
        self.dirty = true;
        Ok(())
    }

    fn open_editor(&mut self, path: std::path::PathBuf) -> Result<(), Box<dyn Error>> {
        if let Some(editor) = &self.editor
            && editor.is_modified()
        {
            let message = "Save or discard the current file before opening another one";
            if let Some(editor) = &mut self.editor {
                editor.set_message(message, true);
            }
            self.dirty = true;
            return Err(message.into());
        }
        let editor = Editor::open(path)?;
        if let Some(toplevel) = &self.toplevel {
            toplevel.set_title(format!("Beyrix - {}", editor.display_name()));
        }
        self.editor = Some(editor);
        self.selection.clear();
        self.damage_full = true;
        self.dirty = true;
        Ok(())
    }

    fn close_editor(&mut self) {
        self.editor = None;
        if let Some(toplevel) = &self.toplevel {
            toplevel.set_title("Beyrix".to_string());
        }
        self.damage_full = true;
        self.dirty = true;
    }

    fn apply_theme(&mut self, name: &str) -> Result<(), Box<dyn Error>> {
        let theme = crate::theme::Theme::load_for(name)?;
        let renderer = self.renderer.as_mut().ok_or("renderer unavailable")?;
        renderer.retheme(theme.clone())?;
        if let Some(gl) = &mut self.gl {
            gl.invalidate_atlas();
        }
        self.damage_full = true;
        let (cols, rows) = renderer
            .geometry
            .grid_size(self.width as usize, self.height as usize);
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
        self.dirty = true;
        Ok(())
    }

    fn create_buffers(
        &mut self,
        width: i32,
        height: i32,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn Error>> {
        let stride = width.checked_mul(4).ok_or("window width too large")?;
        let size = stride
            .checked_mul(height)
            .filter(|s| *s > 0)
            .ok_or("window dimensions too large")?;
        let shm = self.shm.as_ref().ok_or("wl_shm unavailable")?;
        let mut created = Vec::new();
        for _ in 0..2 {
            let file = tempfile::tempfile()?;
            file.set_len(size as u64)?;
            let pool = shm.create_pool(file.as_fd(), size, qh, ());
            let id = self.next_buffer_id;
            self.next_buffer_id += 1;
            let buffer =
                pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, qh, id);
            pool.destroy();
            created.push(FrameBuffer {
                buffer,
                file,
                busy: false,
                id,
                generation: self.generation,
            });
        }
        self.buffers.extend(created);
        Ok(())
    }

    fn resize(
        &mut self,
        width: i32,
        height: i32,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn Error>> {
        let sizes = SurfaceSizes::new((width, height), self.scale);
        let (width, height) = sizes.logical;
        if (width, height) == (self.width, self.height) {
            // Scale changes and the first configure can keep the same logical
            // size while the viewport is still on its startup destination.
            self.set_viewport_destination(sizes.logical);
            return Ok(());
        }
        let (columns, rows) = self
            .renderer
            .as_ref()
            .ok_or("renderer unavailable")?
            .geometry
            .grid_size(width as usize, height as usize);
        let mut gl_error = None;
        if let Some(gl) = &mut self.gl
            && let Err(error) = gl.resize(sizes.physical.0, sizes.physical.1)
        {
            gl_error = Some(error);
        }
        if let Some(error) = gl_error {
            self.fallback_to_software(error, qh)?;
        }
        if self.gl.is_none() {
            self.generation += 1;
            self.create_buffers(width, height, qh)?;
        }
        self.damage_full = true;
        if let Some(pty) = &self.pty {
            pty.resize(columns as u16, rows as u16)
                .map_err(|e| format!("TIOCSWINSZ: {e}"))?;
        }
        if let Some(terminal) = &mut self.terminal {
            terminal.resize(columns, rows);
        }
        if let Some(editor) = &mut self.editor {
            editor.follow_cursor();
        }
        self.width = width;
        self.height = height;
        self.set_viewport_destination(sizes.logical);
        self.buffers.retain(|frame| {
            if frame.generation != self.generation && !frame.busy {
                frame.buffer.destroy();
                false
            } else {
                true
            }
        });
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
        let renderer = self.renderer.as_ref()?;
        let screen = terminal.screen();
        let geometry = renderer
            .geometry
            .for_window(self.width as usize, self.height as usize);
        let (x, y) = self.pointer_position;
        let column = ((x - geometry.padding as f64) / geometry.cell_width as f64)
            .floor()
            .max(0.0) as usize;
        let display_row = ((y - geometry.padding as f64) / geometry.cell_height as f64)
            .floor()
            .max(0.0) as usize;
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
        let Some((mode, sgr)) = self.terminal.as_ref().map(|terminal| {
            (
                terminal.screen().mouse_mode(),
                terminal.screen().mouse_sgr(),
            )
        }) else {
            return false;
        };
        let Some((column, row)) = self.mouse_cell() else {
            return false;
        };
        let Some(bytes) = crate::mouse::encode(crate::mouse::Event {
            mode,
            sgr,
            button,
            column,
            row,
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
        if self.editor.is_some() {
            let geometry = self.renderer.as_ref().map(|renderer| {
                renderer
                    .geometry
                    .for_window(self.width as usize, self.height as usize)
            });
            let Some(editor) = &mut self.editor else {
                return;
            };
            if pressed {
                if button == crate::mouse::Button::Left
                    && let Some(geometry) = geometry
                {
                    let (x, y) = self.pointer_position;
                    editor.mouse_press(x, y, metrics(geometry), time);
                    self.dirty = true;
                }
            } else {
                editor.mouse_release();
                self.dirty = true;
            }
            return;
        }
        if pressed {
            if self.mouse_reporting() && self.report_mouse(Some(button), true, false) {
                self.mouse_button = Some(button);
                return;
            }
            if button == crate::mouse::Button::Left
                && let (Some(geometry), Some(terminal)) = (
                    self.renderer.as_ref().map(|renderer| {
                        renderer
                            .geometry
                            .for_window(self.width as usize, self.height as usize)
                    }),
                    self.terminal.as_ref(),
                )
            {
                let (x, y) = self.pointer_position;
                let position = crate::selection::position(geometry, x, y, terminal.screen());
                self.selection
                    .press(terminal.screen(), position, time, x, y);
                self.dirty = true;
            }
        } else if let Some(held) = self.mouse_button.take() {
            if self.mouse_mode() != crate::mouse::Mode::Off {
                self.report_mouse(Some(held), false, false);
            }
        } else if button == crate::mouse::Button::Left {
            self.selection.release();
        }
    }

    fn pointer_motion(&mut self) {
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
        if let Some(editor) = &mut self.editor {
            let (rows, remainder) = wheel_rows(self.wheel_remainder, steps, 3);
            self.wheel_remainder = remainder;
            self.dirty |= editor.scroll_lines(-rows);
        } else if self.mouse_reporting() {
            // Report whole wheel notches and keep the fractional remainder so
            // high-resolution touchpad scrolling does not lose events.
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
        } else if let Some(terminal) = &mut self.terminal {
            let screen = terminal.screen_mut();
            let (rows, remainder) = wheel_rows(self.wheel_remainder, steps, screen.scroll_speed());
            self.wheel_remainder = remainder;
            self.dirty |= screen.scroll_view(rows);
        }
        self.update_drag(false);
    }

    fn update_drag(&mut self, tick: bool) {
        if self.editor.is_some() {
            let (Some(renderer), Some(editor)) = (&self.renderer, &mut self.editor) else {
                return;
            };
            if !self.pointer_focus || !editor.is_mouse_dragging() {
                return;
            }
            let geometry = renderer
                .geometry
                .for_window(self.width as usize, self.height as usize);
            let (x, y) = self.pointer_position;
            if tick {
                let top = geometry.padding as f64;
                let bottom = top + (editor.view_rows() * geometry.cell_height) as f64;
                if y < top {
                    self.dirty |= editor.scroll_lines(-1);
                } else if y >= bottom {
                    self.dirty |= editor.scroll_lines(1);
                }
            }
            self.dirty |= editor.mouse_drag(x, y, metrics(geometry));
            return;
        }
        if !self.selection.is_dragging() || !self.pointer_focus {
            self.drag_scroll_at = None;
            return;
        }
        let (Some(renderer), Some(terminal)) = (&self.renderer, &mut self.terminal) else {
            return;
        };
        if !self.selection.active(terminal.screen()) {
            self.selection.clear();
            self.drag_scroll_at = None;
            self.dirty = true;
            return;
        }
        let (x, y) = self.pointer_position;
        let geometry = renderer
            .geometry
            .for_window(self.width as usize, self.height as usize);
        let top = geometry.padding as f64;
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
        self.dirty |= self
            .selection
            .motion(screen, crate::selection::position(geometry, x, y, screen));
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
    fn copy(&mut self, serial: u32, qh: &QueueHandle<Self>) {
        let text = if let Some(editor) = &self.editor {
            editor.selection_text()
        } else {
            self.terminal
                .as_ref()
                .and_then(|t| self.selection.text(t.screen()))
        };
        if let Some(text) = text {
            self.set_clipboard(text, serial, qh);
        }
    }
    fn paste(&mut self) {
        if self.debug_input {
            eprintln!("[clipboard] paste requested");
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
        let destination = if self.editor.is_some() {
            Destination::Editor
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
                                Destination::Editor => {
                                    if let Some(editor) = &mut self.editor {
                                        editor.insert_text(text);
                                        self.dirty = true;
                                    }
                                }
                                Destination::Terminal => {
                                    let bytes = paste_bytes(&transfer.bytes, transfer.bracketed);
                                    self.input.extend(bytes);
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
                if self.debug_input && old_bracketed != terminal.screen().bracketed_paste {
                    eprintln!(
                        "[clipboard] bracketed paste={}",
                        terminal.screen().bracketed_paste
                    );
                }
                self.input.extend(terminal.take_responses());
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
        let confirming = self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.is_confirming_close());
        let Some(command) = editor_command(&press, confirming) else {
            return;
        };
        let effect = match &mut self.editor {
            Some(editor) => editor.handle(command),
            None => return,
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
        if self.backend == ActiveBackend::OpenGl && self.gl.is_some() {
            let logical = (self.width as usize, self.height as usize);
            let force_full = self.damage_full || self.editor.is_some();
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
            if damage.is_empty() {
                self.dirty = false;
                return Ok(());
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
                    self.fallback_to_software(reason, qh)?;
                    self.redraw_software(qh)?;
                }
            }
            return Ok(());
        }
        self.redraw_software(qh)
    }

    fn redraw_software(&mut self, qh: &QueueHandle<Self>) -> Result<(), Box<dyn Error>> {
        self.ensure_software_buffers(qh)?;
        let Some(index) = self
            .buffers
            .iter()
            .position(|buffer| !buffer.busy && buffer.generation == self.generation)
        else {
            return Ok(());
        };
        if let (Some(surface), Some(terminal), Some(renderer)) =
            (&self.surface, &self.terminal, &mut self.renderer)
        {
            let frame = &mut self.buffers[index];
            match &mut self.editor {
                Some(editor) => {
                    let (columns, rows) = renderer
                        .geometry
                        .grid_size(self.width as usize, self.height as usize);
                    let view = editor.view(columns, rows);
                    renderer.render_editor_into(
                        &view,
                        (self.width as usize, self.height as usize),
                        self.keyboard_focus,
                        self.blink_on,
                        &mut self.pixels,
                    );
                }
                None => renderer.render_into(
                    terminal.screen(),
                    (self.width as usize, self.height as usize),
                    &self.selection,
                    self.keyboard_focus,
                    self.blink_on,
                    &mut self.pixels,
                ),
            }
            frame.file.write_all_at(&self.pixels, 0)?;
            surface.attach(Some(&frame.buffer), 0, 0);
            surface.damage_buffer(0, 0, self.width, self.height);
            surface.frame(qh, ());
            surface.commit();
            self.frame_pending = true;
            frame.busy = true;
            self.dirty = false;
            if self.debug_input {
                eprintln!(
                    "[output] framebuffer {index} wrote {} bytes and committed",
                    self.pixels.len()
                );
            }
        }
        Ok(())
    }

    fn render_gl_frame(
        &mut self,
        full: bool,
        damage: &RowDamage,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
        let sizes = SurfaceSizes::new((self.width, self.height), self.scale);
        let logical = (sizes.logical.0 as usize, sizes.logical.1 as usize);
        let scale = self.scale;
        let physical = sizes.physical;
        let geometry = self
            .renderer
            .as_ref()
            .ok_or("renderer unavailable")?
            .geometry;
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
                .map(|range| row_pixel_rect(geometry, logical, scale, physical, *range))
                .filter(|rect| rect.h > 0 && rect.w > 0)
                .collect()
        };
        if rects.is_empty() {
            return Ok(());
        }
        if let Some(surface) = &self.surface {
            surface.frame(qh, ());
        }
        let renderer = self.renderer.as_mut().ok_or("renderer unavailable")?;
        let gl = self.gl.as_mut().ok_or("OpenGL backend unavailable")?;
        if full {
            let scene = match &mut self.editor {
                Some(editor) => {
                    let (columns, rows) = geometry.grid_size(logical.0, logical.1);
                    let view = editor.view(columns, rows);
                    renderer.build_editor_scene(
                        &view,
                        logical,
                        self.keyboard_focus,
                        self.blink_on,
                        scale,
                    )
                }
                None => {
                    let terminal = self.terminal.as_ref().ok_or("terminal unavailable")?;
                    renderer.build_terminal_scene(
                        terminal.screen(),
                        logical,
                        &self.selection,
                        self.keyboard_focus,
                        self.blink_on,
                        scale,
                        None,
                    )
                }
            };
            gl.prepare_scene(renderer, &scene);
            gl.draw(&scene, &rects, true)?;
        } else {
            for (rect, range) in rects.iter().zip(damage.ranges.iter()) {
                let scene = match &mut self.editor {
                    Some(editor) => {
                        let (columns, rows) = geometry.grid_size(logical.0, logical.1);
                        let view = editor.view(columns, rows);
                        renderer.build_editor_scene(
                            &view,
                            logical,
                            self.keyboard_focus,
                            self.blink_on,
                            scale,
                        )
                    }
                    None => {
                        let terminal = self.terminal.as_ref().ok_or("terminal unavailable")?;
                        renderer.build_terminal_scene(
                            terminal.screen(),
                            logical,
                            &self.selection,
                            self.keyboard_focus,
                            self.blink_on,
                            scale,
                            Some(*range),
                        )
                    }
                };
                gl.prepare_scene(renderer, &scene);
                gl.draw(&scene, &[*rect], false)?;
            }
        }
        gl.present(&rects, full)
    }
}

#[derive(PartialEq, Eq)]
struct VisibleState {
    start: usize,
    offset: usize,
    revision: u64,
    cursor: Option<(usize, usize, Option<crate::terminal::CursorStyle>)>,
}

fn visible_state(screen: &crate::terminal::Screen) -> VisibleState {
    // When every visible row is in history, PTY writes cannot change the visible cells.
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
                    if editor.request_close() {
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
        qh: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event {
            state.set_preferred_scale(factor, qh);
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
        qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.set_fractional_scale(scale, qh);
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
                    let frame = state.buffers.remove(index);
                    frame.buffer.destroy();
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
            wl_pointer::Event::AxisDiscrete {
                axis: wayland_client::WEnum::Value(wl_pointer::Axis::VerticalScroll),
                discrete,
            } if state.pointer_focus => {
                *state.wheel_steps.get_or_insert(0.0) += f64::from(discrete);
            }
            wl_pointer::Event::AxisValue120 {
                axis: wayland_client::WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value120,
            } if state.pointer_focus => {
                *state.wheel_steps.get_or_insert(0.0) += f64::from(value120) / 120.0;
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
                button,
                state: wayland_client::WEnum::Value(button_state),
                time,
                ..
            } => {
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
                        state.xkb = Some(x)
                    }
                    Err(e) => state.input_error = Some(format!("load keyboard keymap: {e}")),
                }
            }
            wl_keyboard::Event::Enter { surface, .. } => {
                state.keyboard_focus = state.surface.as_ref() == Some(&surface);
                state.dirty = true;
                if state.debug_input {
                    eprintln!("[input] keyboard enter: focused={}", state.keyboard_focus);
                }
            }
            wl_keyboard::Event::Leave { .. } => {
                state.keyboard_focus = false;
                state.dirty = true;
                if state.debug_input {
                    eprintln!("[input] keyboard leave");
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
                    x.modifiers(mods_depressed, mods_latched, mods_locked, group);
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
                if state.keyboard_focus
                    && key_state == wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed)
                    && let Some(x) = &state.xkb
                {
                    if state.editor.is_some() {
                        state.editor_key(key, serial, qh);
                        return;
                    }
                    if x.shift() && !x.control() && matches!(key, 102 | 107 | 104 | 109) {
                        if let Some(t) = &mut state.terminal {
                            let rows = t.screen().rows as isize;
                            let delta = match key {
                                104 => rows,
                                109 => -rows,
                                102 => isize::MAX,
                                _ => isize::MIN,
                            };
                            state.dirty |= t.screen_mut().scroll_view(delta);
                        }
                        return;
                    }
                    if x.control() && key == 46 {
                        if state
                            .terminal
                            .as_ref()
                            .is_some_and(|t| state.selection.active(t.screen()))
                        {
                            state.copy(serial, qh);
                        } else if !x.shift() {
                            state.input.push_back(3);
                        }
                        return;
                    }
                    if x.control() && key == 47 {
                        state.paste();
                        return;
                    }
                    let bytes = x.key(key);
                    if !bytes.is_empty()
                        && let Some(t) = &mut state.terminal
                    {
                        state.dirty |= t.screen().view_offset() > 0;
                        t.screen_mut().reset_view();
                    }
                    if state.debug_input {
                        eprintln!("[input] translated {} bytes", bytes.len());
                    }
                    state.input.extend(bytes);
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
        let sizes = SurfaceSizes::new((1528, 812), 1.25);
        assert_eq!(sizes.viewport_destination(), (1528, 812));
        assert_eq!(sizes.physical, (1910, 1015));
        assert_ne!(
            sizes.viewport_destination(),
            sizes.physical,
            "the viewport destination must stay in surface-local coordinates"
        );

        let minimum = SurfaceSizes::new((0, 0), 2.0);
        assert_eq!(minimum.logical, (1, 1));
        assert_eq!(minimum.viewport_destination(), (1, 1));
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
            let sizes = SurfaceSizes::new(logical, 1.25);
            assert_eq!(sizes.viewport_destination(), logical);
            assert_eq!(sizes.physical, physical);
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
        let geometry = state
            .renderer
            .as_ref()
            .unwrap()
            .geometry
            .for_window(state.width as usize, state.height as usize);
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
            pointer_position: (22.0, -10.0),
            width: 104,
            height: 60,
            ..Default::default()
        };
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
            editor: Some(Editor::open(dir.path().join("paste.txt")).unwrap()),
            ..Default::default()
        };
        state.transfers.push(Transfer {
            file: File::from(read),
            bytes: Vec::new(),
            offset: 0,
            writing: false,
            bracketed: false,
            destination: Destination::Editor,
        });
        let text = "первая строка\nsecond line\n\n世界";
        write.write_all(text.as_bytes()).unwrap();
        drop(write);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.editor.as_ref().is_some_and(|e| e.text().is_empty())
            && std::time::Instant::now() < deadline
        {
            state.pump_clipboard();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(state.editor.as_ref().unwrap().text(), text);
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
