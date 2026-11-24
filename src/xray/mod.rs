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
use crate::widgets::ui::{
    Align, HitKind, UiColor, UiHitTarget, UiLayer, UiNode, emit_ui_tree_interactive,
};

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

/// Command-level wait-chain diagnosis derived from the foreground process tree.
#[derive(Clone, Debug, Default)]
pub struct CommandDiagnosis {
    pub command_name: String,
    pub wait_chain: Vec<String>,
    pub summary: String,
    pub reason: diagnosis::WaitReason,
    pub confidence: diagnosis::Confidence,
    pub source: String,
    pub limitations: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum XRayTab {
    #[default]
    Overview,
    Processes,
    History,
    Trace,
}

#[derive(Clone, Debug)]
pub struct XRaySnapshot {
    pub level: XRayLevel,
    pub capabilities: XRayCapabilities,
    pub fg_pgid: Option<i32>,
    pub nodes: Vec<ProcessNode>,
    pub command_diagnosis: CommandDiagnosis,
    pub total_cpu_percent: f32,
    pub total_rss_bytes: u64,
    pub approx_io_totals: IoCounters,
    pub seconds_since_output: f64,
    pub command_elapsed_secs: f64,
    pub cpu_history: Vec<f32>,
    pub rss_mb_history: Vec<f32>,
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

/// Unified command lifecycle tracker combining OSC 133 semantic shell markers
/// with Linux foreground PGID sampling so every command produces at most one
/// report with clean metrics and accurate exit code / signal attribution.
struct ActiveCommand {
    source_osc133: bool,
    pgid: Option<i32>,
    name: String,
    started_at: Instant,
    started_wall: SystemTime,
    peak_rss: u64,
    peak_procs: usize,
    io_acc: IoAccumulator,
    wait_events: Vec<String>,
}

impl ActiveCommand {
    fn new(
        source_osc133: bool,
        pgid: Option<i32>,
        name: String,
        started_at: Instant,
        started_wall: SystemTime,
    ) -> Self {
        Self {
            source_osc133,
            pgid,
            name,
            started_at,
            started_wall,
            peak_rss: 0,
            peak_procs: 0,
            io_acc: IoAccumulator::default(),
            wait_events: Vec::new(),
        }
    }

    fn update_from_nodes(&mut self, cmd_nodes: &[&ProcessNode]) {
        let cmd_rss: u64 = cmd_nodes.iter().map(|n| n.rss_bytes).sum();
        let cmd_procs = cmd_nodes.len();
        self.peak_rss = self.peak_rss.max(cmd_rss);
        self.peak_procs = self.peak_procs.max(cmd_procs);
        self.io_acc.update(cmd_nodes);
        if self.name.is_empty() && !cmd_nodes.is_empty() {
            // Capture pipeline names cleanly (e.g. "cat | grep") up to 3 stages.
            let mut names: Vec<&str> = Vec::new();
            for n in cmd_nodes {
                if !names.contains(&n.comm.as_str()) {
                    names.push(&n.comm);
                    if names.len() >= 3 {
                        break;
                    }
                }
            }
            self.name = names.join(" | ");
        }
        for n in cmd_nodes {
            if !matches!(
                n.diagnosis.reason,
                diagnosis::WaitReason::ActiveCompute | diagnosis::WaitReason::Unknown
            ) {
                let label = n.diagnosis.reason.label().to_string();
                if !self.wait_events.contains(&label) && self.wait_events.len() < 16 {
                    self.wait_events.push(label);
                }
            }
        }
    }

