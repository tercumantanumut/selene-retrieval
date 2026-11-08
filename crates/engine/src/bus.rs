//! The one event bus (docs/02 §2.5 "Events"): sequencing + replay ring + live broadcast, built on
//! [`crate::pipe::EventHub`] so pipe subscribers (app-RPC) get live events in seq order with
//! resume / re-snapshot. In-process consumers (MCP progress, `resources/updated`) subscribe to the
//! same broadcast.

use sr_common::events::{Cursor, DEFAULT_RING_CAPACITY, EngineEvent, Replay, Seq, Snapshot};
use tokio::sync::broadcast;

use crate::pipe::EventHub;

/// The engine's event bus.
pub struct EventBus {
    hub: EventHub<EngineEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self {
            hub: EventHub::new(DEFAULT_RING_CAPACITY),
        }
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("cursor", &self.hub.cursor())
            .finish()
    }
}

impl EventBus {
    /// Sequence, keep in the ring and broadcast.
    pub fn publish(&self, ev: EngineEvent) -> Seq<EngineEvent> {
        self.hub.publish(ev)
    }
    /// Snapshot taken under the bus lock, so `seq` exactly matches the state captured by `f`.
    pub fn snapshot<S>(&self, f: impl FnOnce() -> S) -> Snapshot<S> {
        self.hub.snapshot(f)
    }
    pub fn replay_since(&self, cursor: Cursor) -> Replay<EngineEvent> {
        self.hub.replay_since(cursor)
    }
    /// Live events from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Seq<EngineEvent>> {
        self.hub.subscribe()
    }
    pub fn cursor(&self) -> Cursor {
        self.hub.cursor()
    }
    /// The hub the pipe server serves.
    pub fn hub(&self) -> &EventHub<EngineEvent> {
        &self.hub
    }
}

/// sr-sync pushes its events (scan/index progress, folder status, queue depth) through the bus,
/// which assigns the `seq`.
impl sr_sync::EventSink for EventBus {
    fn emit(&self, ev: EngineEvent) {
        self.publish(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_then_live_events_without_gaps() {
        let bus = EventBus::default();
        bus.publish(EngineEvent::SettingsChanged);
        let snap = bus.snapshot(|| ());
        assert_eq!(snap.seq, 1);
        let mut rx = bus.subscribe();
        bus.publish(EngineEvent::SettingsChanged);
        assert_eq!(rx.recv().await.map(|e| e.seq).ok(), Some(2));
        match bus.replay_since(snap.cursor()) {
            Replay::Events(v) => assert_eq!(v.iter().map(|e| e.seq).collect::<Vec<_>>(), [2]),
            Replay::Resnapshot => panic!("expected events"),
        }
    }
}
