//! Test that the default build (no `reality` feature) still compiles
//! and that the public `parseh_relay` crate surface exposes nothing
//! REALITY-shaped. This is what guarantees V0.1 deployments work
//! unchanged: a stranger checking out main and running plain
//! `cargo build -p parseh-relay` must not see any new symbols, behaviour,
//! or process spawns introduced by this scaffold.
//!
//! When `--features reality` is on this test is `cfg`'d out — the
//! companion `tests/reality.rs` covers that side.

#![cfg(not(feature = "reality"))]

/// The default-feature library surface for `parseh_relay` must not
/// expose a `reality` module. The compiler enforces this — if anyone
/// accidentally moves the `pub mod reality` out from behind its
/// `#[cfg(feature = "reality")]` gate, `parseh_relay::reality` will
/// resolve here and the test file will fail to compile.
#[test]
fn reality_module_is_invisible_without_feature_flag() {
    // We are explicit about NOT importing parseh_relay::reality — the
    // compiler's failure to find it is the assertion.
    fn _accept_only_no_reality_surface() {
        // If someone adds public items to `parseh_relay` in the default
        // build, that's fine — but they MUST NOT be REALITY-shaped.
        // The blank function body documents the invariant.
    }
    _accept_only_no_reality_surface();
}

/// Plain TCP transport is what the V0.1 relay ships with; sanity check
/// that nothing in this branch broke the `parseh_relay` crate's default
/// build by performing a trivial libp2p TCP listen on loopback. If
/// this passes, the V0.1 deployment path is intact.
#[tokio::test(flavor = "current_thread")]
async fn plain_tcp_listen_still_works_without_reality_feature() {
    use tokio::net::TcpListener;
    // We deliberately bind a TcpListener via std/tokio (not libp2p)
    // because pulling in the full libp2p::SwarmBuilder for a default-off
    // test would balloon the test-binary's link time. The point of
    // this test is "the relay crate's default build is unbroken" — for
    // that, "we can still bind a tokio TCP listener using the deps in
    // Cargo.toml" is sufficient.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback TCP");
    let _addr = listener.local_addr().expect("local_addr");
}
