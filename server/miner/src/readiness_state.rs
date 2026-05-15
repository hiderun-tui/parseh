//! Readiness state machine — production wiring of
//! [`parseh_core::ReadinessState`] per
//! the project notes §3.4.
//!
//! The miner owns a single [`ReadinessTracker`] that observes startup
//! events (swarm built, first peer discovered, first capability
//! advertised, first task accepted, resource pressure cleared) and
//! transitions the local state accordingly. The gossipsub `caps_tick`
//! reads the current state and embeds it in every outbound
//! `CapabilityAdvertisement`.
//!
//! Concurrency: a single `parking_lot::Mutex<ReadinessState>` wrapped
//! in `Arc`. Transitions are cheap (atomic enum swap) and the read hot
//! path (in `publish_caps_v0_2`) takes the lock for a single load+copy.

use std::sync::Arc;

use parking_lot::Mutex;
use parseh_core::peer_registry::ReadinessState;
use tracing::info;

/// Concurrent handle to the local node's readiness state.
///
/// Clone is cheap — one `Arc` bump. Hand copies to every task that
/// needs to read or transition the state.
#[derive(Debug, Clone)]
pub struct ReadinessTracker {
    state: Arc<Mutex<ReadinessState>>,
    /// In-flight task count — the floor of [`ReadinessState::Active`].
    /// When this drops to zero we step back to [`ReadinessState::Ready`].
    in_flight: Arc<Mutex<usize>>,
}

impl ReadinessTracker {
    /// Construct a tracker in the [`ReadinessState::Initialised`] state.
    /// This is the post-`parseh-miner init` value before the swarm is
    /// built.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ReadinessState::Initialised)),
            in_flight: Arc::new(Mutex::new(0)),
        }
    }

    /// Snapshot the current state.
    #[inline]
    pub fn current(&self) -> ReadinessState {
        *self.state.lock()
    }

    /// Move to [`ReadinessState::Connected`]. Called once the libp2p
    /// `SwarmBuilder` returns successfully.
    pub fn mark_connected(&self) {
        self.transition_to(ReadinessState::Connected, "swarm built");
    }

    /// Move to [`ReadinessState::Listening`]. Called when the first Kad
    /// peer is observed (DHT-discovery vs first dial-and-handshake).
    pub fn mark_listening(&self) {
        // Only step up — if we are already past Listening (e.g. Ready)
        // the discovery of a fresh peer should not regress us.
        let mut s = self.state.lock();
        if (*s as u8) < (ReadinessState::Listening as u8) {
            *s = ReadinessState::Listening;
            info!(state = ?*s, "readiness · listening");
        }
    }

    /// Move to [`ReadinessState::Ready`]. Called the first time we
    /// publish our own [`parseh_core::CapabilityAdvertisement`] on
    /// `parseh.caps.v1`.
    pub fn mark_ready(&self) {
        let mut s = self.state.lock();
        // Allow Ready ← Active when in_flight drops to zero, but do not
        // regress from Degraded/Stopped silently.
        if matches!(
            *s,
            ReadinessState::Initialised
                | ReadinessState::Connected
                | ReadinessState::Listening
        ) {
            *s = ReadinessState::Ready;
            info!(state = ?*s, "readiness · ready");
        }
    }

    /// Increment the in-flight count and transition to
    /// [`ReadinessState::Active`].
    ///
    /// Returns the new in-flight count.
    pub fn task_started(&self) -> usize {
        let mut g = self.in_flight.lock();
        *g += 1;
        let new = *g;
        drop(g);
        let mut s = self.state.lock();
        if matches!(
            *s,
            ReadinessState::Ready | ReadinessState::Listening | ReadinessState::Connected
        ) {
            *s = ReadinessState::Active;
            info!(state = ?*s, in_flight = new, "readiness · active");
        }
        new
    }

    /// Decrement the in-flight count and, if it reaches zero, step
    /// back to [`ReadinessState::Ready`].
    pub fn task_finished(&self) -> usize {
        let mut g = self.in_flight.lock();
        if *g > 0 {
            *g -= 1;
        }
        let new = *g;
        drop(g);
        if new == 0 {
            let mut s = self.state.lock();
            if matches!(*s, ReadinessState::Active) {
                *s = ReadinessState::Ready;
                info!(state = ?*s, "readiness · idle (back to ready)");
            }
        }
        new
    }

    /// Flag CPU / memory / network pressure. The peer-matchmaking logic
    /// filters [`ReadinessState::Degraded`] peers out of new-work
    /// selection per §3.4.
    pub fn mark_degraded(&self, reason: &str) {
        let mut s = self.state.lock();
        if !matches!(*s, ReadinessState::Stopped) {
            *s = ReadinessState::Degraded;
            info!(state = ?*s, %reason, "readiness · degraded");
        }
    }

    /// Pressure cleared — return to Active (if we have in-flight work)
    /// or Ready (if not).
    pub fn mark_recovered(&self) {
        let in_flight = *self.in_flight.lock();
        let mut s = self.state.lock();
        if matches!(*s, ReadinessState::Degraded) {
            *s = if in_flight > 0 {
                ReadinessState::Active
            } else {
                ReadinessState::Ready
            };
            info!(state = ?*s, in_flight, "readiness · recovered");
        }
    }

    /// Graceful shutdown observed. Terminal — peers will see this on
    /// the wire one last time before we drop the swarm.
    pub fn mark_stopped(&self) {
        let mut s = self.state.lock();
        *s = ReadinessState::Stopped;
        info!(state = ?*s, "readiness · stopped");
    }

    fn transition_to(&self, target: ReadinessState, reason: &str) {
        let mut s = self.state.lock();
        let prev = *s;
        *s = target;
        info!(prev = ?prev, next = ?target, %reason, "readiness · transition");
    }
}

