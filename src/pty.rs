// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ffi::CString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;

pub struct Pty {
    pub master: OwnedFd,
    pub child_pid: libc::pid_t,
    pub _command_dir: Option<tempfile::TempDir>,
}

impl Pty {
    pub fn resize(&self, columns: u16, rows: u16) -> io::Result<()> {
        let size = libc::winsize {
            ws_col: columns,
            ws_row: rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn spawn() -> io::Result<Self> {
        Self::spawn_with_prompt(None, None, None)
    }

    #[cfg(test)]
    pub fn spawn_with_size(size: Option<(u16, u16)>) -> io::Result<Self> {
        Self::spawn_with_prompt(size, None, None)
    }
    pub fn spawn_with_prompt(
        size: Option<(u16, u16)>,
        theme: Option<&str>,
        control: Option<(&std::path::Path, &std::path::Path)>,
    ) -> io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let control = control
            .map(|(socket, prompt)| -> io::Result<_> {
                Ok((
                    CString::new(socket.as_os_str().as_bytes()).map_err(io::Error::other)?,
                    CString::new(prompt.as_os_str().as_bytes()).map_err(io::Error::other)?,
                ))
            })
            .transpose()?;
        let command_dir = if theme.is_some() {
            Some(command_directory(&std::env::current_exe()?)?)
        } else {
            None
        };
        let command_path = command_dir
            .as_ref()
            .map(|dir| {
                use std::os::unix::ffi::OsStrExt;
                CString::new(dir.path().as_os_str().as_bytes()).map_err(io::Error::other)
            })
            .transpose()?;
        let rcfile = if theme.is_some() {
            let mut file = tempfile::tempfile()?;
            file.write_all(crate::prompt::script().as_bytes())?;
            let fd = file.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Some(file)
        } else {
            None
        };
        let rc_path = rcfile
            .as_ref()
            .map(|f| CString::new(format!("/proc/self/fd/{}", f.as_raw_fd())).unwrap());
        let mut master_fd = -1;
        let mut slave_fd = -1;

        let winsize = size.map(|(cols, rows)| libc::winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        });
        let result = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                ptr::null_mut(),
                ptr::null_mut(),
                winsize.as_ref().map_or(ptr::null(), |v| v as *const _),
            )
        };

        if result == -1 {
            return Err(io::Error::last_os_error());
        }

        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };

        let pid = unsafe { libc::fork() };

        if pid == -1 {
            return Err(io::Error::last_os_error());
        }

        if pid == 0 {
            drop(master);

            if unsafe { libc::setsid() } == -1 {
                unsafe { libc::_exit(1) };
            }

            if unsafe { libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) } == -1 {
                unsafe { libc::_exit(1) };
            }

            for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                if unsafe { libc::dup2(slave_fd, fd) } == -1 {
                    unsafe { libc::_exit(1) };
                }
            }

            drop(slave);

            if let Some((socket, prompt)) = &control {
                unsafe {
                    libc::setenv(c"BEYRIX_CONTROL_SOCKET".as_ptr(), socket.as_ptr(), 1);
                    libc::setenv(c"BEYRIX_PROMPT_THEME".as_ptr(), prompt.as_ptr(), 1);
                }
            }
            if let Some(path) = &command_path {
                unsafe {
                    libc::setenv(c"BEYRIX_COMMAND_DIR".as_ptr(), path.as_ptr(), 1);
                }
            }
            if size.is_some() {
                unsafe {
                    libc::setenv(c"TERM".as_ptr(), c"ansi".as_ptr(), 1);
                    libc::unsetenv(c"COLORTERM".as_ptr());
                }
            }
            let shell = CString::new("/bin/bash").unwrap();
            let arg0 = CString::new("bash").unwrap();
            let arg1 = CString::new("-i").unwrap();
            let rc_arg = CString::new("--rcfile").unwrap();

            unsafe {
                if let Some(path) = &rc_path {
                    libc::execl(
                        shell.as_ptr(),
                        arg0.as_ptr(),
                        rc_arg.as_ptr(),
                        path.as_ptr(),
                        arg1.as_ptr(),
                        ptr::null::<libc::c_char>(),
                    );
                }
                libc::execl(
                    shell.as_ptr(),
                    arg0.as_ptr(),
                    arg1.as_ptr(),
                    ptr::null::<libc::c_char>(),
                );

                libc::_exit(127);
            }
        }

        drop(slave);

        Ok(Self {
            master,
            child_pid: pid,
            _command_dir: command_dir,
        })
    }
}

fn command_directory(executable: &std::path::Path) -> io::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    std::os::unix::fs::symlink(executable, dir.path().join("beyrix"))?;
    // Keep the old command name available inside shells started by Beynal
    // until existing sessions finish their upgrade.
    std::os::unix::fs::symlink(executable, dir.path().join("beynal"))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn command_directory_resolves_real_executable() {
        let executable = std::env::current_exe().unwrap();
        let dir = command_directory(&executable).unwrap();
        assert_eq!(
            std::fs::canonicalize(dir.path().join("beyrix")).unwrap(),
            executable
        );
        assert_eq!(
            std::fs::canonicalize(dir.path().join("beynal")).unwrap(),
            executable
        );
    }
    #[test]
    fn graphical_pty_has_requested_size() {
        let pty = Pty::spawn_with_size(Some((80, 27))).unwrap();
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::ioctl(pty.master.as_raw_fd(), libc::TIOCGWINSZ, &mut size) },
            0
        );
        assert_eq!((size.ws_col, size.ws_row), (80, 27));
        pty.resize(42, 12).unwrap();
        assert_eq!(
            unsafe { libc::ioctl(pty.master.as_raw_fd(), libc::TIOCGWINSZ, &mut size) },
            0
        );
        assert_eq!((size.ws_col, size.ws_row), (42, 12));
        unsafe {
            libc::kill(pty.child_pid, libc::SIGKILL);
            libc::waitpid(pty.child_pid, std::ptr::null_mut(), 0);
        }
    }
}
