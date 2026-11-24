// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Capability-based permission system for Beyrix extensions.
//!
//! By default, third-party extensions have zero access to sensitive resources
//! (user keystrokes/passwords, terminal contents, environment variables,
//! arbitrary files, network, process spawning/control, system audio capture).
//! Each capability must be declared in the extension manifest and explicitly
//! granted by the user or policy.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Individual capability that an extension can request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// Render native UI nodes inside the widget's own bounds.
    Ui,
    /// Draw to a 2D pixel/vector canvas inside the widget's bounds.
    Canvas,
    /// Read basic non-sensitive system metrics (CPU%, RAM%, uptime).
    SystemInfo,
    /// Subscribe to keyboard layout change events (active group & layout names).
    /// Does NOT grant access to keystrokes or typed characters.
    KeyboardLayout,
    /// Read media session metadata and send playback control commands (MPRIS / GSMTC / local player).
    MediaControl,
    /// Subscribe to processed audio analysis frames (RMS, peak, FFT bands, beat).
    AudioAnalysis,
    /// Capture raw system audio loopback (PipeWire / WASAPI). Sensitive!
    AudioCapture,
    /// Read terminal grid / command text (OSC 133 / visible lines). Sensitive!
    TerminalRead,
    /// Control Beyrix Background Engine scenes, objects, and native data bindings.
    BackgroundControl,
    /// Read X-Ray process summary & wait-reason diagnostics.
    XRayRead,
    /// Read detailed X-Ray inspection data (open FDs, network sockets, locks). Sensitive!
    XRayInspect,
    /// Perform process control actions via X-Ray (pause/resume/signal/priority). Sensitive!
    XRayControl,
    /// Scoped key-value storage isolated to this extension's ID.
    Storage,
    /// Make outbound HTTP/network requests. Sensitive!
    Network,
    /// Spawn child processes. Sensitive!
    ProcessSpawn,
    /// Read Beyrix theme colors and subscribe to theme changes.
    Theme,
    /// Query window geometry, focus state, and scale factor.
    Window,
    /// Read/write files outside the extension's sandboxed directory. Sensitive!
    Filesystem,
    /// Read process environment variables. Sensitive!
    Environment,
}

impl Capability {
    /// Whether this capability exposes potentially sensitive user/system data
    /// and must never be auto-granted to third-party extensions.
    pub fn is_sensitive(self) -> bool {
        matches!(
            self,
            Self::AudioCapture
                | Self::TerminalRead
                | Self::XRayInspect
                | Self::XRayControl
                | Self::Network
                | Self::ProcessSpawn
                | Self::Filesystem
                | Self::Environment
        )
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Ui => "Render native widget UI elements",
            Self::Canvas => "Draw 2D graphics on widget canvas",
            Self::SystemInfo => "Read aggregate CPU/RAM/uptime statistics",
            Self::KeyboardLayout => "Observe active keyboard layout changes (no keylogging)",
            Self::MediaControl => "Read media metadata and control playback",
            Self::AudioAnalysis => "Read FFT spectrum, RMS, peak, and beat events",
            Self::AudioCapture => "Capture system audio stream (PipeWire/WASAPI)",
            Self::TerminalRead => "Read terminal screen text and command output",
            Self::BackgroundControl => "Create and control 3D Background Engine scenes",
            Self::XRayRead => "Read X-Ray process activity and wait diagnostics",
            Self::XRayInspect => "Inspect open files, sockets, and locks via X-Ray",
            Self::XRayControl => "Pause, resume, or signal processes via X-Ray",
            Self::Storage => "Persist extension-scoped key-value settings",
            Self::Network => "Make outbound network requests",
            Self::ProcessSpawn => "Spawn external child processes",
            Self::Theme => "Read active Beyrix theme colors",
            Self::Window => "Read window size, focus, and HiDPI scale",
            Self::Filesystem => "Access files outside extension sandbox",
            Self::Environment => "Read environment variables",
        }
    }
}

/// Active permission set enforced at runtime for a single extension instance.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSet {
    pub granted: BTreeSet<Capability>,
    pub denied: BTreeSet<Capability>,
}

impl PermissionSet {
    pub fn new(granted: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            granted: granted.into_iter().collect(),
            denied: BTreeSet::new(),
        }
    }

    /// Build a safe default permission set for an untrusted third-party extension:
    /// only non-sensitive requested capabilities are granted unless explicitly approved.
    pub fn safe_default(requested: &[Capability], user_approved: &[Capability]) -> Self {
        let mut granted = BTreeSet::new();
        let mut denied = BTreeSet::new();
        for &cap in requested {
            if !cap.is_sensitive() || user_approved.contains(&cap) {
                granted.insert(cap);
            } else {
                denied.insert(cap);
            }
        }
        Self { granted, denied }
    }

    pub fn has(&self, cap: Capability) -> bool {
        self.granted.contains(&cap) && !self.denied.contains(&cap)
    }

    pub fn require(&self, cap: Capability) -> Result<(), PermissionDenied> {
        if self.has(cap) {
            Ok(())
        } else {
            Err(PermissionDenied { capability: cap })
        }
    }

    pub fn grant(&mut self, cap: Capability) {
        self.denied.remove(&cap);
        self.granted.insert(cap);
    }

    pub fn revoke(&mut self, cap: Capability) {
        self.granted.remove(&cap);
        self.denied.insert(cap);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionDenied {
    pub capability: Capability,
}

impl std::fmt::Display for PermissionDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "permission denied: {:?} ({})",
            self.capability,
            self.capability.description()
        )
    }
}

impl std::error::Error for PermissionDenied {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_capabilities_are_denied_by_default() {
        let requested = [
            Capability::Ui,
            Capability::Theme,
            Capability::TerminalRead,
            Capability::Network,
            Capability::ProcessSpawn,
            Capability::AudioCapture,
        ];
        let perms = PermissionSet::safe_default(&requested, &[]);
        assert!(perms.has(Capability::Ui));
        assert!(perms.has(Capability::Theme));
        assert!(!perms.has(Capability::TerminalRead));
        assert!(!perms.has(Capability::Network));
        assert!(!perms.has(Capability::ProcessSpawn));
        assert!(!perms.has(Capability::AudioCapture));
        assert!(perms.require(Capability::Network).is_err());
    }

    #[test]
    fn explicit_user_approval_grants_sensitive_capability() {
        let requested = [Capability::Ui, Capability::AudioCapture];
        let perms = PermissionSet::safe_default(&requested, &[Capability::AudioCapture]);
        assert!(perms.has(Capability::AudioCapture));
    }
}
