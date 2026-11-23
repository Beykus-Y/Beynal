// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Cargo and Fedora install regular files. Release archives use a symlink.
use std::os::unix::process::CommandExt;

fn main() -> std::process::ExitCode {
    let error = match std::env::current_exe() {
        Ok(path) => std::process::Command::new(path.with_file_name("beyrix"))
            .arg0("bedit")
            .args(std::env::args_os().skip(1))
            .exec(),
        Err(error) => error,
    };
    eprintln!("bedit: cannot start the adjacent beyrix executable: {error}");
    std::process::ExitCode::FAILURE
}
