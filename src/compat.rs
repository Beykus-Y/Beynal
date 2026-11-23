// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Read-only compatibility with environment variables exported by Beynal.
//!
//! Beyrix itself always exports `BEYRIX_*` variables. These fallbacks only let a
//! new CLI talk to a window or shell started by the previous version while the
//! user finishes an upgrade, and can be removed once Beynal sessions are gone.

use std::ffi::OsString;

pub fn env(name: &str) -> Option<OsString> {
    std::env::var_os(name).or_else(|| {
        let legacy = match name {
            "BEYRIX_CONTROL_SOCKET" => "BEYNAL_CONTROL_SOCKET",
            "BEYRIX_PROMPT_THEME" => "BEYNAL_PROMPT_THEME",
            "BEYRIX_COMMAND_DIR" => "BEYNAL_COMMAND_DIR",
            "BEYRIX_GL_DUMP" => "BEYNAL_GL_DUMP",
            "BEYRIX_DEBUG_INPUT" => "BEYNAL_DEBUG_INPUT",
            _ => return None,
        };
        std::env::var_os(legacy)
    })
}
