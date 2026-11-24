// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native Linux procfs + pidfd + /proc/locks backend for Beyrix X-Ray.

use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;

use super::diagnosis::{
    Diagnosis, DiagnosisInput, FileLockInfo, ProcessStateKind, classify_wait_reason,
};
use super::{ProcessAction, ProcessNode, XRayCapabilities};

pub fn capabilities() -> XRayCapabilities {
    let wchan_ok = fs::read_to_string("/proc/self/wchan").is_ok();
    let io_ok = fs::read_to_string("/proc/self/io").is_ok();
    let locks_ok = fs::read_to_string("/proc/locks").is_ok();
    XRayCapabilities {
        process_tree: true,
        cpu_and_memory: true,
        wait_reason_wchan: wchan_ok,
        io_counters: io_ok,
        file_locks: locks_ok,
        open_fds_and_sockets: true,
        process_control: true,
        trace_syscalls: false,
    }
}

/// Sample total system CPU ticks from `/proc/stat` and total RAM from `/proc/meminfo`.
pub fn read_system_cpu_mem() -> (u64, u64) {
    let cpu_ticks = fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| {
            let line = s.lines().next()?;
            Some(
                line.split_whitespace()
                    .skip(1)
                    .filter_map(|v| v.parse::<u64>().ok())
                    .sum::<u64>(),
            )
        })
        .unwrap_or(0);
    let mem_total_kb = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .unwrap_or(1);
    (cpu_ticks, mem_total_kb * 1024)
}

#[derive(Clone, Debug)]
struct ProcStatRaw {
    pid: i32,
    ppid: i32,
    pgid: i32,
    comm: String,
    state: ProcessStateKind,
    utime: u64,
    stime: u64,
    starttime: u64,
    rss_bytes: u64,
    threads: u64,
}

fn read_proc_stat(pid: i32) -> Option<ProcStatRaw> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text[open + 1..close].to_string();
    let rest: Vec<&str> = text[close + 2..].split_whitespace().collect();
    if rest.len() < 22 {
        return None;
    }
    let state = ProcessStateKind::from_proc_char(rest[0].chars().next().unwrap_or('?'));
    let ppid = rest[1].parse().ok()?;
    let pgid = rest[2].parse().ok()?;
    let utime = rest[11].parse::<u64>().ok()?;
    let stime = rest[12].parse::<u64>().ok()?;
    let threads = rest[17].parse::<u64>().unwrap_or(1);
    let starttime = rest[19].parse::<u64>().ok()?;
    let rss_pages = rest[21].parse::<u64>().unwrap_or(0);
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    Some(ProcStatRaw {
        pid,
        ppid,
        pgid,
        comm,
        state,
        utime,
        stime,
        starttime,
        rss_bytes: rss_pages * page_size,
        threads,
    })
}

fn read_proc_io(pid: i32) -> Option<(u64, u64, u64, u64)> {
    let text = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let mut rchar = 0;
    let mut wchar = 0;
    let mut read_bytes = 0;
    let mut write_bytes = 0;
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let val = v.trim().parse::<u64>().unwrap_or(0);
            match k {
                "rchar" => rchar = val,
                "wchar" => wchar = val,
                "read_bytes" => read_bytes = val,
                "write_bytes" => write_bytes = val,
                _ => {}
            }
        }
    }
    Some((rchar, wchar, read_bytes, write_bytes))
}

#[derive(Clone, Debug)]
struct LockEntry {
    pid: i32,
    mode: String,
    inode: String,
    blocked: bool,
}

fn parse_proc_locks() -> Vec<LockEntry> {
    let Ok(text) = fs::read_to_string("/proc/locks") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            continue;
        }
        let blocked = parts[1] == "->";
        let offset = if blocked { 1 } else { 0 };
        if parts.len() < 6 + offset {
            continue;
        }
        let kind = parts[1 + offset];
        let mode = parts[3 + offset];
        let Ok(pid) = parts[4 + offset].parse::<i32>() else {
            continue;
        };
        let inode = parts[5 + offset].to_string();
        out.push(LockEntry {
            pid,
            mode: format!("{kind} {mode}"),
            inode,
            blocked,
        });
    }
    out
}

pub fn foreground_pgid(pty_master_fd: Option<i32>) -> Option<i32> {
    let fd = pty_master_fd?;
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    (pgid > 0).then_some(pgid)
}

