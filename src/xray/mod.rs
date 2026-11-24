// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Beyrix X-Ray: OS-level process, I/O, and wait-reason diagnostics.

#![allow(dead_code)]

pub mod diagnosis;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod report;
#[cfg(not(target_os = "linux"))]
pub mod windows;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::config::{XRayConfig, XRayLevel};
use crate::events::{AppEvent, EventBus, XRaySummaryPayload};
use crate::render::{Renderer, Scene};
use crate::theme::Theme;
use crate::widgets::ui::{Align, UiColor, UiHitTarget, UiLayer, UiNode, emit_ui_tree};

use diagnosis::{Diagnosis, ProcessStateKind};
use report::{CommandReport, ReportStore};

#[derive(Clone, Debug)]
pub struct XRayCapabilities {
    pub process_tree: bool,
    pub cpu_and_memory: bool,
    pub wait_reason_wchan: bool,
    pub io_counters: bool,
    pub file_locks: bool,
    pub open_fds_and_sockets: bool,
    pub process_control: bool,
    pub trace_syscalls: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessAction {
    Pause,
    Resume,
    Terminate,
    Kill,
    Renice(i32),
}

#[derive(Clone, Debug)]
pub struct ProcessNode {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub starttime: u64,
    pub comm: String,
    pub state: ProcessStateKind,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub threads: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub logical_read_bytes: u64,
    pub logical_write_bytes: u64,
    pub open_fds: Vec<String>,
    pub net_sockets: usize,
    pub diagnosis: Diagnosis,
}

#[derive(Clone, Debug)]
pub struct XRaySnapshot {
    pub level: XRayLevel,
    pub capabilities: XRayCapabilities,
    pub fg_pgid: Option<i32>,
    pub nodes: Vec<ProcessNode>,
    pub total_cpu_percent: f32,
    pub total_rss_bytes: u64,
    pub reports: Vec<CommandReport>,
}

/// Per-`(pid, starttime)` I/O delta accumulator accounting for `proc_pid_io(5)` semantics.
///
/// In Linux, `/proc/[pid]/io` includes the process's own I/O **plus** all
/// waited-for (`waitpid`) children. Alive (unwaited) children are not yet in the
/// parent's `/proc/[ppid]/io`. Summing live tracked processes plus any exited
/// processes whose parent is no longer in the tracked tree avoids double-counting
/// waited-for children while preserving reparented/exited subtrees.
pub type IoCounters = (u64, u64, u64, u64);
type PidKey = (i32, u64);

#[derive(Default)]
pub struct IoAccumulator {
    live: std::collections::HashMap<PidKey, (i32, IoCounters)>,
    unabsorbed_exited: IoCounters,
    pub totals: IoCounters,
}

impl IoAccumulator {
    pub fn reset(&mut self) {
        self.live.clear();
        self.unabsorbed_exited = (0, 0, 0, 0);
        self.totals = (0, 0, 0, 0);
    }

