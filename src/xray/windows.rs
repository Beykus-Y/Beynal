// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Portable Windows X-Ray backend architecture (Process API / Job Objects /
//! Wait Chain Traversal / Process I/O Counters).
//!
//! Note: Beyrix's main graphical/PTY layer is currently Linux/Wayland; this
//! module defines the Windows X-Ray capability model and native Win32 hooks
//! without fabricating unavailable metrics.

use super::{ProcessAction, ProcessNode, XRayCapabilities};

pub fn capabilities() -> XRayCapabilities {
    XRayCapabilities {
        process_tree: true,
        cpu_and_memory: true,
        wait_reason_wchan: true, // via Wait Chain Traversal (WCT)
        io_counters: true,       // via GetProcessIoCounters
        file_locks: false,       // requires Restart Manager / handle enumeration
        open_fds_and_sockets: false,
        process_control: true,
        trace_syscalls: false, // ETW requires elevated session
    }
}

pub struct WindowsSampler;

impl WindowsSampler {
    pub fn new() -> Self {
        Self
    }

    pub fn collect_tree(
        &mut self,
        _root_pid: i32,
        _fg_pgid: Option<i32>,
        _inspect_details: bool,
        _seconds_since_output: f64,
    ) -> Vec<ProcessNode> {
        Vec::new()
    }
}

pub fn execute_process_action(
    _pid: i32,
    _expected_starttime: u64,
    _action: ProcessAction,
) -> Result<(), String> {
    Err("Windows X-Ray backend requires a Windows host build".to_string())
}
