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

use crate::pty::Pty;
use crate::render;
use crate::terminal::Terminal;

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
        wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
};

use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const WIDTH: i32 = 800;
const HEIGHT: i32 = 500;

struct Transfer {
    file: File,
    bytes: Vec<u8>,
    offset: usize,
    writing: bool,
    bracketed: bool,
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
    renderer: Option<render::Renderer>,
    pty: Option<Pty>,
}

pub fn run() -> Result<(), Box<dyn Error>> {
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
        debug_input: std::env::var_os("BEYRIX_DEBUG_INPUT").as_deref()
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

    state
        .create_window(&qh)
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
                Ok(Some(name)) => {
                    if let Err(e) = state.apply_theme(&name) {
                        eprintln!("theme update: {e}");
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
            if state.keyboard_focus
                && let (Some(renderer), Some(terminal)) = (&state.renderer, &state.terminal)
                && terminal.screen().cursor_visible
                && terminal.screen().view_offset() == 0
                && renderer.cursor_blinks(terminal.screen())
            {
                state.dirty = true;
            }
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

impl State {
    fn create_window(&mut self, qh: &QueueHandle<Self>) -> Result<(), Box<dyn Error>> {
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
        let renderer =
            render::Renderer::with_theme(theme.clone()).map_err(|e| format!("load fonts: {e}"))?;
        let (columns, rows) = renderer.geometry.grid_size(WIDTH as usize, HEIGHT as usize);
        let theme_name = theme.name.clone();
        if let Some(control) = &self.control {
            control.set_prompt(&theme)?;
        }
        let terminal = Terminal::with_theme(columns, rows, theme);
        self.create_buffers(WIDTH, HEIGHT, qh)?;

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

    fn apply_theme(&mut self, name: &str) -> Result<(), Box<dyn Error>> {
        let theme = crate::theme::Theme::load_for(name)?;
        let renderer = self.renderer.as_mut().ok_or("renderer unavailable")?;
        renderer.retheme(theme.clone())?;
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
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        let (columns, rows) = self
            .renderer
            .as_ref()
            .ok_or("renderer unavailable")?
            .geometry
            .grid_size(width as usize, height as usize);
        self.generation += 1;
        self.create_buffers(width, height, qh)?;
        if let Some(pty) = &self.pty {
            pty.resize(columns as u16, rows as u16)
                .map_err(|e| format!("TIOCSWINSZ: {e}"))?;
        }
        if let Some(terminal) = &mut self.terminal {
            terminal.resize(columns, rows);
        }
        self.width = width;
        self.height = height;
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

    fn scroll_pointer(&mut self) {
        let steps = self.wheel_steps.take().unwrap_or(self.wheel_axis / 10.0);
        self.wheel_axis = 0.0;
        if let Some(terminal) = &mut self.terminal {
            let screen = terminal.screen_mut();
            let (rows, remainder) = wheel_rows(self.wheel_remainder, steps, screen.scroll_speed());
            self.wheel_remainder = remainder;
            self.dirty |= screen.scroll_view(rows);
        }
        self.update_drag(false);
    }

    fn update_drag(&mut self, tick: bool) {
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
    fn copy(&mut self, serial: u32, qh: &QueueHandle<Self>) {
        let (Some(manager), Some(device)) = (&self.data_manager, &self.data_device) else {
            return;
        };
        let Some(text) = self
            .terminal
            .as_ref()
            .and_then(|t| self.selection.text(t.screen()))
        else {
            return;
        };
        let source = manager.create_data_source(qh, Arc::new(text.into_bytes()));
        source.offer("text/plain;charset=utf-8".into());
        source.offer("text/plain".into());
        device.set_selection(Some(&source), serial);
        if let Some(old) = self.data_source.replace(source) {
            old.destroy();
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
        self.transfers.push(Transfer {
            file: File::from(read),
            bytes: Vec::new(),
            offset: 0,
            writing: false,
            bracketed: self
                .terminal
                .as_ref()
                .is_some_and(|t| t.screen().bracketed_paste),
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
                        Ok(_) => {
                            let bytes = paste_bytes(&transfer.bytes, transfer.bracketed);
                            if self.debug_input {
                                eprintln!(
                                    "[clipboard] payload received={} queued={} bracketed={}",
                                    transfer.bytes.len(),
                                    bytes.len(),
                                    transfer.bracketed
                                );
                            }
                            self.input.extend(bytes);
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
        let mut bytes = [0u8; 8192];
        let n = unsafe {
            libc::read(
                pty.master.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if n <= 0 {
            if n < 0 {
                let error = std::io::Error::last_os_error();
                if self.debug_input {
                    eprintln!("[output] PTY read: {error}");
                }
                if error.kind() == std::io::ErrorKind::Interrupted
                    || error.kind() == std::io::ErrorKind::WouldBlock
                {
                    return Ok(());
                }
                if error.raw_os_error() != Some(libc::EIO) {
                    return Err(error.into());
                }
            }
            if self.debug_input {
                eprintln!("[output] PTY closed; reaping bash");
            }
            if let Some(pty) = self.pty.take() {
                unsafe {
                    libc::waitpid(pty.child_pid, std::ptr::null_mut(), 0);
                }
            }
            return Ok(());
        }
        let terminal = self.terminal.as_mut().unwrap();
        let old_bracketed = terminal.screen().bracketed_paste;
        let before = visible_state(terminal.screen());
        let epoch = terminal.screen().selection_epoch;
        let was_selected = self.selection.active(terminal.screen());
        if self.debug_input {
            eprintln!("[output] PTY read {n} bytes");
        }
        terminal.process(&bytes[..n as usize]);
        if self.debug_input && old_bracketed != terminal.screen().bracketed_paste {
            eprintln!(
                "[clipboard] bracketed paste={}",
                terminal.screen().bracketed_paste
            );
        }
        self.input.extend(terminal.take_responses());
        if self.debug_input {
            eprintln!(
                "[output] terminal processed {n} bytes, cursor=({}, {})",
                terminal.screen().cursor_x,
                terminal.screen().cursor_y
            );
        }
        self.dirty |= before != visible_state(terminal.screen())
            || was_selected != self.selection.active(terminal.screen())
            || (terminal.screen().view_offset() < terminal.screen().rows
                && epoch != terminal.screen().selection_epoch);
        Ok(())
    }

    fn redraw(&mut self, qh: &QueueHandle<Self>) -> Result<(), Box<dyn Error>> {
        if !self.configured || !self.dirty || self.frame_pending {
            return Ok(());
        }
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
            renderer.render_into(
                terminal.screen(),
                (self.width as usize, self.height as usize),
                &self.selection,
                self.keyboard_focus,
                self.blink_on,
                &mut self.pixels,
            );
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
}

#[derive(PartialEq, Eq)]
struct VisibleState {
    start: usize,
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
        screen.cursor_y,
        screen.cursor_style,
    ));
    VisibleState {
        start: screen.visible_start(),
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
                        version.min(4),
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
            xdg_toplevel::Event::Close => state.running = false,
            xdg_toplevel::Event::Configure { width, height, .. } => {
                let previous = state
                    .proposed_size
                    .or(state.pending_size)
                    .unwrap_or((state.width, state.height));
                state.proposed_size = Some((
                    if width > 0 { width } else { previous.0 },
                    if height > 0 { height } else { previous.1 },
                ));
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
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
                state.selection.release();
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
                state.update_drag(false);
            }
            wl_pointer::Event::Button {
                button: 0x110,
                state: wayland_client::WEnum::Value(button_state),
                time,
                ..
            } => match button_state {
                wl_pointer::ButtonState::Pressed if state.pointer_focus => {
                    if let (Some(renderer), Some(terminal)) = (&state.renderer, &state.terminal) {
                        let (x, y) = state.pointer_position;
                        let p = crate::selection::position(
                            renderer
                                .geometry
                                .for_window(state.width as usize, state.height as usize),
                            x,
                            y,
                            terminal.screen(),
                        );
                        state.selection.press(terminal.screen(), p, time, x, y);
                        state.dirty = true;
                    }
                }
                wl_pointer::ButtonState::Released => state.selection.release(),
                _ => {}
            },
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
    fn readline_accepts_bracketed_paste_and_edits_with_arrow_keys() {
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

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let wait_for = |state: &mut State, marker: &str| loop {
            state.read_pty().unwrap();
            let screen = state.terminal.as_ref().unwrap().screen();
            if screen.bracketed_paste
                && (0..screen.rows).any(|row| screen.line(row).contains(marker))
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "missing {marker}");
            std::thread::sleep(Duration::from_millis(5));
        };
        wait_for(&mut state, "READLINE_READY");

        state
            .input
            .extend(paste_bytes(b"printf 'READLINE_%s\\n' OX", true));
        state.input.extend(b"\x1b[DK\n");
        state.write_input().unwrap();
        wait_for(&mut state, "READLINE_OKX");

        state.input.extend(b"exit\n");
        state.write_input().unwrap();
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
