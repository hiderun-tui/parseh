//! Hiderun desktop client — Tauri entry point.
//!
//! The Rust side exposes `#[tauri::command]` functions that the TypeScript
//! frontend invokes via the Tauri IPC bridge. The actual network worker
//! lives inside `parseh_sdk::Client`; this file is just the desktop shell.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use parseh_sdk::Client;
use tauri::{Manager, State};
use tracing::info;

/// Tauri-managed application state. One `Client` per app instance.
struct AppState {
    client: Arc<Client>,
}

/// Returns the `parseh-sdk` crate version. Lets the frontend confirm the
/// Rust side is alive and which build is running.
#[tauri::command]
fn sdk_version() -> String {
    parseh_sdk::sdk_version()
}

/// Returns the default `ClientConfig` as JSON so the frontend has a
/// template to edit and persist.
#[tauri::command]
fn default_config() -> String {
    parseh_sdk::default_client_config()
}

/// Begins connecting the long-lived `Client`. Non-blocking.
#[tauri::command]
fn connect_to_network(state: State<AppState>) -> Result<String, String> {
    state.client.connect();
    let status = state.client.status();
    info!(state = ?status.state, "connect invoked");
    Ok(format!("state={:?} peers={}", status.state, status.peers))
}

/// Disconnects the network worker.
#[tauri::command]
fn disconnect_from_network(state: State<AppState>) -> Result<String, String> {
    state.client.disconnect();
    Ok("disconnected".into())
}

/// Reads a fresh `NetworkStatus` snapshot for the status bar.
#[tauri::command]
fn network_status(state: State<AppState>) -> Result<serde_json::Value, String> {
    let s = state.client.status();
    Ok(serde_json::json!({
        "state":      format!("{:?}", s.state),
        "peers":      s.peers,
        "bytes_in":   s.bytes_in,
        "bytes_out":  s.bytes_out,
        "last_error": s.last_error,
    }))
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hiderun=info,parseh_sdk=info".into()),
        )
        .init();

    let cfg = parseh_sdk::default_client_config();
    let client: Arc<Client> = Client::new(cfg);  // SDK already returns Arc

    tauri::Builder::default()
        .manage(AppState { client })
        .setup(|app| {
            info!(window_count = app.webview_windows().len(), "Hiderun starting");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            sdk_version,
            default_config,
            connect_to_network,
            disconnect_from_network,
            network_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Hiderun");
}
