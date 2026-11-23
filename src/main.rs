// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod compat;
mod control;
mod editor;
mod gl;
mod gpu;
mod keyboard;
mod mouse;
mod prompt;
mod pty;
mod render;
mod selection;
mod terminal;
mod theme;
mod wayland;
use pty::Pty;
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::thread;

struct RawMode {
    original: libc::termios,
}

impl RawMode {
    fn enable() -> io::Result<Self> {
        let fd = libc::STDIN_FILENO;
        let mut original = unsafe { std::mem::zeroed() };

        if unsafe { libc::tcgetattr(fd, &mut original) } == -1 {
            return Err(io::Error::last_os_error());
        }

        let mut raw = original;

        unsafe {
            libc::cfmakeraw(&mut raw);
        }

        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let Some(args) = args
        .iter()
        .map(|arg| arg.to_str())
        .collect::<Option<Vec<_>>>()
    else {
        eprintln!("beyrix: arguments must be valid UTF-8");
        return std::process::ExitCode::FAILURE;
    };
    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("beyrix: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn print_help() {
    let color = io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("TERM").as_deref() != Some(std::ffi::OsStr::new("dumb"));
    let (accent, reset) = if color {
        ("\x1b[1;36m", "\x1b[0m")
    } else {
        ("", "")
    };
    println!("{accent}Beyrix{reset} · Wayland terminal\n");
    for (command, description) in [
        ("open", "Open a new window"),
        ("theme", "Show the active theme"),
        ("theme list", "List built-in themes"),
        ("theme set NAME", "Save a theme and update this window"),
        ("layout [list|set NAME]", "Show or save the cursor layout"),
        (
            "renderer [list|set NAME]",
            "Show or save the rendering backend",
        ),
        (
            "gpu",
            "Show detected GPUs, graphics APIs and backend choice",
        ),
        ("edit FILE", "Open a file in the built-in editor"),
        ("--pty", "Run Bash in the current terminal"),
        ("--help", "Show this help"),
    ] {
        println!("  {accent}beyrix {command:<14}{reset}  {description}");
    }
}

fn run(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    match args {
        ["--help" | "-h"] => print_help(),
        [] if crate::compat::env("BEYRIX_CONTROL_SOCKET").is_some() => print_help(),
        [] | ["open"] => wayland::run()?,
        ["--pty"] => run_pty()?,
        ["edit", file] => edit_file(file)?,
        ["edit", ..] => return Err("usage: beyrix edit FILE".into()),
        ["theme"] => {
            let active = crate::compat::env("BEYRIX_PROMPT_THEME")
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|data| {
                    data.lines()
                        .next()
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                });
            let name = match active {
                Some(name) => name,
                None => theme::AppConfig::load()?.theme.name,
            };
            println!("{name}");
        }
        ["theme", "list"] => println!("minimal\ndeveloper\nsignature"),
        ["theme", "set", name] => {
            theme::Theme::save_choice(name)?;
            match crate::compat::env("BEYRIX_CONTROL_SOCKET") {
                Some(path) => match control::notify_theme(std::path::Path::new(&path), name) {
                    Ok(()) => println!("Theme: {name} (update requested)"),
                    Err(e) => {
                        eprintln!("Theme saved for next launch; window notification failed: {e}")
                    }
                },
                None => println!("Theme: {name} (saved for next launch)"),
            }
        }
        ["theme", ..] => return Err("usage: beyrix theme [list | set NAME]".into()),
        ["layout"] => println!("{}", theme::Theme::load_layout()?.name()),
        ["layout", "list"] => {
            for mode in terminal::LayoutMode::ALL {
                println!("{}", mode.name());
            }
        }
        ["layout", "set", name] => {
            let mode = terminal::LayoutMode::parse(name).ok_or_else(|| {
                format!("unknown layout {name:?}; expected as-is, bottom, or bottom-input")
            })?;
            theme::Theme::save_layout(mode)?;
            println!("Layout: {} (saved for new windows)", mode.name());
        }
        ["layout", ..] => return Err("usage: beyrix layout [list | set NAME]".into()),
        ["renderer"] => println!("{}", theme::Theme::load_renderer()?),
        ["renderer", "list"] => {
            for choice in gpu::RendererChoice::ALL {
                println!("{}", choice.name());
            }
        }
        ["renderer", "set", name] => {
            let choice = gpu::RendererChoice::parse(name).ok_or_else(|| {
                format!("unknown renderer {name:?}; expected auto, opengl, vulkan, or software")
            })?;
            theme::Theme::save_renderer(choice)?;
            println!("Renderer: {} (saved for new windows)", choice.name());
        }
        ["renderer", ..] => return Err("usage: beyrix renderer [list | set NAME]".into()),
        ["gpu"] => graphics_report()?,
        [command @ ("open" | "--pty" | "--help" | "-h"), ..] => {
            return Err(format!("{command} takes no arguments").into());
        }
        [command, ..] => {
            return Err(format!("unknown command {command:?}. See beyrix --help").into());
        }
    }
    Ok(())
}

fn edit_file(file: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::PathBuf::from(file);
    // Validate before creating a window or talking to a running one, so CLI
    // errors (missing permissions, invalid UTF-8, directories) are immediate.
    editor::Editor::open(&path)?;
    if let Some(socket) = crate::compat::env("BEYRIX_CONTROL_SOCKET") {
        match control::notify_open(std::path::Path::new(&socket), &path) {
            Ok(()) => {
                println!("Opened {} in the running Beyrix window", path.display());
                return Ok(());
            }
            Err(error) => {
                eprintln!("beyrix: running window unavailable ({error}); starting a new window")
            }
        }
    }
    wayland::run_editor(path)?;
    Ok(())
}

fn graphics_report() -> Result<(), Box<dyn std::error::Error>> {
    use wayland_client::Proxy;
    let requested = theme::Theme::load_renderer()?;
    let connection = match wayland_client::Connection::connect_to_env() {
        Ok(connection) => Some(connection),
        Err(error) => {
            eprintln!("beyrix: Wayland display unavailable for EGL probe: {error}");
            None
        }
    };
    let display = connection
        .as_ref()
        .map(|connection| connection.display().id().as_ptr().cast());
    // Keep the connection alive while EGL is initialized on its display.
    let report = gpu::report(display);
    let decision = gpu::decide(requested, &report.egl);
    gpu::print_report(&report, &decision);
    Ok(())
}

fn run_pty() -> io::Result<()> {
    let pty = Pty::spawn()?;
    let _raw_mode = RawMode::enable()?;

    let master_fd = pty.master.as_raw_fd();

    let input_fd = unsafe { libc::dup(master_fd) };

    if input_fd == -1 {
        return Err(io::Error::last_os_error());
    }

    let input_thread = thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buffer = [0u8; 4096];

        loop {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut written = 0;

                    while written < n {
                        let result = unsafe {
                            libc::write(input_fd, buffer[written..n].as_ptr().cast(), n - written)
                        };

                        if result <= 0 {
                            unsafe { libc::close(input_fd) };
                            return;
                        }

                        written += result as usize;
                    }
                }
            }
        }

        unsafe { libc::close(input_fd) };
    });

    let mut output = std::fs::File::from(pty.master);
    let mut stdout = io::stdout();
    let mut buffer = [0u8; 4096];

    loop {
        match output.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                stdout.write_all(&buffer[..n])?;
                stdout.flush()?;
            }
            Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
            Err(err) => return Err(err),
        }
    }

    // Пока поток ввода может оставаться заблокированным на stdin.
    // Не ждём его завершения после выхода Bash.
    drop(input_thread);

    let mut status = 0;
    unsafe {
        libc::waitpid(pty.child_pid, &mut status, 0);
    }

    Ok(())
}
