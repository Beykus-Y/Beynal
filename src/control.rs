// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    ffi::OsString,
    io,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::PermissionsExt,
        net::UnixDatagram,
    },
    path::PathBuf,
};

pub const MAX_MESSAGE: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Theme(String),
    Open(PathBuf),
}

fn parse(bytes: &[u8]) -> Option<Message> {
    if let Some(rest) = bytes.strip_prefix(b"open:") {
        if rest.is_empty() {
            return None;
        }
        let path = PathBuf::from(OsString::from_vec(rest.to_vec()));
        return path.is_absolute().then_some(Message::Open(path));
    }
    let name = bytes.strip_prefix(b"theme:").unwrap_or(bytes);
    let name = std::str::from_utf8(name).ok()?;
    matches!(name, "minimal" | "developer" | "signature").then(|| Message::Theme(name.to_owned()))
}

pub struct Control {
    _dir: tempfile::TempDir,
    socket: UnixDatagram,
    pub path: PathBuf,
    pub prompt_path: PathBuf,
}
impl Control {
    pub fn new() -> io::Result<Self> {
        let root = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
        Self::new_in(std::path::Path::new(&root))
    }
    fn new_in(root: &std::path::Path) -> io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("beyrix-")
            .tempdir_in(root)?;
        let path = dir.path().join("control.sock");
        let socket = UnixDatagram::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        socket.set_nonblocking(true)?;
        let prompt_path = dir.path().join("theme");
        Ok(Self {
            _dir: dir,
            socket,
            path,
            prompt_path,
        })
    }
    pub fn socket(&self) -> &UnixDatagram {
        &self.socket
    }

    pub fn receive(&self) -> io::Result<Option<Message>> {
        let mut bytes = [0u8; MAX_MESSAGE];
        match self.socket.recv(&mut bytes) {
            Ok(n) => Ok(parse(&bytes[..n])),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
    pub fn set_prompt(&self, theme: &crate::theme::Theme) -> io::Result<()> {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(self._dir.path())?;
        writeln!(file, "{}\n{}", theme.name, theme.signature_compact)?;
        file.persist(&self.prompt_path)?;
        Ok(())
    }
}
pub fn notify_theme(path: &std::path::Path, name: &str) -> io::Result<()> {
    if !matches!(name, "minimal" | "developer" | "signature") {
        return Err(io::Error::other("invalid theme"));
    }
    UnixDatagram::unbound()?.send_to(format!("theme:{name}").as_bytes(), path)?;
    Ok(())
}

pub fn notify_open(path: &std::path::Path, file: &std::path::Path) -> io::Result<()> {
    let file = std::path::absolute(file)?;
    let mut message = b"open:".to_vec();
    message.extend_from_slice(file.as_os_str().as_bytes());
    if message.len() > MAX_MESSAGE {
        return Err(io::Error::other("path is too long"));
    }
    UnixDatagram::unbound()?.send_to(&message, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn isolated_sockets_and_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let a = Control::new_in(root.path()).unwrap();
        let b = Control::new_in(root.path()).unwrap();
        notify_theme(&a.path, "signature").unwrap();
        assert_eq!(
            a.receive().unwrap(),
            Some(Message::Theme("signature".into()))
        );
        assert!(b.receive().unwrap().is_none());
        assert!(notify_theme(&a.path, "invalid").is_err());
        let path = a.path.clone();
        drop(a);
        assert!(!path.exists());
        assert!(notify_theme(&path, "minimal").is_err());
    }

    #[test]
    fn open_messages_are_absolute_and_validated() {
        let root = tempfile::tempdir().unwrap();
        let control = Control::new_in(root.path()).unwrap();
        let file = root.path().join("with space/файл.txt");
        notify_open(&control.path, &file).unwrap();
        assert_eq!(
            control.receive().unwrap(),
            Some(Message::Open(std::path::absolute(&file).unwrap()))
        );
        assert_eq!(
            parse(b"theme:developer"),
            Some(Message::Theme("developer".into()))
        );
        assert_eq!(parse(b"unknown"), None);
        assert_eq!(parse(b"open:relative.txt"), None);
        assert_eq!(parse(b"open:"), None);
    }
}
