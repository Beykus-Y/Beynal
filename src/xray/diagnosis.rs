// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wait-reason classification and evidence-backed diagnostics for Beyrix X-Ray.
//!
//! Every diagnostic conclusion includes observed state, supporting evidence,
//! data source, confidence level, known limitations, and update timestamp.
//! Heuristics are never presented as established facts.

use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessStateKind {
    Running,
    Sleeping,
    DiskSleep,
    Stopped,
    Zombie,
    Idle,
    Unknown,
}

impl ProcessStateKind {
    pub fn from_proc_char(c: char) -> Self {
        match c {
            'R' => Self::Running,
            'S' => Self::Sleeping,
            'D' => Self::DiskSleep,
            'T' | 't' => Self::Stopped,
            'Z' => Self::Zombie,
            'I' => Self::Idle,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Sleeping => "Sleeping",
            Self::DiskSleep => "Disk/IO Sleep (D)",
            Self::Stopped => "Stopped",
            Self::Zombie => "Zombie",
            Self::Idle => "Idle",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitReason {
    ActiveCompute,
    WaitingStdin,
    WaitingChild,
    WaitingFileLock,
    WaitingSync,
    WaitingNetwork,
    WaitingDiskIo,
    Stopped,
    Zombie,
    PossibleDeadlock,
    NoObservedActivity,
    Unknown,
}

impl WaitReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::ActiveCompute => "Active computation",
            Self::WaitingStdin => "Waiting for terminal/stdin input",
            Self::WaitingChild => "Waiting for child process",
            Self::WaitingFileLock => "Blocked on file lock",
            Self::WaitingSync => "Waiting on synchronization/futex",
            Self::WaitingNetwork => "Waiting on network/socket I/O",
            Self::WaitingDiskIo => "Waiting on disk I/O",
            Self::Stopped => "Stopped (SIGSTOP/SIGTSTP)",
            Self::Zombie => "Zombie (awaiting parent wait)",
            Self::PossibleDeadlock => "Possible deadlock",
            Self::NoObservedActivity => "Sleeping / no observed CPU or I/O activity",
            Self::Unknown => "Unknown wait state",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
    Unknown,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Self::High => "High",
            Self::Medium => "Medium",
            Self::Low => "Low",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileLockInfo {
    pub path_hint: String,
    pub owner_pid: Option<i32>,
    pub owner_comm: Option<String>,
    pub mode: String,
}

#[derive(Clone, Debug)]
pub struct Diagnosis {
    pub reason: WaitReason,
    pub summary: String,
    pub evidence: Vec<String>,
    pub source: String,
    pub confidence: Confidence,
    pub limitations: String,
    pub updated_at: Instant,
}

pub struct DiagnosisInput<'a> {
    pub state: ProcessStateKind,
    pub cpu_percent: f32,
    pub wchan: Option<&'a str>,
    pub syscall_nr: Option<i64>,
    pub has_tty_fg: bool,
    pub has_children: bool,
    pub io_delta_bytes: u64,
    pub net_sockets: usize,
    pub file_lock: Option<&'a FileLockInfo>,
    pub seconds_since_output: f64,
}

/// Classify why a process is currently executing or waiting, based strictly on
/// observed OS signals.
pub fn classify_wait_reason(input: &DiagnosisInput<'_>) -> Diagnosis {
    let now = Instant::now();

    if input.state == ProcessStateKind::Zombie {
        return Diagnosis {
            reason: WaitReason::Zombie,
            summary: "Process exited and is awaiting parent waitpid()".into(),
            evidence: vec!["/proc/[pid]/stat state = Z".into()],
            source: "procfs stat".into(),
            confidence: Confidence::High,
            limitations: "Process is already terminated".into(),
            updated_at: now,
        };
    }

    if input.state == ProcessStateKind::Stopped {
        return Diagnosis {
            reason: WaitReason::Stopped,
            summary: "Process is stopped by signal (e.g. Ctrl+Z / SIGSTOP)".into(),
            evidence: vec!["/proc/[pid]/stat state = T".into()],
            source: "procfs stat".into(),
            confidence: Confidence::High,
            limitations: "Use SIGCONT / Resume to continue execution".into(),
            updated_at: now,
        };
    }

    if let Some(lock) = input.file_lock {
        let owner_desc = match (lock.owner_pid, &lock.owner_comm) {
            (Some(pid), Some(comm)) => format!("held by PID {pid} ({comm})"),
            (Some(pid), None) => format!("held by PID {pid}"),
            _ => "owner PID unavailable".to_string(),
        };
        return Diagnosis {
            reason: WaitReason::WaitingFileLock,
            summary: format!("Blocked waiting for {} lock ({owner_desc})", lock.mode),
            evidence: vec![
                format!(
                    "/proc/locks: blocked {} lock on {}",
                    lock.mode, lock.path_hint
                ),
                format!("Lock holder: {owner_desc}"),
            ],
            source: "/proc/locks + procfs fd".into(),
            confidence: Confidence::High,
            limitations: "Kernel lock table is a point-in-time snapshot".into(),
            updated_at: now,
        };
    }

    if input.state == ProcessStateKind::Running || input.cpu_percent >= 5.0 {
        return Diagnosis {
            reason: WaitReason::ActiveCompute,
            summary: format!(
                "Actively computing ({:.1}% CPU, {:.0}s since last output)",
                input.cpu_percent, input.seconds_since_output
            ),
            evidence: vec![format!(
                "state={}, cpu={:.1}%",
                input.state.label(),
                input.cpu_percent
            )],
            source: "procfs stat".into(),
            confidence: Confidence::High,
            limitations: "User-space loops without stdout still show CPU activity".into(),
            updated_at: now,
        };
    }

    if input.state == ProcessStateKind::DiskSleep {
        return Diagnosis {
            reason: WaitReason::WaitingDiskIo,
            summary: "Uninterruptible sleep (D) in kernel I/O".into(),
            evidence: vec![
                "state = D (uninterruptible)".into(),
                format!("wchan = {}", input.wchan.unwrap_or("unavailable")),
            ],
            source: "procfs stat + wchan".into(),
            confidence: Confidence::High,
            limitations: "D-state processes cannot handle signals until I/O completes".into(),
            updated_at: now,
        };
    }

    if let Some(wchan) = input.wchan {
        let w = wchan.trim();
        if w.contains("wait_woken") || w.contains("n_tty_read") || w.contains("tty_read") {
            return Diagnosis {
                reason: WaitReason::WaitingStdin,
                summary: "Waiting for user input on terminal (tty_read)".into(),
                evidence: vec![
                    format!("wchan = {w}"),
                    format!("fg_pgid = {}", input.has_tty_fg),
                ],
                source: "procfs wchan".into(),
                confidence: Confidence::High,
                limitations: "Expects input on stdin".into(),
                updated_at: now,
            };
        }
        if w.contains("do_wait") || w.contains("wait4") || w.contains("waitid") {
            return Diagnosis {
                reason: WaitReason::WaitingChild,
                summary: "Parent process is waiting for child process(es) to exit".into(),
                evidence: vec![
                    format!("wchan = {w}"),
                    format!("has_children = {}", input.has_children),
                ],
                source: "procfs wchan".into(),
                confidence: Confidence::High,
                limitations: "Inspect child processes in the tree for root cause".into(),
                updated_at: now,
            };
        }
        if w.contains("futex") {
            return Diagnosis {
                reason: WaitReason::WaitingSync,
                summary: "Blocked on user-space mutex/condvar (futex)".into(),
                evidence: vec![format!("wchan = {w}")],
                source: "procfs wchan".into(),
                confidence: Confidence::Medium,
                limitations: "User-space lock owner cannot be determined without ptrace/debuginfo"
                    .into(),
                updated_at: now,
            };
        }
        if w.contains("flock") || w.contains("locks_lock") || w.contains("posix_lock") {
            return Diagnosis {
                reason: WaitReason::WaitingFileLock,
                summary: "Waiting in kernel file-lock syscall".into(),
                evidence: vec![format!("wchan = {w}")],
                source: "procfs wchan".into(),
                confidence: Confidence::Medium,
                limitations: "Exact lock owner not matched in /proc/locks snapshot".into(),
                updated_at: now,
            };
        }
        if w.contains("sk_wait")
            || w.contains("tcp_")
            || w.contains("inet_")
            || w.contains("unix_stream")
            || (input.net_sockets > 0 && (w.contains("ep_poll") || w.contains("do_sys_poll")))
        {
            return Diagnosis {
                reason: WaitReason::WaitingNetwork,
                summary: format!(
                    "Waiting on network/socket I/O ({} open sockets)",
                    input.net_sockets
                ),
                evidence: vec![
                    format!("wchan = {w}"),
                    format!("sockets = {}", input.net_sockets),
                ],
                source: "procfs wchan + fd".into(),
                confidence: Confidence::Medium,
                limitations: "Poll/epoll may also watch pipes or timers".into(),
                updated_at: now,
            };
        }
        if w.contains("hrtimer") || w.contains("nanosleep") || w.contains("schedule_timeout") {
            return Diagnosis {
                reason: WaitReason::NoObservedActivity,
                summary: "Sleeping on timer (nanosleep / timeout)".into(),
                evidence: vec![format!("wchan = {w}")],
                source: "procfs wchan".into(),
                confidence: Confidence::High,
                limitations: "Intentional sleep or polling backoff".into(),
                updated_at: now,
            };
        }
    }

    // Fallback when wchan is restricted (e.g., Yama/hidepid) or generic poll
    if input.syscall_nr == Some(0) && input.has_tty_fg {
        return Diagnosis {
            reason: WaitReason::WaitingStdin,
            summary: "Blocked in read(0) while holding foreground terminal".into(),
            evidence: vec!["syscall = 0 (read)".into(), "fg_pgid = true".into()],
            source: "procfs syscall".into(),
            confidence: Confidence::Medium,
            limitations: "FD 0 could be a pipe if redirected".into(),
            updated_at: now,
        };
    }

    if input.io_delta_bytes > 0 {
        return Diagnosis {
            reason: WaitReason::WaitingDiskIo,
            summary: "Performing I/O with low CPU".into(),
            evidence: vec![format!("io_delta_bytes = {}", input.io_delta_bytes)],
            source: "procfs io".into(),
            confidence: Confidence::Low,
            limitations: "Includes page cache reads/writes".into(),
            updated_at: now,
        };
    }

    Diagnosis {
        reason: WaitReason::Unknown,
        summary: "Sleeping; exact wait reason unavailable under current permissions".into(),
        evidence: vec![
            format!("state = {}", input.state.label()),
            format!(
                "wchan = {}",
                input.wchan.unwrap_or("restricted/unavailable")
            ),
        ],
        source: "procfs".into(),
        confidence: Confidence::Unknown,
        limitations: "wchan/syscall may be restricted by kernel or Yama ptrace_scope".into(),
        updated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_active_compute_stdin_child_and_file_lock() {
        let compute = classify_wait_reason(&DiagnosisInput {
            state: ProcessStateKind::Running,
            cpu_percent: 92.0,
            wchan: None,
            syscall_nr: None,
            has_tty_fg: true,
            has_children: false,
            io_delta_bytes: 0,
            net_sockets: 0,
            file_lock: None,
            seconds_since_output: 12.0,
        });
        assert_eq!(compute.reason, WaitReason::ActiveCompute);

        let stdin = classify_wait_reason(&DiagnosisInput {
            state: ProcessStateKind::Sleeping,
            cpu_percent: 0.0,
            wchan: Some("n_tty_read"),
            syscall_nr: Some(0),
            has_tty_fg: true,
            has_children: false,
            io_delta_bytes: 0,
            net_sockets: 0,
            file_lock: None,
            seconds_since_output: 5.0,
        });
        assert_eq!(stdin.reason, WaitReason::WaitingStdin);

        let lock_info = FileLockInfo {
            path_hint: "/tmp/target/.cargo-lock".into(),
            owner_pid: Some(4242),
            owner_comm: Some("rust-analyzer".into()),
            mode: "FLOCK WRITE".into(),
        };
        let locked = classify_wait_reason(&DiagnosisInput {
            state: ProcessStateKind::Sleeping,
            cpu_percent: 0.0,
            wchan: Some("flock_lock_inode_wait"),
            syscall_nr: None,
            has_tty_fg: true,
            has_children: false,
            io_delta_bytes: 0,
            net_sockets: 0,
            file_lock: Some(&lock_info),
            seconds_since_output: 3.0,
        });
        assert_eq!(locked.reason, WaitReason::WaitingFileLock);
        assert!(locked.summary.contains("4242"));
    }
}
