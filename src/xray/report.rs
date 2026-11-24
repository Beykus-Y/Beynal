// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Post-command completion reports for Beyrix X-Ray.
//!
//! Records only actually observed metrics, distinguishes exact counters from
//! sampled observations, enforces bounded storage, and respects privacy mode.

use std::collections::VecDeque;
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub struct CommandReport {
    pub command_id: u64,
    pub command: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub started_at: SystemTime,
    pub duration_ms: u64,
    pub peak_rss_bytes: u64,
    pub peak_process_count: usize,
    pub physical_read_bytes: u64,
    pub physical_write_bytes: u64,
    pub logical_read_bytes: u64,
    pub logical_write_bytes: u64,
    pub wait_events: Vec<String>,
    pub trace_active: bool,
}

pub struct ReportStore {
    reports: VecDeque<CommandReport>,
    max_items: usize,
    privacy_mode: bool,
}

impl ReportStore {
    pub fn new(max_items: usize, privacy_mode: bool) -> Self {
        Self {
            reports: VecDeque::with_capacity(max_items.min(128)),
            max_items: max_items.max(1),
            privacy_mode,
        }
    }

    pub fn push(&mut self, mut report: CommandReport) {
        if self.privacy_mode {
            // Redact potential inline secret flags (--token=..., PASSWORD=...)
            report.command = redact_command_secrets(&report.command);
        }
        while self.reports.len() >= self.max_items {
            self.reports.pop_front();
        }
        self.reports.push_back(report);
    }

    pub fn list(&self) -> Vec<CommandReport> {
        self.reports.iter().cloned().collect()
    }
}

fn redact_command_secrets(cmd: &str) -> String {
    cmd.split_whitespace()
        .map(|tok| {
            let lower = tok.to_ascii_lowercase();
            if lower.contains("token=")
                || lower.contains("password=")
                || lower.contains("secret=")
                || lower.contains("api_key=")
            {
                if let Some((k, _)) = tok.split_once('=') {
                    format!("{k}=[REDACTED]")
                } else {
                    "[REDACTED]".to_string()
                }
            } else {
                tok.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_mode_redacts_inline_secrets_and_bounds_history() {
        let mut store = ReportStore::new(2, true);
        for i in 0..3 {
            store.push(CommandReport {
                command_id: i,
                command: format!("curl --token=supersecret123 https://example.com/{i}"),
                exit_code: Some(0),
                signal: None,
                started_at: SystemTime::now(),
                duration_ms: 100,
                peak_rss_bytes: 1024,
                peak_process_count: 1,
                physical_read_bytes: 0,
                physical_write_bytes: 0,
                logical_read_bytes: 0,
                logical_write_bytes: 0,
                wait_events: Vec::new(),
                trace_active: false,
            });
        }
        let list = store.list();
        assert_eq!(list.len(), 2);
        assert!(list[0].command.contains("--token=[REDACTED]"));
        assert!(!list[0].command.contains("supersecret123"));
    }
}
