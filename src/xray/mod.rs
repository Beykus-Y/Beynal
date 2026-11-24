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
    pub nodes: Vec<ProcessNode>,
    pub total_cpu_percent: f32,
    pub total_rss_bytes: u64,
    pub reports: Vec<CommandReport>,
}

pub struct XRayEngine {
    pub config: XRayConfig,
    pub panel_open: bool,
    snapshot: Arc<Mutex<XRaySnapshot>>,
    last_output_at: Arc<Mutex<Instant>>,
    level: Arc<Mutex<XRayLevel>>,
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
            shutdown: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
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
    }

    pub fn set_level(&mut self, level: XRayLevel) {
        self.config.level = level;
        *self.level.lock().unwrap_or_else(|e| e.into_inner()) = level;
    }

    pub fn ensure_started(&mut self, root_pid: i32, pty_master_fd: Option<i32>, bus: EventBus) {
        if !self.config.enabled || self.worker.is_some() {
            return;
        }
        let snapshot = Arc::clone(&self.snapshot);
        let last_out = Arc::clone(&self.last_output_at);
        let level_shared = Arc::clone(&self.level);
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

                    // Track foreground job lifecycle for post-command reports
                    if let Some(fg) = fg_pgid {
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
                                    physical_read_bytes: 0,
                                    physical_write_bytes: 0,
                                    logical_read_bytes: 0,
                                    logical_write_bytes: 0,
                                    wait_events: Vec::new(),
                                    trace_active: lvl == XRayLevel::Trace,
                                });
                                cmd_seq += 1;
                            }
                            prev_fg = Some(fg);
                            cmd_started = Instant::now();
                            cmd_wall = SystemTime::now();
                            peak_rss = total_rss;
                            peak_procs = nodes.len();
                            cmd_name = nodes
                                .iter()
                                .find(|n| n.pgid == fg)
                                .map(|n| n.comm.clone())
                                .unwrap_or_else(|| format!("pgid:{fg}"));
                        } else {
                            peak_rss = peak_rss.max(total_rss);
                            peak_procs = peak_procs.max(nodes.len());
                        }
                    }

                    if let Some(focus_node) = nodes.last() {
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
                        snap.nodes = nodes;
                        snap.total_cpu_percent = total_cpu;
                        snap.total_rss_bytes = total_rss;
                        snap.reports = reports.list();
                    }

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
            let status_text = if let Some(last) = snap.nodes.last() {
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
}