    pub fn update(&mut self, nodes: &[&ProcessNode]) {
        let live_pids: std::collections::HashSet<i32> = nodes.iter().map(|n| n.pid).collect();
        let current_keys: std::collections::HashSet<(i32, u64)> =
            nodes.iter().map(|n| (n.pid, n.starttime)).collect();

        // Any previously-live process that disappeared: walk its recorded ancestor
        // chain (`ppid`). If ANY ancestor is still alive in `live_pids`, that
        // ancestor (or an intermediate parent) will absorb/has absorbed its I/O via
        // waitpid(). Only add to `unabsorbed_exited` if no tracked ancestor remains.
        let disappeared: Vec<PidKey> = self
            .live
            .keys()
            .filter(|k| !current_keys.contains(k))
            .copied()
            .collect();
        let snapshot_live = self.live.clone();
        for key in disappeared {
            if let Some((ppid, last_io)) = self.live.remove(&key) {
                let mut cur_ppid = ppid;
                let mut has_live_ancestor = false;
                for _ in 0..32 {
                    if live_pids.contains(&cur_ppid) {
                        has_live_ancestor = true;
                        break;
                    }
                    if let Some((_, (next_ppid, _))) =
                        snapshot_live.iter().find(|((p, _), _)| *p == cur_ppid)
                    {
                        if *next_ppid == cur_ppid {
                            break;
                        }
                        cur_ppid = *next_ppid;
                    } else {
                        break;
                    }
                }
                if !has_live_ancestor {
                    self.unabsorbed_exited.0 += last_io.0;
                    self.unabsorbed_exited.1 += last_io.1;
                    self.unabsorbed_exited.2 += last_io.2;
                    self.unabsorbed_exited.3 += last_io.3;
                }
            }
        }

        let mut live_sum = (0u64, 0u64, 0u64, 0u64);
        for n in nodes {
            let cur = (
                n.read_bytes,
                n.write_bytes,
                n.logical_read_bytes,
                n.logical_write_bytes,
            );
            self.live.insert((n.pid, n.starttime), (n.ppid, cur));
            live_sum.0 += cur.0;
            live_sum.1 += cur.1;
            live_sum.2 += cur.2;
            live_sum.3 += cur.3;
        }

        let candidate = (
            self.unabsorbed_exited.0 + live_sum.0,
            self.unabsorbed_exited.1 + live_sum.1,
            self.unabsorbed_exited.2 + live_sum.2,
            self.unabsorbed_exited.3 + live_sum.3,
        );
        self.totals.0 = self.totals.0.max(candidate.0);
        self.totals.1 = self.totals.1.max(candidate.1);
        self.totals.2 = self.totals.2.max(candidate.2);
        self.totals.3 = self.totals.3.max(candidate.3);
    }
}

pub struct XRayEngine {
    pub config: XRayConfig,
    pub panel_open: bool,
    snapshot: Arc<Mutex<XRaySnapshot>>,
    last_output_at: Arc<Mutex<Instant>>,
    level: Arc<Mutex<XRayLevel>>,
    dirty: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl XRayEngine {
    pub fn new(config: XRayConfig) -> Self {
        #[cfg(target_os = "linux")]
        let caps = linux::capabilities();
        #[cfg(not(target_os = "linux"))]
        let caps = windows::capabilities();

        let level = Arc::new(Mutex::new(config.level));
        let snapshot = Arc::new(Mutex::new(XRaySnapshot {
            level: config.level,
            capabilities: caps,
            fg_pgid: None,
            nodes: Vec::new(),
            total_cpu_percent: 0.0,
            total_rss_bytes: 0,
            reports: Vec::new(),
        }));
        Self {
            config,
            panel_open: false,
            snapshot,
            last_output_at: Arc::new(Mutex::new(Instant::now())),
            level,
            dirty: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    pub fn notify_terminal_output(&self) {
        if self.config.enabled {
            *self
                .last_output_at
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Instant::now();
        }
    }

    pub fn toggle_panel(&mut self) {
        self.panel_open = !self.panel_open;
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn set_level(&mut self, level: XRayLevel) {
        self.config.level = level;
        *self.level.lock().unwrap_or_else(|e| e.into_inner()) = level;
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn ensure_started(&mut self, root_pid: i32, pty_master_fd: Option<i32>, bus: EventBus) {
        if !self.config.enabled || self.worker.is_some() {
            return;
        }
        let snapshot = Arc::clone(&self.snapshot);
        let last_out = Arc::clone(&self.last_output_at);
        let level_shared = Arc::clone(&self.level);
        let dirty_flag = Arc::clone(&self.dirty);
        let shutdown = Arc::clone(&self.shutdown);
        let interval = Duration::from_millis(self.config.poll_interval_ms.max(50));
        let history_limit = self.config.history_limit;
        let privacy = self.config.privacy_mode;

        self.worker = std::thread::Builder::new()
            .name("beyrix-xray".to_string())
            .spawn(move || {
                #[cfg(target_os = "linux")]
                let mut sampler = linux::LinuxSampler::new();
                #[cfg(not(target_os = "linux"))]
                let mut sampler = windows::WindowsSampler::new();

                let mut reports = ReportStore::new(history_limit, privacy);
                let mut prev_fg: Option<i32> = None;
                let mut cmd_started = Instant::now();
                let mut cmd_wall = SystemTime::now();
                let mut cmd_name = String::new();
                let mut peak_rss = 0u64;
                let mut peak_procs = 0usize;
                let mut io_acc = IoAccumulator::default();
                let mut wait_events: Vec<String> = Vec::new();
                let mut cmd_seq = 1u64;

                while !shutdown.load(Ordering::Relaxed) {
                    let lvl = *level_shared.lock().unwrap_or_else(|e| e.into_inner());
                    let inspect = matches!(lvl, XRayLevel::Inspect | XRayLevel::Trace);
                    #[cfg(target_os = "linux")]
                    let fg_pgid = linux::foreground_pgid(pty_master_fd);
                    #[cfg(not(target_os = "linux"))]
                    let fg_pgid = None;

                    let secs_since_out = last_out
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .elapsed()
                        .as_secs_f64();

                    let nodes = sampler.collect_tree(root_pid, fg_pgid, inspect, secs_since_out);
                    let total_cpu: f32 = nodes.iter().map(|n| n.cpu_percent).sum();
                    let total_rss: u64 = nodes.iter().map(|n| n.rss_bytes).sum();

                    if let Some(fg) = fg_pgid {
                        let cmd_nodes: Vec<&ProcessNode> =
                            nodes.iter().filter(|n| n.pgid == fg).collect();
                        let cmd_rss: u64 = cmd_nodes.iter().map(|n| n.rss_bytes).sum();
                        let cmd_procs = cmd_nodes.len();

                        if prev_fg != Some(fg) {
                            if let Some(old_fg) = prev_fg
                                && !cmd_name.is_empty()
                                && old_fg != root_pid
                            {
                                reports.push(CommandReport {
                                    command_id: cmd_seq,
                                    command: cmd_name.clone(),
                                    exit_code: None,
                                    signal: None,
                                    started_at: cmd_wall,
                                    duration_ms: cmd_started.elapsed().as_millis() as u64,
                                    peak_rss_bytes: peak_rss,
                                    peak_process_count: peak_procs,
                                    physical_read_bytes: io_acc.totals.0,
                                    physical_write_bytes: io_acc.totals.1,
                                    logical_read_bytes: io_acc.totals.2,
                                    logical_write_bytes: io_acc.totals.3,
                                    wait_events: std::mem::take(&mut wait_events),
                                    trace_active: false,
                                    io_is_approximate: true,
                                });
                                cmd_seq += 1;
                            }
                            prev_fg = Some(fg);
                            cmd_started = Instant::now();
                            cmd_wall = SystemTime::now();
                            peak_rss = cmd_rss;
                            peak_procs = cmd_procs;
                            io_acc.reset();
                            io_acc.update(&cmd_nodes);
                            wait_events.clear();
                            cmd_name = cmd_nodes
                                .first()
                                .map(|n| n.comm.clone())
                                .unwrap_or_else(|| format!("pgid:{fg}"));
                        } else {
                            peak_rss = peak_rss.max(cmd_rss);
                            peak_procs = peak_procs.max(cmd_procs);
                            io_acc.update(&cmd_nodes);
                            for n in &cmd_nodes {
                                if !matches!(
                                    n.diagnosis.reason,
                                    diagnosis::WaitReason::ActiveCompute
                                        | diagnosis::WaitReason::Unknown
                                ) {
                                    let label = n.diagnosis.reason.label().to_string();
                                    if !wait_events.contains(&label) && wait_events.len() < 16 {
                                        wait_events.push(label);
                                    }
                                }
                            }
                        }
                    }

                    if let Some(focus_node) = select_focus_process(&nodes, fg_pgid) {
                        bus.publish(AppEvent::XRayUpdate(XRaySummaryPayload {
                            root_pid,
                            process_count: nodes.len(),
                            cpu_percent: total_cpu,
                            rss_bytes: total_rss,
                            state_label: focus_node.state.label().to_string(),
                            waiting_reason: focus_node.diagnosis.reason.label().to_string(),
                        }));
                    }

                    {
                        let mut snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                        snap.level = lvl;
                        snap.fg_pgid = fg_pgid;
                        snap.nodes = nodes;
                        snap.total_cpu_percent = total_cpu;
                        snap.total_rss_bytes = total_rss;
                        snap.reports = reports.list();
                    }
                    dirty_flag.store(true, Ordering::Relaxed);

                    std::thread::sleep(interval);
                }
            })
            .ok();
    }

    pub fn snapshot(&self) -> XRaySnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn has_known_tui_foreground(&self) -> bool {
        const TUI_NAMES: &[&str] = &[
            "vim", "nvim", "vi", "nano", "htop", "btop", "top", "tmux", "screen", "less", "man",
            "mc", "lazygit", "tig", "ranger", "yazi",
        ];
        let snap = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        snap.nodes.iter().any(|n| {
            let c = n.comm.to_ascii_lowercase();
            TUI_NAMES.contains(&c.as_str())
        })
    }

    /// Render compact indicator and (if open) full X-Ray panel into `Scene` via BeyrixWidgets UI.
    pub fn emit_to_scene(
        &self,
        win_size: (f32, f32),
        scale: f32,
        theme: &Theme,
        renderer: &mut Renderer,
        scene: &mut Scene,
        hits: &mut Vec<UiHitTarget>,
    ) {
        if !self.config.enabled {
            return;
        }
        let snap = self.snapshot();

        if self.config.compact_indicator {
            let focus = select_focus_process(&snap.nodes, snap.fg_pgid);
            let status_text = if let Some(last) = focus {
                format!(
                    "X-Ray [{}] {} · {:.1}% CPU · {} MB · {}",
                    snap.level.name(),
                    last.comm,
                    snap.total_cpu_percent,
                    snap.total_rss_bytes / (1024 * 1024),
                    last.diagnosis.reason.label()
                )
            } else {
                format!("X-Ray [{}] idle", snap.level.name())
            };
            let badge = UiNode::Container {
                padding: 6.0,
                bg: Some(UiColor::Token("card".into())),
                border: Some(UiColor::Token("cyan".into())),
                radius: 6.0,
                width: None,
                height: None,
                child: Box::new(UiNode::Text {
                    text: status_text,
                    size: Some(11.5),
                    color: Some(UiColor::Token("fg".into())),
                    align: Align::Start,
                }),
            };
            let w = (360.0 * scale).min(win_size.0 - 24.0);
            let h = 28.0 * scale;
            emit_ui_tree(
                "xray-indicator",
                &badge,
                (12.0 * scale, (win_size.1 - h - 10.0 * scale).max(0.0), w, h),
                scale,
                0.92,
                UiLayer::Overlay,
                theme,
                renderer,
                scene,
                hits,
            );
        }

        if !self.panel_open {
            return;
        }

        let mut rows = vec![UiNode::Row {
            gap: 8.0,
            align: Align::Center,
            children: vec![
                UiNode::Icon {
                    name: "xray".into(),
                    size: Some(15.0),
                    color: Some(UiColor::Token("cyan".into())),
                },
                UiNode::Text {
                    text: format!("Beyrix X-Ray ({})", snap.level.name()),
                    size: Some(14.0),
                    color: Some(UiColor::Token("fg".into())),
                    align: Align::Start,
                },
                UiNode::Button {
                    id: "xray-level-light".into(),
                    label: "Light".into(),
                    bg: None,
                    color: None,
                    width: Some(54.0),
                    height: Some(22.0),
                    radius: 4.0,
                },
                UiNode::Button {
                    id: "xray-level-inspect".into(),
                    label: "Inspect".into(),
                    bg: None,
                    color: None,
                    width: Some(64.0),
                    height: Some(22.0),
                    radius: 4.0,
                },
            ],
        }];

        for node in snap.nodes.iter().take(6) {
            rows.push(UiNode::Text {
                text: format!(
                    "PID {} ({}) · {} · {:.1}% CPU · {} MB · {}",
                    node.pid,
                    node.comm,
                    node.state.label(),
                    node.cpu_percent,
                    node.rss_bytes / (1024 * 1024),
                    node.diagnosis.summary
                ),
                size: Some(11.5),
                color: Some(UiColor::Token("fg".into())),
                align: Align::Start,
            });
            rows.push(UiNode::Text {
                text: format!(
                    "  Confidence: {} | Source: {} | Limit: {}",
                    node.diagnosis.confidence.label(),
                    node.diagnosis.source,
                    node.diagnosis.limitations
                ),
                size: Some(10.5),
                color: Some(UiColor::Token("muted".into())),
                align: Align::Start,
            });
        }

        let panel = UiNode::Container {
            padding: 12.0,
            bg: Some(UiColor::Token("card".into())),
            border: Some(UiColor::Token("cyan".into())),
            radius: 10.0,
            width: None,
            height: None,
            child: Box::new(UiNode::Column {
                gap: 6.0,
                align: Align::Stretch,
                children: rows,
            }),
        };

        let pw = (640.0 * scale).min(win_size.0 - 32.0);
        let ph = (320.0 * scale).min(win_size.1 - 64.0);
        emit_ui_tree(
            "xray-panel",
            &panel,
            (16.0 * scale, 16.0 * scale, pw, ph),
            scale,
            0.96,
            UiLayer::Overlay,
            theme,
            renderer,
            scene,
            hits,
        );
    }
}

impl Drop for XRayEngine {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Select the most diagnostic-relevant process in the foreground job (prioritizing
/// blocked/waiting processes, then highest CPU, never an unrelated background process).
pub fn select_focus_process(nodes: &[ProcessNode], fg_pgid: Option<i32>) -> Option<&ProcessNode> {
    let candidates: Vec<&ProcessNode> = match fg_pgid {
        Some(fg) => {
            let fg_nodes: Vec<&ProcessNode> = nodes.iter().filter(|n| n.pgid == fg).collect();
            if fg_nodes.is_empty() {
                nodes.iter().collect()
            } else {
                fg_nodes
            }
        }
        None => nodes.iter().collect(),
    };
    candidates.into_iter().max_by(|a, b| {
        let priority = |n: &ProcessNode| -> u8 {
            match n.diagnosis.reason {
                diagnosis::WaitReason::WaitingFileLock => 6,
                diagnosis::WaitReason::PossibleDeadlock => 5,
                diagnosis::WaitReason::WaitingStdin => 4,
                diagnosis::WaitReason::WaitingDiskIo | diagnosis::WaitReason::WaitingNetwork => 3,
                diagnosis::WaitReason::Stopped | diagnosis::WaitReason::Zombie => 2,
                diagnosis::WaitReason::ActiveCompute => 1,
                _ => 0,
            }
        };
        priority(a)
            .cmp(&priority(b))
            .then_with(|| a.cpu_percent.total_cmp(&b.cpu_percent))
            .then_with(|| a.pid.cmp(&b.pid))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn disabled_xray_spawns_no_worker() {
        let mut engine = XRayEngine::new(XRayConfig::default());
        assert!(!engine.config.enabled);
        engine.ensure_started(unsafe { libc::getpid() }, None, EventBus::new());
        assert!(engine.worker.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_backend_observes_real_sleeping_stopped_and_cpu_processes() {
        let mut child = Command::new("sleep")
            .arg("5")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(60));

        let mut sampler = linux::LinuxSampler::new();
        let tree = sampler.collect_tree(pid, None, true, 1.0);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].pid, pid);
        assert!(tree[0].rss_bytes > 0);

        // Pause via safe PID+starttime-verified action
        let starttime = tree[0].starttime;
        linux::execute_process_action(pid, starttime, ProcessAction::Pause).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let stopped_tree = sampler.collect_tree(pid, None, true, 1.0);
        assert_eq!(stopped_tree[0].state, ProcessStateKind::Stopped);
        assert_eq!(
            stopped_tree[0].diagnosis.reason,
            diagnosis::WaitReason::Stopped
        );

        // Wrong starttime must abort safely (PID-reuse protection)
        assert!(linux::execute_process_action(pid, starttime + 999, ProcessAction::Kill).is_err());

        // Resume and terminate cleanly
        linux::execute_process_action(pid, starttime, ProcessAction::Resume).unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_backend_detects_real_file_lock_contention() {
        let dir = tempfile::tempdir().unwrap();
        let lock_file = dir.path().join("cargo.lock");
        std::fs::write(&lock_file, b"").unwrap();

        let mut holder = Command::new("flock")
            .args([lock_file.to_str().unwrap(), "sleep", "4"])
            .spawn()
            .expect("spawn flock holder");
        std::thread::sleep(Duration::from_millis(80));

        let mut waiter = Command::new("flock")
            .args([lock_file.to_str().unwrap(), "true"])
            .spawn()
            .expect("spawn flock waiter");
        std::thread::sleep(Duration::from_millis(100));

        let mut sampler = linux::LinuxSampler::new();
        let tree = sampler.collect_tree(waiter.id() as i32, None, true, 1.0);
        assert_eq!(tree.len(), 1);
        assert_eq!(
            tree[0].diagnosis.reason,
            diagnosis::WaitReason::WaitingFileLock
        );

        let _ = waiter.kill();
        let _ = waiter.wait();
        let _ = holder.kill();
        let _ = holder.wait();
    }

    #[test]
    fn select_focus_process_prioritizes_blocked_fg_over_idle_last_node() {
        let mk = |pid: i32, pgid: i32, reason: diagnosis::WaitReason, cpu: f32| ProcessNode {
            pid,
            ppid: 1,
            pgid,
            starttime: 100,
            comm: format!("proc-{pid}"),
            state: ProcessStateKind::Sleeping,
            cpu_percent: cpu,
            rss_bytes: 1024,
            threads: 1,
            read_bytes: 0,
            write_bytes: 0,
            logical_read_bytes: 0,
            logical_write_bytes: 0,
            open_fds: Vec::new(),
            net_sockets: 0,
            diagnosis: Diagnosis {
                reason,
                summary: String::new(),
                evidence: Vec::new(),
                source: String::new(),
                confidence: diagnosis::Confidence::High,
                limitations: String::new(),
                updated_at: Instant::now(),
            },
        };
        let nodes = vec![
            mk(10, 20, diagnosis::WaitReason::WaitingFileLock, 0.0),
            mk(11, 20, diagnosis::WaitReason::ActiveCompute, 12.0),
            mk(12, 99, diagnosis::WaitReason::NoObservedActivity, 0.0),
        ];
        let focus = select_focus_process(&nodes, Some(20)).unwrap();
        assert_eq!(focus.pid, 10);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn io_accumulator_preserves_exited_child_io_and_renice_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let out_file = dir.path().join("io_test.bin");
        // Parent bash spawns a child `dd` that writes 1 MiB and exits while parent stays alive.
        let script = format!(
            "dd if=/dev/zero of={} bs=65536 count=16 conv=fdatasync 2>/dev/null; sleep 2",
            out_file.display()
        );
        let mut parent = Command::new("bash")
            .args(["-c", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bash");
        let root_pid = parent.id() as i32;

        let mut sampler = linux::LinuxSampler::new();
        let mut acc = IoAccumulator::default();
        for _ in 0..12 {
            let tree = sampler.collect_tree(root_pid, None, false, 1.0);
            let refs: Vec<&ProcessNode> = tree.iter().collect();
            acc.update(&refs);
            std::thread::sleep(Duration::from_millis(35));
        }
        // Exited child's 1 MiB write must be preserved in logical_write_bytes (totals.3)
        // and not double-counted across samples.
        assert!(
            acc.totals.3 >= 1_048_576,
            "expected >= 1 MiB logical write, got {}",
            acc.totals.3
        );
        assert!(
            acc.totals.3 < 2 * 1_048_576,
            "suspected double-count: got {}",
            acc.totals.3
        );

        let tree = sampler.collect_tree(root_pid, None, false, 1.0);
        if let Some(node) = tree.first() {
            assert!(
                linux::execute_process_action(node.pid, node.starttime, ProcessAction::Renice(5))
                    .is_err()
            );
        }

        let _ = parent.kill();
        let _ = parent.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn io_accumulator_does_not_double_count_waited_for_child() {
        use std::io::{BufRead, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let out_file = dir.path().join("det_io.bin");
        let pipe_go = dir.path().join("go.fifo");
        let pipe_done = dir.path().join("done.fifo");
        let pipe_exit = dir.path().join("exit.fifo");

        // Create named pipes for deterministic synchronization:
        // 1. Child writes 1 MiB, signals READY on stdout, waits on `go.fifo`.
        // 2. Test samples BOTH parent and child while child is alive.
        // 3. Test writes to `go.fifo` -> child exits -> parent waitpids -> parent prints WAITED.
        // 4. Test samples parent AFTER waitpid, then writes to `exit.fifo` so parent exits.
        for p in [&pipe_go, &pipe_done, &pipe_exit] {
            let c_p = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c_p.as_ptr(), 0o600) }, 0);
        }

        let py = format!(
            r#"
import os, sys
pid = os.fork()
if pid == 0:
    with open({out:?}, 'wb') as f:
        f.write(b'A' * 1048576)
        f.flush()
        os.fsync(f.fileno())
    with open({done:?}, 'w') as d:
        d.write('READY\n')
        d.flush()
    with open({go:?}, 'r') as g:
        g.read()
    os._exit(0)
else:
    with open({done:?}, 'r') as d:
        d.readline()
    print('CHILD_ALIVE', flush=True)
    os.waitpid(pid, 0)
    print('CHILD_WAITED', flush=True)
    with open({exit_p:?}, 'r') as e:
        e.read()
"#,
            out = out_file.to_str().unwrap(),
            done = pipe_done.to_str().unwrap(),
            go = pipe_go.to_str().unwrap(),
            exit_p = pipe_exit.to_str().unwrap(),
        );

        let mut parent = Command::new("python3")
            .args(["-c", &py])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn python3");
        let root_pid = parent.id() as i32;
        let mut stdout = BufReader::new(parent.stdout.take().unwrap());

        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "CHILD_ALIVE");

        let mut sampler = linux::LinuxSampler::new();
        let mut acc = IoAccumulator::default();

        // Sample 1: BOTH parent and child are alive!
        let tree1 = sampler.collect_tree(root_pid, None, false, 1.0);
        assert_eq!(tree1.len(), 2, "expected parent + alive child");
        let refs1: Vec<&ProcessNode> = tree1.iter().collect();
        acc.update(&refs1);

        // Allow child to exit and parent to waitpid()
        std::fs::write(&pipe_go, b"GO\n").unwrap();
        line.clear();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "CHILD_WAITED");

        // Sample 2: Only parent is alive, and parent's /proc/<pid>/io now includes child's 1 MiB!
        let tree2 = sampler.collect_tree(root_pid, None, false, 1.0);
        assert_eq!(tree2.len(), 1, "expected only parent after waitpid");
        let refs2: Vec<&ProcessNode> = tree2.iter().collect();
        acc.update(&refs2);

        // Release parent
        let _ = std::fs::write(&pipe_exit, b"EXIT\n");
        let _ = parent.wait();

        // Must be ~1 MiB, NEVER ~2 MiB!
        assert!(
            acc.totals.3 >= 1_048_576,
            "expected >= 1 MiB logical write, got {}",
            acc.totals.3
        );
        assert!(
            acc.totals.3 < 1_500_000,
            "double-counted waited-for child I/O: got {} bytes",
            acc.totals.3
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn io_accumulator_does_not_double_count_three_generation_waitpid() {
        use std::io::{BufRead, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let out_file = dir.path().join("gen3_io.bin");
        let pipe_go = dir.path().join("go3.fifo");
        let pipe_ready = dir.path().join("ready3.fifo");
        let pipe_exit = dir.path().join("exit3.fifo");
        for p in [&pipe_go, &pipe_ready, &pipe_exit] {
            let c_p = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c_p.as_ptr(), 0o600) }, 0);
        }

        // A -> B -> C: C writes 1 MiB and blocks; after Sample 1, C exits,
        // B waitpid(C) and exits, A waitpid(B); Sample 2 sees only A.
        let py = format!(
            r#"
import os
pid_b = os.fork()
if pid_b == 0:
    pid_c = os.fork()
    if pid_c == 0:
        with open({out:?}, 'wb') as f:
            f.write(b'B' * 1048576)
            f.flush()
            os.fsync(f.fileno())
        with open({ready:?}, 'w') as r:
            r.write('READY\n')
            r.flush()
        with open({go:?}, 'r') as g:
            g.read()
        os._exit(0)
    else:
        os.waitpid(pid_c, 0)
        os._exit(0)
else:
    with open({ready:?}, 'r') as r:
        r.readline()
    print('TREE_ALIVE', flush=True)
    os.waitpid(pid_b, 0)
    print('AB_WAITED', flush=True)
    with open({exit_p:?}, 'r') as e:
        e.read()
"#,
            out = out_file.to_str().unwrap(),
            ready = pipe_ready.to_str().unwrap(),
            go = pipe_go.to_str().unwrap(),
            exit_p = pipe_exit.to_str().unwrap(),
        );

        let mut parent = Command::new("python3")
            .args(["-c", &py])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn python3");
        let root_pid = parent.id() as i32;
        let mut stdout = BufReader::new(parent.stdout.take().unwrap());

        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "TREE_ALIVE");

        let mut sampler = linux::LinuxSampler::new();
        let mut acc = IoAccumulator::default();

        // Sample 1: A, B, and C are all alive.
        let tree1 = sampler.collect_tree(root_pid, None, false, 1.0);
        assert_eq!(tree1.len(), 3, "expected A, B, and C alive");
        let refs1: Vec<&ProcessNode> = tree1.iter().collect();
        acc.update(&refs1);

        // Allow C to exit -> B waitpid(C) & exit -> A waitpid(B).
        std::fs::write(&pipe_go, b"GO\n").unwrap();
        line.clear();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "AB_WAITED");

        // Sample 2: Only A is alive (absorbing B+C's 1 MiB).
        let tree2 = sampler.collect_tree(root_pid, None, false, 1.0);
        assert_eq!(tree2.len(), 1, "expected only A alive after waitpid(B)");
        let refs2: Vec<&ProcessNode> = tree2.iter().collect();
        acc.update(&refs2);

        let _ = std::fs::write(&pipe_exit, b"EXIT\n");
        let _ = parent.wait();

        assert!(acc.totals.3 >= 1_048_576);
        assert!(
            acc.totals.3 < 1_500_000,
            "double-counted 3-generation waited-for I/O: got {} bytes",
            acc.totals.3
        );
    }

    #[test]
    fn xray_take_dirty_is_independent_of_widgets() {
        let mut engine = XRayEngine::new(XRayConfig {
            enabled: true,
            ..Default::default()
        });
        assert!(!engine.take_dirty());
        engine.toggle_panel();
        assert!(engine.take_dirty());
        assert!(!engine.take_dirty());
        engine.set_level(XRayLevel::Inspect);
        assert!(engine.take_dirty());
    }
}
