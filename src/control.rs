// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    io,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::PathBuf,
};

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

    pub fn receive(&self) -> io::Result<Option<String>> {
        let mut bytes = [0u8; 64];
        match self.socket.recv(&mut bytes) {
            Ok(n) => {
                let name = std::str::from_utf8(&bytes[..n]).unwrap_or("");
                if matches!(name, "minimal" | "developer" | "signature") {
                    Ok(Some(name.to_owned()))
                } else {
                    Ok(None)
                }
            }
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
pub fn notify(path: &std::path::Path, name: &str) -> io::Result<()> {
    if !matches!(name, "minimal" | "developer" | "signature") {
        return Err(io::Error::other("invalid theme"));
    }
    UnixDatagram::unbound()?.send_to(name.as_bytes(), path)?;
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
        notify(&a.path, "signature").unwrap();
        assert_eq!(a.receive().unwrap().as_deref(), Some("signature"));
        assert!(b.receive().unwrap().is_none());
        assert!(notify(&a.path, "invalid").is_err());
        let path = a.path.clone();
        drop(a);
        assert!(!path.exists());
        assert!(notify(&path, "minimal").is_err());
    }
}
