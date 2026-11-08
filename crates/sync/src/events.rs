//! Push-only progress (P1.5): sr-sync emits `sr_common::events::EngineEvent`s into an
//! [`EventSink`]; the sink assigns the `seq` (the engine's `EventBus`, or
//! `sr_common::events::EventLog` directly). Nothing here is polled by a UI.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sr_common::events::{EngineEvent, EventLog};
use sr_common::ids::FolderId;

/// Where sr-sync pushes events. The engine implements it for its `EventBus` (sequenced, fanned
/// out to pipe subscribers).
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: EngineEvent);
}

/// `EventLog::publish` assigns the strictly monotonic `seq` under the ring lock.
impl EventSink for EventLog<EngineEvent> {
    fn emit(&self, ev: EngineEvent) {
        self.publish(ev);
    }
}

/// Discards events (tools, tests that don't look at events).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _ev: EngineEvent) {}
}

/// Per-key rate limiter for progress events (`ScanProgress`, `IndexProgress`, `QueueDepth`).
#[derive(Debug)]
pub(crate) struct Throttle {
    every: Duration,
    last: Mutex<HashMap<Option<FolderId>, Instant>>,
}

impl Throttle {
    pub(crate) fn new(every: Duration) -> Self {
        Self {
            every,
            last: Mutex::new(HashMap::new()),
        }
    }

    /// True when an event for `key` may be emitted now (and records it).
    pub(crate) fn ready(&self, key: Option<FolderId>) -> bool {
        let now = Instant::now();
        let mut g = match self.last.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match g.get(&key) {
            Some(t) if now.duration_since(*t) < self.every => false,
            _ => {
                g.insert(key, now);
                true
            }
        }
    }
}
