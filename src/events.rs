// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified bounded event bus for Beyrix subsystems.
//!
//! Provides non-blocking publish/subscribe communication between Terminal Core,
//! Keyboard Layout, Media/Audio, Background Engine, X-Ray, and BeyrixWidgets
//! without creating cyclic module dependencies or unbounded memory growth.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

/// Maximum number of queued events per subscriber before oldest events are dropped
/// (backpressure).
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;

/// Coarse event categories for subscription filtering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventKind {
    KeyboardLayout,
    WindowFocus,
    WindowResize,
    TerminalTuiMode,
    TerminalCommand,
    MediaSession,
    AudioFrame,
    XRayUpdate,
    XRayReport,
    BackgroundState,
    WidgetAction,
    ThemeChanged,
}

/// Snapshot of the active keyboard layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyboardLayoutEvent {
    pub active_index: u32,
    pub active_name: String,
    pub short_name: String,
    pub layouts: Vec<String>,
}

/// Snapshot of a terminal command lifecycle (from OSC 133 or PTY process group).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalCommandEvent {
    pub command_id: u64,
    pub command_line: Option<String>,
    pub cwd: Option<String>,
    pub phase: CommandPhase,
    pub exit_code: Option<i32>,
    pub pid: Option<i32>,
    pub pgid: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandPhase {
    PromptStart,
    CommandStart,
    CommandExecuted,
    CommandFinished,
}

/// Snapshot of a media session update.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaEventPayload {
    pub session_id: String,
    pub source: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub volume: Option<f64>,
}

/// Compact audio analysis frame summary for event subscribers.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioFramePayload {
    pub amplitude: f32,
    pub rms: f32,
    pub peak: f32,
    pub beat: bool,
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    pub bands: Vec<f32>,
}

/// X-Ray process activity summary.
#[derive(Clone, Debug, PartialEq)]
pub struct XRaySummaryPayload {
    pub root_pid: i32,
    pub process_count: usize,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub state_label: String,
    pub waiting_reason: String,
}

/// Unified event payload.
#[derive(Clone, Debug)]
pub enum AppEvent {
    KeyboardLayout(KeyboardLayoutEvent),
    WindowFocus(bool),
    WindowResize {
        width: i32,
        height: i32,
        scale: f32,
    },
    TerminalTuiMode {
        active: bool,
        alternate_screen: bool,
    },
    TerminalCommand(TerminalCommandEvent),
    MediaSession(MediaEventPayload),
    AudioFrame(AudioFramePayload),
    XRayUpdate(XRaySummaryPayload),
    XRayReportReady {
        command_id: u64,
        command: String,
        exit_code: Option<i32>,
        duration_ms: u64,
    },
    BackgroundState {
        enabled: bool,
        preset: String,
        fps: u32,
    },
    WidgetAction {
        widget_id: String,
        action: String,
        payload: String,
    },
    ThemeChanged {
        name: String,
    },
}