pub struct LinuxSampler {
    prev_total_cpu: u64,
    prev_proc_ticks: HashMap<(i32, u64), u64>,
    prev_io: HashMap<(i32, u64), u64>,
}

impl LinuxSampler {
    pub fn new() -> Self {
        let (cpu, _) = read_system_cpu_mem();
        Self {
            prev_total_cpu: cpu,
            prev_proc_ticks: HashMap::new(),
            prev_io: HashMap::new(),
        }
    }

    pub fn collect_tree(
        &mut self,
        root_pid: i32,
        fg_pgid: Option<i32>,
        inspect_details: bool,
        seconds_since_output: f64,
    ) -> Vec<ProcessNode> {
        let (total_cpu, _) = read_system_cpu_mem();
        let cpu_delta = total_cpu.saturating_sub(self.prev_total_cpu).max(1);
        self.prev_total_cpu = total_cpu;

        let mut all_stats: Vec<ProcStatRaw> = Vec::new();
        if let Ok(entries) = fs::read_dir("/proc") {
            for entry in entries.flatten() {
                if let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<i32>().ok())
                    && let Some(st) = read_proc_stat(pid)
                {
                    all_stats.push(st);
                }
            }
        }

        // Build descendant set rooted at `root_pid`
        let mut by_ppid: HashMap<i32, Vec<i32>> = HashMap::new();
        let mut stat_map: HashMap<i32, ProcStatRaw> = HashMap::new();
        for st in all_stats {
            by_ppid.entry(st.ppid).or_default().push(st.pid);
            stat_map.insert(st.pid, st);
        }

        let mut tree_pids = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        if stat_map.contains_key(&root_pid) {
            queue.push_back(root_pid);
        }
        while let Some(pid) = queue.pop_front() {
            tree_pids.push(pid);
            if let Some(children) = by_ppid.get(&pid) {
                for &c in children {
                    if !tree_pids.contains(&c) {
                        queue.push_back(c);
                    }
                }
            }
        }

        let locks = if inspect_details {
            parse_proc_locks()
        } else {
            Vec::new()
        };

        let mut nodes = Vec::with_capacity(tree_pids.len());
        for pid in tree_pids {
            let Some(st) = stat_map.get(&pid) else {
                continue;
            };
            let key = (st.pid, st.starttime);
            let ticks = st.utime + st.stime;
            let prev_t = self.prev_proc_ticks.insert(key, ticks).unwrap_or(ticks);
            let num_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as f32;
            let cpu_percent =
                ((ticks.saturating_sub(prev_t) as f32 / cpu_delta as f32) * 100.0 * num_cpus)
                    .clamp(0.0, 100.0 * num_cpus);

            let (rchar, wchar, read_bytes, write_bytes) = read_proc_io(pid).unwrap_or((0, 0, 0, 0));
            let io_total = rchar + wchar + read_bytes + write_bytes;
            let prev_io = self.prev_io.insert(key, io_total).unwrap_or(io_total);
            let io_delta = io_total.saturating_sub(prev_io);

            let wchan = if inspect_details {
                fs::read_to_string(format!("/proc/{pid}/wchan"))
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty() && s != "0")
            } else {
                None
            };

            let (syscall_nr, syscall_fd) = if inspect_details {
                fs::read_to_string(format!("/proc/{pid}/syscall"))
                    .ok()
                    .map(|s| {
                        let mut parts = s.split_whitespace();
                        let nr = parts.next().and_then(|v| v.parse::<i64>().ok());
                        let fd = parts.next().and_then(|v| {
                            let stripped = v.strip_prefix("0x").unwrap_or(v);
                            i64::from_str_radix(stripped, 16).ok()
                        });
                        (nr, fd)
                    })
                    .unwrap_or((None, None))
            } else {
                (None, None)
            };

            let fd0_is_tty = if inspect_details {
                fs::read_link(format!("/proc/{pid}/fd/0"))
                    .ok()
                    .is_some_and(|p| {
                        let s = p.to_string_lossy();
                        s.starts_with("/dev/pts/") || s.starts_with("/dev/tty")
                    })
            } else {
                false
            };

