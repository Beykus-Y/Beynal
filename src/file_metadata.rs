// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    ffi::CStr,
    fs::{File, FileTimes, Metadata},
    io,
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

/// Copy the metadata associated with an existing file before publishing its
/// replacement. Use file descriptors so the same inodes are used throughout.
pub fn preserve(source: &File, destination: &File, metadata: &Metadata) -> io::Result<()> {
    let staged = destination.metadata()?;
    if (staged.uid(), staged.gid()) != (metadata.uid(), metadata.gid()) {
        // chown can clear mode bits and capabilities, so perform it first.
        status(unsafe { libc::fchown(destination.as_raw_fd(), metadata.uid(), metadata.gid()) })?;
    }
    destination.set_permissions(metadata.permissions())?;

    let source_names = names(source)?;
    let destination_names = names(destination)?;
    // A temporary file can inherit an ACL from the directory or a security
    // label from policy. Preserve the source's attribute set.
    for name in destination_names
        .iter()
        .filter(|name| !source_names.contains(name))
    {
        status(unsafe { libc::fremovexattr(destination.as_raw_fd(), name.as_ptr()) })?;
    }
    for name in source_names {
        let value = get(source, &name)?;
        if destination_names.contains(&name) && get(destination, &name)? == value {
            continue;
        }
        set(destination, &name, &value)?;
    }
    destination.set_times(FileTimes::new().set_accessed(metadata.accessed()?))?;
    let staged = destination.metadata()?;
    if staged.permissions().mode() != metadata.permissions().mode()
        || (staged.uid(), staged.gid()) != (metadata.uid(), metadata.gid())
    {
        return Err(io::Error::other(
            "replacement file ownership or mode differs from the original",
        ));
    }
    Ok(())
}

fn names(file: &File) -> io::Result<Vec<std::ffi::CString>> {
    let list = read_attribute(|data, capacity| unsafe {
        libc::flistxattr(file.as_raw_fd(), data.cast(), capacity)
    });
    let bytes = match list {
        Ok(bytes) => bytes,
        Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    bytes
        .split_inclusive(|byte| *byte == 0)
        .map(|bytes| {
            CStr::from_bytes_with_nul(bytes)
                .map(CStr::to_owned)
                .map_err(io::Error::other)
        })
        .collect()
}

fn get(file: &File, name: &CStr) -> io::Result<Vec<u8>> {
    read_attribute(|data, capacity| unsafe {
        libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), data.cast(), capacity)
    })
}

fn set(file: &File, name: &CStr, data: &[u8]) -> io::Result<()> {
    status(unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            data.as_ptr().cast(),
            data.len(),
            0,
        )
    })
}

fn read_attribute(call: impl Fn(*mut u8, usize) -> isize) -> io::Result<Vec<u8>> {
    let length = size(call(std::ptr::null_mut(), 0))?;
    let mut data = vec![0; length];
    let received = size(call(data.as_mut_ptr(), length))?;
    if received > length {
        return Err(io::Error::other(
            "file extended attribute changed while reading",
        ));
    }
    data.truncate(received);
    Ok(data)
}

fn size(result: isize) -> io::Result<usize> {
    usize::try_from(result).map_err(|_| io::Error::last_os_error())
}

fn status(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Document;

    #[test]
    fn atomic_save_preserves_owner_mode_user_attributes_and_acl() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("with-metadata.txt");
        std::fs::write(&path, "old").unwrap();
        let file = File::open(&path).unwrap();
        let attribute = c"user.bedit.test";
        if let Err(error) = set(&file, attribute, b"test\0value") {
            if error.raw_os_error() == Some(libc::EOPNOTSUPP) {
                return;
            }
            panic!("set xattr: {error}");
        }
        // Linux POSIX ACL xattr format v2 with one named user and a mask.
        let mut acl = 2_u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [
            (0x01_u16, 6_u16, u32::MAX),
            (0x02, 4, file.metadata().unwrap().uid().wrapping_add(1)),
            (0x04, 4, u32::MAX),
            (0x10, 4, u32::MAX),
            (0x20, 0, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(perm.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        let acl_name = c"system.posix_acl_access";
        let acl = match set(&file, acl_name, &acl) {
            Ok(()) => Some(get(&file, acl_name).unwrap()),
            Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => None,
            Err(error) => panic!("set ACL: {error}"),
        };
        let before = file.metadata().unwrap();
        let mut doc = Document::open(&path).unwrap();
        doc.insert_text("new ");
        doc.save().unwrap();
        let saved = File::open(&path).unwrap();
        let after = saved.metadata().unwrap();
        assert_eq!(
            (before.uid(), before.gid(), before.mode()),
            (after.uid(), after.gid(), after.mode())
        );
        assert_eq!(get(&saved, attribute).unwrap(), b"test\0value");
        if let Some(acl) = acl {
            assert_eq!(get(&saved, acl_name).unwrap(), acl);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new old");
    }
}
