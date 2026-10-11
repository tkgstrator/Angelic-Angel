//! Receiver runtime state shared between the listener and the HTTP API.
//!
//! Only counters, coarse states and short fixed messages are kept here; payloads
//! and raw provider errors never enter this structure.

use serde::Serialize;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiverState {
    Stopped,
    Connecting,
    Connected,
    Retrying,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Registering,
    Unregistering,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Counters {
    pub connected_since: Option<u64>,
    pub last_notification_at: Option<u64>,
    pub received_count: u64,
    pub forwarded_count: u64,
    pub failed_count: u64,
    pub retry_count: u64,
    pub last_error: Option<String>,
}

#[derive(Debug)]
struct Inner {
    state: ReceiverState,
    operation: Option<Operation>,
    counters: Counters,
}

#[derive(Debug)]
pub struct Telemetry {
    inner: Mutex<Inner>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: ReceiverState::Stopped,
                operation: None,
                counters: Counters {
                    connected_since: None,
                    last_notification_at: None,
                    received_count: 0,
                    forwarded_count: 0,
                    failed_count: 0,
                    retry_count: 0,
                    last_error: None,
                },
            }),
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Telemetry {
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(&mut self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn snapshot(&self) -> (ReceiverState, Option<Operation>, Counters) {
        self.with(|i| (i.state, i.operation, i.counters.clone()))
    }

    pub fn set_operation(&self, operation: Option<Operation>) {
        self.with(|i| i.operation = operation);
    }

    pub fn set_stopped(&self) {
        self.with(|i| {
            i.state = ReceiverState::Stopped;
            i.counters.connected_since = None;
        });
    }

    pub fn set_connecting(&self) {
        self.with(|i| {
            i.state = ReceiverState::Connecting;
            i.counters.connected_since = None;
        });
    }

    pub fn set_connected(&self) {
        self.with(|i| {
            i.state = ReceiverState::Connected;
            i.counters.connected_since = Some(now());
        });
    }

    /// Marks a (re)connection attempt as pending; `error` must be a fixed safe message.
    pub fn set_retrying(&self, error: &str) {
        self.with(|i| {
            i.state = ReceiverState::Retrying;
            i.counters.connected_since = None;
            i.counters.retry_count += 1;
            i.counters.last_error = Some(error.to_string());
        });
    }

    pub fn set_error(&self, error: &str) {
        self.with(|i| {
            i.state = ReceiverState::Error;
            i.counters.connected_since = None;
            i.counters.last_error = Some(error.to_string());
        });
    }

    pub fn record_received(&self) {
        self.with(|i| {
            i.counters.received_count += 1;
            i.counters.last_notification_at = Some(now());
        });
    }

    pub fn record_forwarded(&self) {
        self.with(|i| i.counters.forwarded_count += 1);
    }

    pub fn record_failed(&self, error: &str) {
        self.with(|i| {
            i.counters.failed_count += 1;
            i.counters.last_error = Some(error.to_string());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_states_track_events() {
        let t = Telemetry::default();
        t.set_connecting();
        t.set_connected();
        t.record_received();
        t.record_forwarded();
        t.record_received();
        t.record_failed("webhook delivery failed");
        t.set_retrying("connection lost");
        let (state, op, c) = t.snapshot();
        assert_eq!(state, ReceiverState::Retrying);
        assert!(op.is_none());
        assert_eq!(c.received_count, 2);
        assert_eq!(c.forwarded_count, 1);
        assert_eq!(c.failed_count, 1);
        assert_eq!(c.retry_count, 1);
        assert!(c.connected_since.is_none());
        assert!(c.last_notification_at.is_some());
        assert_eq!(c.last_error.as_deref(), Some("connection lost"));
    }
}
