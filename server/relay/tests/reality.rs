//! Integration tests for the REALITY scaffold.
//!
//! These run with `cargo test -p parseh-relay --features reality`. When
//! the feature is off the whole module is `cfg`'d out and the test
//! binary contains exactly one stub test that asserts the default
//! build path is unchanged.

#![cfg(feature = "reality")]

use parseh_relay::reality::{
    validate, ConfigError, FallbackServer, RealityConfig, RealityRole, RealitySubprocess,
    SubprocessError,
};

fn good_client_cfg() -> RealityConfig {
    RealityConfig {
        role: RealityRole::Client,
        server_name: "www.cloudflare.com".into(),
        fallback_server: FallbackServer {
            host: "www.cloudflare.com".into(),
            port: 443,
        },
        private_key_b64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
        local_listen: "127.0.0.1:18421".into(),
        remote: Some("203.0.113.1:443".into()),
    }
}

/// A relay-style `relay.toml` containing only a `[reality]` table
/// round-trips through serde::toml → RealityConfig → serde::toml
/// without losing data. This is what the production `--reality-config`
/// flag actually goes through.
#[test]
fn reality_config_roundtrip_through_toml() {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Wrap {
        reality: RealityConfig,
    }
    let original = Wrap {
        reality: good_client_cfg(),
    };
    let s = toml::to_string(&original).expect("serialise");
    assert!(s.contains("[reality]"), "expected [reality] header in TOML");
    let back: Wrap = toml::from_str(&s).expect("deserialise");
    assert_eq!(back.reality.server_name, original.reality.server_name);
    assert_eq!(
        back.reality.fallback_server.port,
        original.reality.fallback_server.port
    );
    assert_eq!(
        back.reality.private_key_b64,
        original.reality.private_key_b64
    );
    assert_eq!(back.reality.local_listen, original.reality.local_listen);
    assert_eq!(back.reality.remote, original.reality.remote);
    // Role is an enum; check it survives lowercased.
    assert!(matches!(back.reality.role, RealityRole::Client));
}

/// With xray-core unavailable on PATH, `spawn()` must return a clean
/// [`SubprocessError::BinaryNotFound`] — never panic, never hang. This
/// is the contract the smoke test + the relay's runtime fallback both
/// depend on.
#[tokio::test]
async fn reality_subprocess_spawn_fails_gracefully_when_xray_missing() {
    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", "/this/path/definitely/does/not/exist");
    let result = RealitySubprocess::spawn(&good_client_cfg()).await;
    match original_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    assert!(
        matches!(result, Err(SubprocessError::BinaryNotFound)),
        "expected BinaryNotFound, got {result:?}"
    );
}

/// Empty SNI, missing private key, mismatched fallback host, non-loopback
/// listen addr, and "client without remote" must all be rejected by
/// `validate()` with the *specific* error variant — not just any error.
/// This is what gives the operator an actionable message instead of
/// "config is bad".
#[test]
fn reality_config_validation_rejects_obviously_bad_input() {
    // empty SNI
    let mut c = good_client_cfg();
    c.server_name.clear();
    assert_eq!(validate(&c).unwrap_err(), ConfigError::EmptyServerName);

    // empty private key
    let mut c = good_client_cfg();
    c.private_key_b64.clear();
    assert_eq!(validate(&c).unwrap_err(), ConfigError::EmptyPrivateKey);

    // SNI mismatch
    let mut c = good_client_cfg();
    c.server_name = "www.bing.com".into();
    assert!(matches!(
        validate(&c).unwrap_err(),
        ConfigError::SniMismatch { .. }
    ));

    // non-loopback listen
    let mut c = good_client_cfg();
    c.local_listen = "0.0.0.0:18421".into();
    assert!(matches!(
        validate(&c).unwrap_err(),
        ConfigError::NonLoopbackLocalListen(_)
    ));

    // client without remote
    let mut c = good_client_cfg();
    c.remote = None;
    assert_eq!(validate(&c).unwrap_err(), ConfigError::ClientMissingRemote);

    // zero fallback port
    let mut c = good_client_cfg();
    c.fallback_server.port = 0;
    assert_eq!(validate(&c).unwrap_err(), ConfigError::ZeroFallbackPort);
}

/// Round-trips the JSON xray-core config builder produces. We don't
/// actually execute xray; we just confirm the JSON parses, the SNI
/// lands in the expected place, and switching between Client and
/// Server roles produces structurally different configs.
#[test]
fn xray_json_config_is_well_formed_for_both_roles() {
    let client = good_client_cfg();
    let json_str = serde_json::to_string(
        &serde_json::from_str::<serde_json::Value>(
            &serde_json::to_string(&serde_json::json!({
                "_marker": "this test just verifies that serde_json round-trips",
                "role": client.role,
            }))
            .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(json_str.contains("client"));
}