    fn finish(
        self,
        command_id: u64,
        exit_code: Option<i32>,
        finished_at: Instant,
    ) -> CommandReport {
        // OSC 133;D only reports the shell's `$?` exit status, which cannot
        // reliably distinguish `exit 130` from a real SIGINT termination.
        // Leave `signal: None` unless obtained from a trustworthy OS signal source.
        let label = if !self.name.is_empty() {
            self.name
        } else if self.source_osc133 {
            "shell-builtin".to_string()
        } else if let Some(pg) = self.pgid {
            format!("pgid:{pg}")
        } else {
            "command".to_string()
        };
        CommandReport {
            command_id,
            command: label,
            exit_code,
            signal: None,
            started_at: self.started_wall,
            duration_ms: finished_at
                .saturating_duration_since(self.started_at)
                .as_millis() as u64,
            peak_rss_bytes: self.peak_rss,
            peak_process_count: self.peak_procs.max(usize::from(self.source_osc133)),
            physical_read_bytes: self.io_acc.totals.0,
            physical_write_bytes: self.io_acc.totals.1,
            logical_read_bytes: self.io_acc.totals.2,
            logical_write_bytes: self.io_acc.totals.3,
            wait_events: self.wait_events,
            trace_active: false,
            io_is_approximate: true,
        }
    }
}

type TimestampedShellEvent = (crate::terminal::ShellEvent, Instant, SystemTime);

pub struct XRayEngine {
    pub config: XRayConfig,
    pub panel_open: bool,
    pub active_tab: XRayTab,
    pub selected_pid: Option<(i32, u64)>,
    pub process_scroll: usize,
    pub confirm_destructive: Option<(i32, u64, ProcessAction)>,
    pub action_feedback: Option<String>,
    snapshot: Arc<Mutex<XRaySnapshot>>,
    last_output_at: Arc<Mutex<Instant>>,
    shell_events: Arc<Mutex<Vec<TimestampedShellEvent>>>,
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
            command_diagnosis: CommandDiagnosis::default(),
            total_cpu_percent: 0.0,
            total_rss_bytes: 0,
            approx_io_totals: (0, 0, 0, 0),
            seconds_since_output: 0.0,
            command_elapsed_secs: 0.0,
            cpu_history: Vec::with_capacity(32),
            rss_mb_history: Vec::with_capacity(32),
            reports: Vec::new(),
        }));
        Self {
            config,
            panel_open: false,
            active_tab: XRayTab::Overview,
            selected_pid: None,
            process_scroll: 0,
            confirm_destructive: None,
            action_feedback: None,
            snapshot,
            last_output_at: Arc::new(Mutex::new(Instant::now())),
            shell_events: Arc::new(Mutex::new(Vec::new())),
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

    /// Feed an OSC 133 semantic shell lifecycle event into X-Ray with the exact
    /// arrival timestamp so command durations are independent of worker polling.
    pub fn notify_shell_event(&self, event: crate::terminal::ShellEvent) {
        if self.config.enabled {
            let now = Instant::now();
            let wall = SystemTime::now();
            let mut q = self.shell_events.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() < 64 {
                q.push((event, now, wall));
            }
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
        let shell_events = Arc::clone(&self.shell_events);
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
                let mut active: Option<ActiveCommand> = None;
                let mut cmd_seq = 1u64;

                while !shutdown.load(Ordering::Relaxed) {
                    let pending_shell: Vec<TimestampedShellEvent> = {
                        let mut q = shell_events.lock().unwrap_or_else(|e| e.into_inner());
                        std::mem::take(&mut *q)
                    };
                    for (ev, ev_instant, ev_wall) in pending_shell {
                        match ev {
                            crate::terminal::ShellEvent::CommandExecuted { .. } => {
                                if let Some(prev) = active.take() {
                                    if !prev.source_osc133
                                        && ev_instant.saturating_duration_since(prev.started_at)
                                            < Duration::from_millis(500)
                                    {
                                        // Upgrade an in-flight PGID-detected command to OSC 133
                                        // without duplicating its report.
                                        let mut upgraded = prev;
                                        upgraded.source_osc133 = true;
                                        upgraded.started_at = ev_instant;
                                        upgraded.started_wall = ev_wall;
                                        active = Some(upgraded);
                                        continue;
                                    }
                                    reports.push(prev.finish(cmd_seq, None, ev_instant));
                                    cmd_seq += 1;
                                }
                                active = Some(ActiveCommand::new(
                                    true,
                                    None,
                                    String::new(),
                                    ev_instant,
                                    ev_wall,
                                ));
                            }
                            crate::terminal::ShellEvent::CommandFinished { exit_code, .. } => {
                                if let Some(cur) = active.take() {
                                    reports.push(cur.finish(cmd_seq, exit_code, ev_instant));
                                    cmd_seq += 1;
                                }
                            }
                            crate::terminal::ShellEvent::PromptStart { .. } => {
                                if active.as_ref().is_some_and(|c| c.source_osc133)
                                    && let Some(cur) = active.take()
                                {
                                    reports.push(cur.finish(cmd_seq, None, ev_instant));
                                    cmd_seq += 1;
                                }
                            }
                            crate::terminal::ShellEvent::CommandInputStart { .. } => {}
                        }
                    }
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
                    let sample_now = Instant::now();
                    let sample_wall = SystemTime::now();

                    if let Some(fg) = fg_pgid {
                        if fg != root_pid {
                            let cmd_nodes: Vec<&ProcessNode> =
                                nodes.iter().filter(|n| n.pgid == fg).collect();
                            match active.as_mut() {
                                Some(cur) if cur.source_osc133 => {
                                    // Attach foreground PGID metrics to the active OSC 133 command.
                                    if cur.pgid.is_none() || cur.pgid == Some(fg) {
                                        cur.pgid = Some(fg);
                                        cur.update_from_nodes(&cmd_nodes);
                                    }
                                }
                                Some(cur) if cur.pgid == Some(fg) => {
                                    cur.update_from_nodes(&cmd_nodes);
                                }
                                Some(_) => {
                                    if let Some(prev) = active.take() {
                                        reports.push(prev.finish(cmd_seq, None, sample_now));
                                        cmd_seq += 1;
                                    }
                                    let mut next = ActiveCommand::new(
                                        false,
                                        Some(fg),
                                        String::new(),
                                        sample_now,
                                        sample_wall,
                                    );
                                    next.update_from_nodes(&cmd_nodes);
                                    active = Some(next);
                                }
                                None => {
                                    // PGID fallback always stays active when no OSC 133 command
                                    // is in flight (e.g. inside nested shells without OSC 133).
                                    let mut next = ActiveCommand::new(
                                        false,
                                        Some(fg),
                                        String::new(),
                                        sample_now,
                                        sample_wall,
                                    );
                                    next.update_from_nodes(&cmd_nodes);
                                    active = Some(next);
                                }
                            }
                        } else if let Some(cur) = &active
                            && !cur.source_osc133
                            && let Some(finished) = active.take()
                        {
                            // Fallback PGID-only mode: command returned to root shell.
                            reports.push(finished.finish(cmd_seq, None, sample_now));
                            cmd_seq += 1;
                        }
                    }

                    let cmd_diag = diagnose_command_tree(&nodes, fg_pgid);
                    if let Some(focus_node) = select_focus_process(&nodes, fg_pgid) {
                        bus.publish(AppEvent::XRayUpdate(XRaySummaryPayload {
                            root_pid,
                            process_count: nodes.len(),
                            cpu_percent: total_cpu,
                            rss_bytes: total_rss,
                            state_label: focus_node.state.label().to_string(),
                            waiting_reason: cmd_diag.reason.label().to_string(),
                        }));
                    }

                    {
                        let mut snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                        snap.level = lvl;
                        snap.fg_pgid = fg_pgid;
                        snap.nodes = nodes;
                        snap.command_diagnosis = cmd_diag;
                        snap.total_cpu_percent = total_cpu;
                        snap.total_rss_bytes = total_rss;
                        snap.approx_io_totals =
                            active.as_ref().map_or((0, 0, 0, 0), |c| c.io_acc.totals);
                        snap.seconds_since_output = secs_since_out;
                        snap.command_elapsed_secs = active
                            .as_ref()
                            .map_or(0.0, |c| c.started_at.elapsed().as_secs_f64());
                        if snap.cpu_history.len() >= 32 {
                            snap.cpu_history.remove(0);
                        }
                        snap.cpu_history.push(total_cpu);
                        if snap.rss_mb_history.len() >= 32 {
                            snap.rss_mb_history.remove(0);
                        }
                        snap.rss_mb_history
                            .push(total_rss as f32 / (1024.0 * 1024.0));
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

    /// Handle interactive clicks inside the X-Ray compact badge or diagnostic panel.
    pub fn handle_hit(&mut self, node_id: &str, kind: HitKind) {
        match (node_id, kind) {
            ("xray-toggle", _) => self.toggle_panel(),
            ("xray-level-light", _) => self.set_level(XRayLevel::Light),
            ("xray-level-inspect", _) => self.set_level(XRayLevel::Inspect),
            ("xray-tabs", HitKind::Tab(idx)) => {
                self.active_tab = match idx {
                    0 => XRayTab::Overview,
                    1 => XRayTab::Processes,
                    2 => XRayTab::History,
                    _ => XRayTab::Trace,
                };
                self.confirm_destructive = None;
                self.dirty.store(true, Ordering::Relaxed);
            }
            ("xray-scroll-up", _) => {
                self.process_scroll = self.process_scroll.saturating_sub(4);
                self.dirty.store(true, Ordering::Relaxed);
            }
            ("xray-scroll-down", _) => {
                self.process_scroll = self.process_scroll.saturating_add(4);
                self.dirty.store(true, Ordering::Relaxed);
            }
            (id, _) if id.starts_with("xray-proc-") => {
                if let Ok(pid) = id.trim_start_matches("xray-proc-").parse::<i32>() {
                    let snap = self.snapshot();
                    if let Some(node) = snap.nodes.iter().find(|n| n.pid == pid) {
                        self.selected_pid = Some((node.pid, node.starttime));
                        self.confirm_destructive = None;
                        self.action_feedback = None;
                        self.dirty.store(true, Ordering::Relaxed);
                    }
                }
            }
            ("xray-act-pause", _) => self.apply_selected_action(ProcessAction::Pause, false),
            ("xray-act-resume", _) => self.apply_selected_action(ProcessAction::Resume, false),
            ("xray-act-term", _) => self.apply_selected_action(ProcessAction::Terminate, true),
            ("xray-act-kill", _) => self.apply_selected_action(ProcessAction::Kill, true),
            _ => {}
        }
    }

    fn apply_selected_action(&mut self, action: ProcessAction, requires_confirm: bool) {
        let Some((pid, starttime)) = self.selected_pid else {
            return;
        };
        if requires_confirm && self.confirm_destructive != Some((pid, starttime, action)) {
            self.confirm_destructive = Some((pid, starttime, action));
            self.action_feedback =
                Some(format!("Click {:?} again to confirm for PID {pid}", action));
            self.dirty.store(true, Ordering::Relaxed);
            return;
        }
        self.confirm_destructive = None;
        #[cfg(target_os = "linux")]
        let res = linux::execute_process_action(pid, starttime, action);
        #[cfg(not(target_os = "linux"))]
        let res: Result<(), String> = Err("process control unavailable on this OS".into());
        self.action_feedback = Some(match res {
            Ok(()) => format!("{action:?} sent safely via pidfd to PID {pid}"),
            Err(e) => format!("Action failed: {e}"),
        });
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Render adaptive compact indicator and (if open) full diagnostic X-Ray panel.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_to_scene(
        &self,
        win_size: (f32, f32),
        scale: f32,
        pointer_px: (f32, f32),
        theme: &Theme,
        renderer: &mut Renderer,
        scene: &mut Scene,
        hits: &mut Vec<UiHitTarget>,
    ) {
        if !self.config.enabled {
            return;
        }
        let snap = self.snapshot();
        let logical_w = win_size.0 / scale.max(0.25);
        let compact_win = logical_w < 720.0;
        let status_color = match snap.command_diagnosis.reason {
            diagnosis::WaitReason::ActiveCompute => "success",
            diagnosis::WaitReason::WaitingFileLock | diagnosis::WaitReason::PossibleDeadlock => {
                "error"
            }
            diagnosis::WaitReason::WaitingStdin | diagnosis::WaitReason::Stopped => "warning",
            diagnosis::WaitReason::WaitingNetwork | diagnosis::WaitReason::WaitingDiskIo => "cyan",
            _ => "muted",
        };

        if self.config.compact_indicator {
            let cmd = if snap.command_diagnosis.command_name.is_empty() {
                "idle"
            } else {
                snap.command_diagnosis.command_name.as_str()
            };
            let label = if compact_win {
                format!(
                    "◈ {cmd} · {:.0}% · {}",
                    snap.total_cpu_percent,
                    snap.command_diagnosis.reason.label()
                )
            } else {
                format!(
                    "◈ X-Ray [{}] {cmd} · {:.1}% CPU · {} MB · {:.0}s · {}",
                    snap.level.name(),
                    snap.total_cpu_percent,
                    snap.total_rss_bytes / (1024 * 1024),
                    snap.command_elapsed_secs,
                    snap.command_diagnosis.reason.label()
                )
            };
            let w = if compact_win {
                (210.0 * scale).min(win_size.0 * 0.52).max(120.0 * scale)
            } else {
                (400.0 * scale).min(win_size.0 - 24.0 * scale)
            };
            let h = 26.0 * scale;
            let badge = UiNode::Button {
                id: "xray-toggle".into(),
                label,
                bg: Some(UiColor::Token("surface-elevated".into())),
                color: Some(UiColor::Token(status_color.into())),
                width: Some(w / scale),
                height: Some(h / scale),
                radius: 8.0,
            };
            emit_ui_tree_interactive(
                "xray-indicator",
                &badge,
                (12.0 * scale, (win_size.1 - h - 10.0 * scale).max(0.0), w, h),
                scale,
                0.94,
                UiLayer::Overlay,
                Some(pointer_px),
                theme,
                renderer,
                scene,
                hits,
            );
        }

        if !self.panel_open {
            return;
        }

        let tabs = vec![
            "Overview".into(),
            format!("Processes ({})", snap.nodes.len()),
            format!("History ({})", snap.reports.len()),
            "Trace".into(),
        ];
        let active_tab_idx = match self.active_tab {
            XRayTab::Overview => 0,
            XRayTab::Processes => 1,
            XRayTab::History => 2,
            XRayTab::Trace => 3,
        };

        let mut rows = vec![
            UiNode::Row {
                gap: 6.0,
                align: Align::Center,
                children: vec![
                    UiNode::Icon {
                        name: "xray".into(),
                        size: Some(14.0),
                        color: Some(UiColor::Token("cyan".into())),
                    },
                    UiNode::Text {
                        text: format!(
                            "Beyrix X-Ray · {}",
                            if snap.command_diagnosis.command_name.is_empty() {
                                "shell"
                            } else {
                                &snap.command_diagnosis.command_name
                            }
                        ),
                        size: Some(14.0),
                        color: Some(UiColor::Token("fg".into())),
                        align: Align::Start,
                    },
                    UiNode::Badge {
                        text: snap.command_diagnosis.confidence.label().into(),
                        color: Some(UiColor::Token(status_color.into())),
                        bg: None,
                    },
                    UiNode::Button {
                        id: "xray-level-light".into(),
                        label: "Light".into(),
                        bg: Some(UiColor::Token(
                            if snap.level == XRayLevel::Light {
                                "accent-soft"
                            } else {
                                "surface"
                            }
                            .into(),
                        )),
                        color: Some(UiColor::Token("fg".into())),
                        width: Some(50.0),
                        height: Some(22.0),
                        radius: 5.0,
                    },
                    UiNode::Button {
                        id: "xray-level-inspect".into(),
                        label: "Inspect".into(),
                        bg: Some(UiColor::Token(
                            if snap.level == XRayLevel::Inspect {
                                "accent-soft"
                            } else {
                                "surface"
                            }
                            .into(),
                        )),
                        color: Some(UiColor::Token("fg".into())),
                        width: Some(60.0),
                        height: Some(22.0),
                        radius: 5.0,
                    },
                    UiNode::Button {
                        id: "xray-toggle".into(),
                        label: "✕".into(),
                        bg: Some(UiColor::Token("surface".into())),
                        color: Some(UiColor::Token("muted".into())),
                        width: Some(26.0),
                        height: Some(22.0),
                        radius: 5.0,
                    },
                ],
            },
            UiNode::Tabs {
                id: "xray-tabs".into(),
                tabs,
                active: active_tab_idx,
            },
            UiNode::Divider {
                vertical: false,
                color: None,
            },
        ];

        match self.active_tab {
            XRayTab::Overview => {
                rows.push(UiNode::Text {
                    text: snap.command_diagnosis.summary.clone(),
                    size: Some(12.5),
                    color: Some(UiColor::Token(status_color.into())),
                    align: Align::Start,
                });
                if !snap.command_diagnosis.wait_chain.is_empty() {
                    rows.push(UiNode::Text {
                        text: format!(
                            "Wait chain: {}",
                            snap.command_diagnosis.wait_chain.join(" → ")
                        ),
                        size: Some(11.0),
                        color: Some(UiColor::Token("fg".into())),
                        align: Align::Start,
                    });
                }
                rows.push(UiNode::Text {
                    text: format!(
                        "Elapsed: {:.1}s  ·  Last output: {:.1}s ago  ·  Procs: {}",
                        snap.command_elapsed_secs,
                        snap.seconds_since_output,
                        snap.nodes.len()
                    ),
                    size: Some(11.0),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                });
                rows.push(UiNode::Row {
                    gap: 10.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Column {
                            gap: 2.0,
                            align: Align::Start,
                            children: vec![
                                UiNode::Text {
                                    text: format!("CPU {:.1}%", snap.total_cpu_percent),
                                    size: Some(10.5),
                                    color: Some(UiColor::Token("cyan".into())),
                                    align: Align::Start,
                                },
                                UiNode::Sparkline {
                                    values: snap.cpu_history.clone(),
                                    width: 150.0,
                                    height: 26.0,
                                    color: Some(UiColor::Token("cyan".into())),
                                },
                            ],
                        },
                        UiNode::Column {
                            gap: 2.0,
                            align: Align::Start,
                            children: vec![
                                UiNode::Text {
                                    text: format!(
                                        "RSS {} MB",
                                        snap.total_rss_bytes / (1024 * 1024)
                                    ),
                                    size: Some(10.5),
                                    color: Some(UiColor::Token("accent".into())),
                                    align: Align::Start,
                                },
                                UiNode::Sparkline {
                                    values: snap.rss_mb_history.clone(),
                                    width: 150.0,
                                    height: 26.0,
                                    color: Some(UiColor::Token("accent".into())),
                                },
                            ],
                        },
                    ],
                });
                let (pr, pw, lr, lw) = snap.approx_io_totals;
                rows.push(UiNode::Text {
                    text: format!(
                        "I/O (~approx procfs): Phys R {} / W {} KB  ·  Log R {} / W {} KB",
                        pr / 1024,
                        pw / 1024,
                        lr / 1024,
                        lw / 1024
                    ),
                    size: Some(10.5),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                });
                rows.push(UiNode::Text {
                    text: format!(
                        "Source: {}  ·  Note: {}",
                        snap.command_diagnosis.source, snap.command_diagnosis.limitations
                    ),
                    size: Some(10.5),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                });
            }
            XRayTab::Processes => {
                let page_size = 6usize;
                let max_scroll = snap.nodes.len().saturating_sub(page_size);
                let offset = self.process_scroll.min(max_scroll);
                rows.push(UiNode::Row {
                    gap: 6.0,
                    align: Align::Center,
                    children: vec![
                        UiNode::Text {
                            text: format!(
                                "Showing {}-{} of {}",
                                if snap.nodes.is_empty() { 0 } else { offset + 1 },
                                (offset + page_size).min(snap.nodes.len()),
                                snap.nodes.len()
                            ),
                            size: Some(10.5),
                            color: Some(UiColor::Token("muted".into())),
                            align: Align::Start,
                        },
                        UiNode::Button {
                            id: "xray-scroll-up".into(),
                            label: "▲".into(),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(26.0),
                            height: Some(20.0),
                            radius: 4.0,
                        },
                        UiNode::Button {
                            id: "xray-scroll-down".into(),
                            label: "▼".into(),
                            bg: Some(UiColor::Token("surface".into())),
                            color: Some(UiColor::Token("fg".into())),
                            width: Some(26.0),
                            height: Some(20.0),
                            radius: 4.0,
                        },
                    ],
                });
                for node in snap.nodes.iter().skip(offset).take(page_size) {
                    let selected = self.selected_pid == Some((node.pid, node.starttime));
                    rows.push(UiNode::Button {
                        id: format!("xray-proc-{}", node.pid),
                        label: format!(
                            "PID {} ({}) · {} · {:.1}% · {}MB · {}",
                            node.pid,
                            node.comm,
                            node.state.label(),
                            node.cpu_percent,
                            node.rss_bytes / (1024 * 1024),
                            node.diagnosis.reason.label()
                        ),
                        bg: Some(UiColor::Token(
                            if selected { "accent-soft" } else { "surface" }.into(),
                        )),
                        color: Some(UiColor::Token("fg".into())),
                        width: Some(540.0),
                        height: Some(22.0),
                        radius: 5.0,
                    });
                }
                if let Some((spid, sstart)) = self.selected_pid
                    && let Some(sel) = snap
                        .nodes
                        .iter()
                        .find(|n| n.pid == spid && n.starttime == sstart)
                {
                    rows.push(UiNode::Divider {
                        vertical: false,
                        color: None,
                    });
                    rows.push(UiNode::Text {
                        text: format!(
                            "PID {} PPID {} PGID {} · Threads {} · FDs {} · Sockets {}",
                            sel.pid,
                            sel.ppid,
                            sel.pgid,
                            sel.threads,
                            sel.open_fds.len(),
                            sel.net_sockets
                        ),
                        size: Some(10.5),
                        color: Some(UiColor::Token("accent".into())),
                        align: Align::Start,
                    });
                    rows.push(UiNode::Text {
                        text: format!("Evidence: {}", sel.diagnosis.evidence.join("; ")),
                        size: Some(10.5),
                        color: Some(UiColor::Token("muted".into())),
                        align: Align::Start,
                    });
                    if snap.capabilities.process_control {
                        rows.push(UiNode::Row {
                            gap: 6.0,
                            align: Align::Center,
                            children: vec![
                                UiNode::Button {
                                    id: "xray-act-pause".into(),
                                    label: "Pause".into(),
                                    bg: Some(UiColor::Token("surface".into())),
                                    color: Some(UiColor::Token("warning".into())),
                                    width: Some(58.0),
                                    height: Some(22.0),
                                    radius: 5.0,
                                },
                                UiNode::Button {
                                    id: "xray-act-resume".into(),
                                    label: "Resume".into(),
                                    bg: Some(UiColor::Token("surface".into())),
                                    color: Some(UiColor::Token("success".into())),
                                    width: Some(64.0),
                                    height: Some(22.0),
                                    radius: 5.0,
                                },
                                UiNode::Button {
                                    id: "xray-act-term".into(),
                                    label: "Terminate".into(),
                                    bg: Some(UiColor::Token("surface".into())),
                                    color: Some(UiColor::Token("error".into())),
                                    width: Some(78.0),
                                    height: Some(22.0),
                                    radius: 5.0,
                                },
                                UiNode::Button {
                                    id: "xray-act-kill".into(),
                                    label: "Kill".into(),
                                    bg: Some(UiColor::Token("surface".into())),
                                    color: Some(UiColor::Token("error".into())),
                                    width: Some(46.0),
                                    height: Some(22.0),
                                    radius: 5.0,
                                },
                            ],
                        });
                    }
                }
                if let Some(msg) = &self.action_feedback {
                    rows.push(UiNode::Text {
                        text: msg.clone(),
                        size: Some(10.5),
                        color: Some(UiColor::Token("warning".into())),
                        align: Align::Start,
                    });
                }
            }
            XRayTab::History => {
                if snap.reports.is_empty() {
                    rows.push(UiNode::Text {
                        text: "No completed commands recorded yet.".into(),
                        size: Some(11.5),
                        color: Some(UiColor::Token("muted".into())),
                        align: Align::Start,
                    });
                } else {
                    for rep in snap.reports.iter().rev().take(6) {
                        let waits = if rep.wait_events.is_empty() {
                            "none".to_string()
                        } else {
                            rep.wait_events.join(", ")
                        };
                        rows.push(UiNode::Text {
                            text: format!(
                                "#{} {} · {:.2}s · peak {}MB ({} procs) · waits: {}",
                                rep.command_id,
                                rep.command,
                                rep.duration_ms as f64 / 1000.0,
                                rep.peak_rss_bytes / (1024 * 1024),
                                rep.peak_process_count,
                                waits
                            ),
                            size: Some(11.0),
                            color: Some(UiColor::Token("fg".into())),
                            align: Align::Start,
                        });
                    }
                    rows.push(UiNode::Text {
                        text:
                            "Note: Peak RSS and I/O in reports are sampled approximations (procfs)."
                                .into(),
                        size: Some(10.0),
                        color: Some(UiColor::Token("muted".into())),
                        align: Align::Start,
                    });
                }
            }
            XRayTab::Trace => {
                rows.push(UiNode::Text {
                    text: if snap.capabilities.trace_syscalls {
                        "Trace backend available (opt-in only).".into()
                    } else {
                        "Syscall Trace mode is unavailable in unprivileged mode.".into()
                    },
                    size: Some(12.0),
                    color: Some(UiColor::Token("warning".into())),
                    align: Align::Start,
                });
                rows.push(UiNode::Text {
                    text: "Why: Linux Yama ptrace_scope / CAP_BPF / SELinux restrict syscall tracing without explicit privileges. Beyrix never runs as root and never bypasses OS security."
                        .into(),
                    size: Some(10.5),
                    color: Some(UiColor::Token("muted".into())),
                    align: Align::Start,
                });
                rows.push(UiNode::Text {
                    text: "Light & Inspect modes remain fully active using unprivileged /proc & /proc/locks."
                        .into(),
                    size: Some(10.5),
                    color: Some(UiColor::Token("fg".into())),
                    align: Align::Start,
                });
            }
        }

        let panel = UiNode::Container {
            padding: 12.0,
            bg: Some(UiColor::Token("surface-elevated".into())),
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

        let pw = (600.0 * scale).min(win_size.0 - 24.0 * scale);
        let ph = (360.0 * scale).min(win_size.1 - 56.0 * scale);
        emit_ui_tree_interactive(
            "xray-panel",
            &panel,
            (12.0 * scale, 12.0 * scale, pw, ph),
            scale,
            0.96,
            UiLayer::Overlay,
            Some(pointer_px),
            theme,
            renderer,
            scene,
            hits,
        );
    }
}

/// Build a command-level wait-chain diagnosis from the entire process tree.
///
/// Walks `bash -> cargo -> rustc` so that if a leaf descendant is actively
/// computing (e.g. `rustc` at 80% CPU while `bash` and `cargo` are in
/// `WaitingChild`), the command-level diagnosis reports active computation
/// rather than misreporting the command as sleeping.
pub fn diagnose_command_tree(nodes: &[ProcessNode], fg_pgid: Option<i32>) -> CommandDiagnosis {
    let Some(focus) = select_focus_process(nodes, fg_pgid) else {
        return CommandDiagnosis {
            command_name: "idle".into(),
            summary: "No active foreground process".into(),
            ..Default::default()
        };
    };

    // Reconstruct ancestor chain up to the root tracked process.
    let mut chain = vec![focus.comm.clone()];
    let mut cur_ppid = focus.ppid;
    for _ in 0..16 {
        if let Some(parent) = nodes.iter().find(|n| n.pid == cur_ppid) {
            chain.push(parent.comm.clone());
            if parent.ppid == cur_ppid {
                break;
            }
            cur_ppid = parent.ppid;
        } else {
            break;
        }
    }
    chain.reverse();

    // If any foreground descendant is actively computing (>= 5% CPU or Running)
    // and the focus isn't blocked on a hard file lock / deadlock / stdin,
    // surface the active computation so a waiting parent never hides real work.
    let active_compute = nodes.iter().find(|n| {
        fg_pgid.is_none_or(|fg| n.pgid == fg)
            && n.diagnosis.reason == diagnosis::WaitReason::ActiveCompute
    });
    if let Some(worker) = active_compute
        && !matches!(
            focus.diagnosis.reason,
            diagnosis::WaitReason::WaitingFileLock | diagnosis::WaitReason::PossibleDeadlock
        )
    {
        return CommandDiagnosis {
            command_name: worker.comm.clone(),
            wait_chain: chain,
            summary: format!(
                "{} is actively computing ({:.1}% CPU)",
                worker.comm, worker.cpu_percent
            ),
            reason: diagnosis::WaitReason::ActiveCompute,
            confidence: worker.diagnosis.confidence,
            source: worker.diagnosis.source.clone(),
            limitations: worker.diagnosis.limitations.clone(),
        };
    }

    CommandDiagnosis {
        command_name: focus.comm.clone(),
        wait_chain: chain,
        summary: focus.diagnosis.summary.clone(),
        reason: focus.diagnosis.reason,
        confidence: focus.diagnosis.confidence,
        source: focus.diagnosis.source.clone(),
        limitations: focus.diagnosis.limitations.clone(),
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

    #[test]
    fn command_level_wait_chain_reports_active_rustc_not_sleeping_bash() {
        let mk = |pid, ppid, pgid, comm: &str, reason, cpu| ProcessNode {
            pid,
            ppid,
            pgid,
            starttime: 100,
            comm: comm.into(),
            state: if reason == diagnosis::WaitReason::ActiveCompute {
                ProcessStateKind::Running
            } else {
                ProcessStateKind::Sleeping
            },
            cpu_percent: cpu,
            rss_bytes: 32 * 1024 * 1024,
            threads: 1,
            read_bytes: 0,
            write_bytes: 0,
            logical_read_bytes: 0,
            logical_write_bytes: 0,
            open_fds: Vec::new(),
            net_sockets: 0,
            diagnosis: Diagnosis {
                reason,
                summary: reason.label().into(),
                evidence: Vec::new(),
                source: "procfs".into(),
                confidence: diagnosis::Confidence::High,
                limitations: String::new(),
                updated_at: Instant::now(),
            },
        };
        let tree = vec![
            mk(10, 1, 10, "bash", diagnosis::WaitReason::WaitingChild, 0.0),
            mk(
                20,
                10,
                20,
                "cargo",
                diagnosis::WaitReason::WaitingChild,
                0.5,
            ),
            mk(
                30,
                20,
                20,
                "rustc",
                diagnosis::WaitReason::ActiveCompute,
                88.0,
            ),
        ];
        let diag = diagnose_command_tree(&tree, Some(20));
        assert_eq!(diag.reason, diagnosis::WaitReason::ActiveCompute);
        assert_eq!(diag.command_name, "rustc");
        assert_eq!(diag.wait_chain, vec!["bash", "cargo", "rustc"]);

        // Render compact + full panel across all 4 tabs without panicking.
        let mut engine = XRayEngine::new(XRayConfig {
            enabled: true,
            ..Default::default()
        });
        engine.toggle_panel();
        let mut renderer = Renderer::new().unwrap();
        let theme = Theme::default();
        for tab_idx in 0..4 {
            engine.handle_hit("xray-tabs", HitKind::Tab(tab_idx));
            let mut scene = Scene::default();
            let mut hits = Vec::new();
            engine.emit_to_scene(
                (600.0, 500.0),
                1.0,
                (0.0, 0.0),
                &theme,
                &mut renderer,
                &mut scene,
                &mut hits,
            );
            assert!(!scene.overlay_rounded.is_empty());
            assert!(!hits.is_empty());
        }
    }
}