impl Default for ReadinessTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_initialised() {
        let t = ReadinessTracker::new();
        assert_eq!(t.current(), ReadinessState::Initialised);
    }

    #[test]
    fn lifecycle_walks_through_states() {
        let t = ReadinessTracker::new();
        t.mark_connected();
        assert_eq!(t.current(), ReadinessState::Connected);
        t.mark_listening();
        assert_eq!(t.current(), ReadinessState::Listening);
        t.mark_ready();
        assert_eq!(t.current(), ReadinessState::Ready);
        assert_eq!(t.task_started(), 1);
        assert_eq!(t.current(), ReadinessState::Active);
        assert_eq!(t.task_started(), 2);
        assert_eq!(t.task_finished(), 1);
        // Still Active — one in-flight remains.
        assert_eq!(t.current(), ReadinessState::Active);
        assert_eq!(t.task_finished(), 0);
        // All tasks done — back to Ready.
        assert_eq!(t.current(), ReadinessState::Ready);
    }

    #[test]
    fn degraded_path() {
        let t = ReadinessTracker::new();
        t.mark_ready();
        t.mark_degraded("cpu>90%");
        assert_eq!(t.current(), ReadinessState::Degraded);
        t.mark_recovered();
        assert_eq!(t.current(), ReadinessState::Ready);
    }

    #[test]
    fn mark_listening_does_not_regress_from_ready() {
        let t = ReadinessTracker::new();
        t.mark_connected();
        t.mark_listening();
        t.mark_ready();
        t.mark_listening(); // no-op; we are already past Listening
        assert_eq!(t.current(), ReadinessState::Ready);
    }

    #[test]
    fn stop_is_terminal() {
        let t = ReadinessTracker::new();
        t.mark_ready();
        t.mark_stopped();
        // Other transitions after stop must not move us back. We don't
        // enforce immutability inside the tracker (the binary owns the
        // lifecycle), but we do test that mark_degraded after stop is
        // a no-op.
        t.mark_degraded("late event");
        assert_eq!(t.current(), ReadinessState::Stopped);
    }
}