            let mut open_fds = Vec::new();
            let mut net_sockets = 0usize;
            if inspect_details && let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) {
                for fd in fds.flatten().take(64) {
                    if let Ok(target) = fs::read_link(fd.path()) {
                        let s = target.to_string_lossy().into_owned();
                        if s.starts_with("socket:") {
                            net_sockets += 1;
                        }
                        open_fds.push(s);
                    }
                }
            }

            let file_lock = locks
                .iter()
                .find(|l| l.blocked && l.pid == pid)
                .map(|blocked_lock| {
                    let owner = locks
                        .iter()
                        .find(|l| !l.blocked && l.inode == blocked_lock.inode);
                    let owner_pid = owner.map(|o| o.pid);
                    let owner_comm = owner_pid.and_then(|op| {
                        stat_map
                            .get(&op)
                            .map(|s| s.comm.clone())
                            .or_else(|| read_proc_stat(op).map(|s| s.comm))
                    });
                    FileLockInfo {
                        path_hint: blocked_lock.inode.clone(),
                        owner_pid,
                        owner_comm,
                        mode: blocked_lock.mode.clone(),
                    }
                });

            let has_children = by_ppid.get(&pid).is_some_and(|v| !v.is_empty());
            let has_tty_fg = fg_pgid.is_some_and(|fg| fg == st.pgid);
            let diagnosis: Diagnosis = classify_wait_reason(&DiagnosisInput {
                state: st.state,
                cpu_percent,
                wchan: wchan.as_deref(),
                syscall_nr,
                syscall_fd,
                fd0_is_tty,
                has_tty_fg,
                has_children,
                io_delta_bytes: io_delta,
                net_sockets,
                file_lock: file_lock.as_ref(),
                seconds_since_output,
            });

            nodes.push(ProcessNode {
                pid: st.pid,
                ppid: st.ppid,
                pgid: st.pgid,
                starttime: st.starttime,
                comm: st.comm.clone(),
                state: st.state,
                cpu_percent,
                rss_bytes: st.rss_bytes,
                threads: st.threads,
                read_bytes,
                write_bytes,
                logical_read_bytes: rchar,
                logical_write_bytes: wchar,
                open_fds,
                net_sockets,
                diagnosis,
            });
        }
        nodes
    }
}

/// Safely perform a process control action using `pidfd_open` BEFORE verifying
/// `(pid, starttime)` identity, and `pidfd_send_signal` without falling back
/// to `kill(2)`, eliminating PID-reuse TOCTOU races.
pub fn execute_process_action(
    pid: i32,
    expected_starttime: u64,
    action: ProcessAction,
) -> Result<(), String> {
    if let ProcessAction::Renice(prio) = action {
        let current =
            read_proc_stat(pid).ok_or_else(|| format!("process {pid} no longer exists"))?;
        if current.starttime != expected_starttime {
            return Err(format!(
                "PID {pid} was reused (starttime mismatch); action aborted for safety"
            ));
        }
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, prio) };
        return if rc == 0 {
            Ok(())
        } else {
            Err(format!("setpriority: {}", std::io::Error::last_os_error()))
        };
    }

    let sig = match action {
        ProcessAction::Pause => libc::SIGSTOP,
        ProcessAction::Resume => libc::SIGCONT,
        ProcessAction::Terminate => libc::SIGTERM,
        ProcessAction::Kill => libc::SIGKILL,
        ProcessAction::Renice(_) => unreachable!(),
    };

    // 1. Pin the kernel `struct pid` with pidfd_open BEFORE checking starttime.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0 as libc::c_uint) };
    if pidfd < 0 {
        return Err(format!(
            "pidfd_open({pid}) failed (refusing unsafe kill fallback): {}",
            std::io::Error::last_os_error()
        ));
    }
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(pidfd as i32) };

    // 2. Verify starttime while holding the pidfd. If the PID was reused before
    //    pidfd_open, starttime will not match and we abort without signaling.
    let current = read_proc_stat(pid).ok_or_else(|| format!("process {pid} no longer exists"))?;
    if current.starttime != expected_starttime {
        return Err(format!(
            "PID {pid} was reused (starttime mismatch); action aborted for safety"
        ));
    }

    // 3. Signal exclusively through pidfd_send_signal. If the process exited
    //    between step 2 and step 3, pidfd_send_signal returns ESRCH and will
    //    never hit a newly-spawned process with the same PID.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0 as libc::c_uint,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "pidfd_send_signal({pid}, {sig}): {}",
            std::io::Error::last_os_error()
        ))
    }
}

use std::os::fd::FromRawFd;
