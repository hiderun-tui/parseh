//! V0.2 gossipsub topic + request-response protocol constants.
//!
//! The miner subscribes to FOUR gossipsub topics in V0.2 and offers
//! TWO request-response protocols for direct, large-payload exchanges:
//!
//! | Topic                       | Purpose                                                            |
//! |-----------------------------|--------------------------------------------------------------------|
//! | `parseh.caps.v1`            | Capability advertisements (CBOR `CapabilityAdvertisement` in V0.2; |
//! |                             | legacy JSON `NodeCapabilities` is still decoded for V0.1 compat).  |
//! | `parseh.tasks.v1`           | `JobSpec` announcements so verifiers can find tasks.               |
//! | `parseh.verify.v1`          | `JobResult` + `JobVerification` envelopes — tag-byte multiplexed.  |
//! | `parseh.state-deltas.v1`    | Signed [`parseh_shared_state::StateDelta`] envelopes               |
//! |                             | (outcomes + reputation + governance).                              |
//!
//! For request-response:
//!
//! - `/parseh/job/1.0.0` — V0.1 legacy `JobOrder` ↔ `JobResult`.
//!   Inbound traffic on this protocol logs a deprecation warning;
//!   removal is planned for V0.2.5.
//! - `/parseh/job/2.0.0` — V0.2 `JobSpec` ↔ `JobResult` (the bulky
//!   completion text travels here so gossipsub stays small).
//! - `/parseh/state-sync/1.0.0` — anti-entropy pull. A peer that
//!   reconnected after isolation (or just joined) asks a
//!   better-connected peer for outcomes finalised since a cutoff. Closes
//!   the partition-recovery gap the chaos harness surfaced. See
//!   the project notes.
//!
//! See the project notes §2-4 for the
//! authoritative architecture diagram these constants implement.

/// libp2p Identify protocol version. V0.2 nodes advertise `0.2.0`.
pub const PARSEH_PROTOCOL_VERSION: &str = "/parseh/0.2.0";

/// Legacy request-response protocol — `JobOrder`/`JobResult` (V0.1).
///
/// Kept active for backward compatibility with V0.1 miners. The miner
/// logs `"received V0.1 JobOrder · deprecation pending"` when an
/// inbound 1.0.0 request arrives. Slated for removal in V0.2.5.
pub const PARSEH_JOB_PROTOCOL_V1: &str = "/parseh/job/1.0.0";

/// V0.2 request-response protocol — `JobSpec`/`JobResult`.
///
/// New in V0.2.1. Carries the bulky CBOR payloads (large completion
/// text, model output) directly between executor and submitter, so the
/// gossipsub envelope on `parseh.tasks.v1` stays well under the 1 MiB
/// `parseh_task::MAX_MESSAGE_SIZE_BYTES` cap.
pub const PARSEH_JOB_PROTOCOL_V2: &str = "/parseh/job/2.0.0";

/// V0.2.5 anti-entropy request-response protocol.
///
/// `/parseh/state-sync/1.0.0` carries
/// [`parseh_task::StateSyncRequest`] ↔ [`parseh_task::StateSyncResponse`].
/// A peer that detects it may have missed outcomes (reconnected after an
/// isolation window, or just joined) pulls the gap from a
/// better-connected peer. This closes the partition-recovery liveness
/// gap the chaos harness proved: gossipsub's IHAVE cache (200 ms
/// heartbeat × `mcache_len` ≈ a few seconds) is far too short to cover a
/// meaningful partition. See the project notes.
pub const PARSEH_STATE_SYNC_PROTOCOL_V1: &str = "/parseh/state-sync/1.0.0";

/// Window a peer assumes it might have missed when it reconnects after
/// isolation. The requester asks for `now - this` (or its newest known
/// outcome's timestamp, whichever is older). Being generous is cheap
/// (the responder clamps the count); missing an outcome is not.
pub const STATE_SYNC_MAX_PARTITION_WINDOW_SECS: u64 = 30 * 60;

/// Periodic anti-entropy backstop cadence. Independently of any
/// isolation event, every node asks one random Established peer for
/// "anything since `now - STATE_SYNC_BACKSTOP_LOOKBACK_SECS`" on this
/// interval — cheap insurance against a missed delta even when the node
/// was never fully isolated.
pub const STATE_SYNC_BACKSTOP_INTERVAL_SECS: u64 = 5 * 60;

/// Lookback the periodic backstop uses (10 min — generously covers the
/// 5 min cadence plus slack).
pub const STATE_SYNC_BACKSTOP_LOOKBACK_SECS: u64 = 10 * 60;

/// A node is considered to have been "isolated" if it spent at least
/// this long with zero connected peers. Reconnecting after such a gap
/// triggers a one-shot catch-up sync request.
pub const STATE_SYNC_ISOLATION_THRESHOLD_SECS: u64 = 30;

/// Capability advertisement topic. Wire format in V0.2 is CBOR
/// [`parseh_core::peer_registry::CapabilityAdvertisement`]; the legacy
/// JSON [`parseh_core::NodeCapabilities`] payload is still decoded as
/// a fallback so V0.1 nodes can talk to V0.2 nodes during the rolling
/// upgrade. Legacy publish path is dropped in V0.2.5.
pub const TOPIC_CAPS: &str = "parseh.caps.v1";

/// `JobSpec` announcement topic (V0.2 new).
pub const TOPIC_TASKS: &str = "parseh.tasks.v1";

/// `JobResult` + `JobVerification` envelopes — tag-byte multiplexed.
pub const TOPIC_VERIFY: &str = "parseh.verify.v1";

/// Signed [`parseh_shared_state::StateDelta`] envelopes
/// (outcomes / reputation / governance).
pub const TOPIC_STATE_DELTAS: &str = parseh_shared_state::GOSSIPSUB_TOPIC;

/// Leading tag byte: this `parseh.verify.v1` envelope carries a
/// signed `JobResult`.
pub const TAG_JOB_RESULT: u8 = 0x02;

/// Leading tag byte: this `parseh.verify.v1` envelope carries a
/// signed `JobVerification`.
pub const TAG_JOB_VERIFICATION: u8 = 0x03;

/// Default V0.2 advertised libp2p listen port.
pub const DEFAULT_LISTEN_PORT: u16 = 8421;

/// Hard-coded default SharedState DB filename, relative to the
/// `parseh-miner` data dir (`$HOME/.parseh/`). Overridable via
/// `--shared-state-db PATH`.
pub const DEFAULT_SHARED_STATE_FILENAME: &str = "shared-state.db";

/// Cadence of the periodic finalisation tick.
///
/// **Critical:** the testnet acceptance run showed that pure
/// event-driven finalisation deadlocks when every verification arrives
/// inside the `t_min` window: by the time `try_finalise` would unblock,
/// there is no fresh verification event to fire it. A 100 ms periodic
/// tick is the smallest interval that produces deterministic close
/// behaviour without burning CPU on idle nodes (verified empirically
/// at `parseh-testnet/src/node.rs`).
pub const FINALISE_TICK_MS: u64 = 100;