impl AppEvent {
    pub fn kind(&self) -> EventKind {
        match self {
            Self::KeyboardLayout(_) => EventKind::KeyboardLayout,
            Self::WindowFocus(_) => EventKind::WindowFocus,
            Self::WindowResize { .. } => EventKind::WindowResize,
            Self::TerminalTuiMode { .. } => EventKind::TerminalTuiMode,
            Self::TerminalCommand(_) => EventKind::TerminalCommand,
            Self::MediaSession(_) => EventKind::MediaSession,
            Self::AudioFrame(_) => EventKind::AudioFrame,
            Self::XRayUpdate(_) => EventKind::XRayUpdate,
            Self::XRayReportReady { .. } => EventKind::XRayReport,
            Self::BackgroundState { .. } => EventKind::BackgroundState,
            Self::WidgetAction { .. } => EventKind::WidgetAction,
            Self::ThemeChanged { .. } => EventKind::ThemeChanged,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EventEnvelope {
    pub seq: u64,
    pub timestamp: Instant,
    pub wall_time: SystemTime,
    pub event: AppEvent,
}

struct Subscriber {
    id: u64,
    filter: Option<Vec<EventKind>>,
    queue: VecDeque<EventEnvelope>,
    capacity: usize,
    dropped: u64,
}

struct BusInner {
    next_seq: u64,
    next_sub_id: u64,
    subscribers: Vec<Subscriber>,
}

/// Thread-safe bounded event bus.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Mutex<BusInner>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BusInner {
                next_seq: 1,
                next_sub_id: 1,
                subscribers: Vec::new(),
            })),
        }
    }

    /// Subscribe to all events or a specific subset of [`EventKind`]s.
    pub fn subscribe(&self, filter: Option<Vec<EventKind>>, capacity: usize) -> EventSubscription {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let id = inner.next_sub_id;
        inner.next_sub_id += 1;
        inner.subscribers.push(Subscriber {
            id,
            filter,
            queue: VecDeque::with_capacity(capacity.min(DEFAULT_QUEUE_CAPACITY)),
            capacity: capacity.max(1),
            dropped: 0,
        });
        EventSubscription {
            id,
            bus: Arc::clone(&self.inner),
        }
    }

    /// Publish an event to all matching subscribers. Never blocks.
    pub fn publish(&self, event: AppEvent) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let kind = event.kind();
        let seq = inner.next_seq;
        inner.next_seq += 1;
        let envelope = EventEnvelope {
            seq,
            timestamp: Instant::now(),
            wall_time: SystemTime::now(),
            event,
        };
        for sub in &mut inner.subscribers {
            let matches = sub
                .filter
                .as_ref()
                .is_none_or(|kinds| kinds.contains(&kind));
            if !matches {
                continue;
            }
            if sub.queue.len() >= sub.capacity {
                sub.queue.pop_front();
                sub.dropped += 1;
            }
            sub.queue.push_back(envelope.clone());
        }
    }

    /// Returns the number of active subscribers (useful to skip work when 0).
    pub fn subscriber_count(&self, kind: EventKind) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .subscribers
            .iter()
            .filter(|sub| {
                sub.filter
                    .as_ref()
                    .is_none_or(|kinds| kinds.contains(&kind))
            })
            .count()
    }
}

/// Handle for receiving events; automatically unsubscribes on drop.
pub struct EventSubscription {
    id: u64,
    bus: Arc<Mutex<BusInner>>,
}

impl EventSubscription {
    /// Drain up to `max` pending events.
    pub fn drain(&self, max: usize) -> Vec<EventEnvelope> {
        let mut inner = self.bus.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sub) = inner.subscribers.iter_mut().find(|s| s.id == self.id) else {
            return Vec::new();
        };
        let count = sub.queue.len().min(max);
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(ev) = sub.queue.pop_front() {
                out.push(ev);
            }
        }
        out
    }

    /// Number of events dropped due to queue capacity backpressure.
    pub fn dropped_count(&self) -> u64 {
        let inner = self.bus.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .subscribers
            .iter()
            .find(|s| s.id == self.id)
            .map_or(0, |s| s.dropped)
    }
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        let mut inner = self.bus.lock().unwrap_or_else(|e| e.into_inner());
        inner.subscribers.retain(|s| s.id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_queue_drops_oldest_under_backpressure() {
        let bus = EventBus::new();
        let sub = bus.subscribe(Some(vec![EventKind::WindowFocus]), 2);
        bus.publish(AppEvent::WindowFocus(true));
        bus.publish(AppEvent::WindowFocus(false));
        bus.publish(AppEvent::WindowFocus(true));
        assert_eq!(sub.dropped_count(), 1);
        let events = sub.drain(10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 2);
        assert_eq!(events[1].seq, 3);
    }

    #[test]
    fn drop_unsubscribes_cleanly() {
        let bus = EventBus::new();
        assert_eq!(bus.subscriber_count(EventKind::AudioFrame), 0);
        let sub = bus.subscribe(Some(vec![EventKind::AudioFrame]), 8);
        assert_eq!(bus.subscriber_count(EventKind::AudioFrame), 1);
        drop(sub);
        assert_eq!(bus.subscriber_count(EventKind::AudioFrame), 0);
    }
}
