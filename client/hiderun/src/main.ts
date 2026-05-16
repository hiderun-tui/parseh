// Hiderun frontend — TypeScript shell that talks to the Rust `parseh-sdk`
// via Tauri's IPC bridge.
//
// Today this is a status panel: it polls `network_status` every 2s and
// updates the dot indicator. Click "Connect" to start the worker.

import { invoke } from '@tauri-apps/api/core';

interface NetworkStatus {
  state: 'Disconnected' | 'Connecting' | 'Connected' | 'Reconnecting' | 'Failed';
  peers: number;
  bytes_in: number;
  bytes_out: number;
  last_error: string | null;
}

const btn = document.getElementById('btn') as HTMLButtonElement | null;
const stateEl = document.getElementById('state') as HTMLSpanElement | null;
const log = document.getElementById('log') as HTMLPreElement | null;

let connected = false;
let pollHandle: number | null = null;

function append(line: string) {
  if (!log) return;
  log.textContent = `${log.textContent ?? ''}\n${line}`.trim();
  log.scrollTop = log.scrollHeight;
}

function paintState(s: NetworkStatus) {
  if (stateEl) {
    stateEl.textContent = `${s.state.toLowerCase()} · ${s.peers} peer${s.peers === 1 ? '' : 's'}`;
  }
}

async function pollStatus() {
  try {
    const s = await invoke<NetworkStatus>('network_status');
    paintState(s);
  } catch (e) {
    append(`status poll failed: ${String(e)}`);
  }
}

async function toggleConnect() {
  if (!btn) return;
  btn.disabled = true;
  try {
    if (!connected) {
      append('$ invoke("connect_to_network")');
      const reply = await invoke<string>('connect_to_network');
      append(`✓ ${reply}`);
      connected = true;
      btn.textContent = 'Disconnect';
      if (pollHandle === null) {
        pollHandle = window.setInterval(pollStatus, 2000) as unknown as number;
      }
    } else {
      append('$ invoke("disconnect_from_network")');
      const reply = await invoke<string>('disconnect_from_network');
      append(`✓ ${reply}`);
      connected = false;
      btn.textContent = 'Connect to PARSEH';
      if (pollHandle !== null) {
        clearInterval(pollHandle);
        pollHandle = null;
      }
    }
  } catch (err) {
    if (stateEl) stateEl.textContent = 'error';
    append(`✗ ${String(err)}`);
  } finally {
    btn.disabled = false;
  }
}

btn?.addEventListener('click', () => { void toggleConnect(); });

// Boot: print SDK version + default config so the contributor sees the
// IPC bridge is alive before they click anything.
(async () => {
  try {
    const v = await invoke<string>('sdk_version');
    append(`parseh_sdk: v${v}`);
  } catch (err) {
    append(`parseh_sdk: not wired (${String(err)})`);
  }
  try {
    const cfg = await invoke<string>('default_config');
    append(`config: ${cfg.split('\n')[0]}…`);
  } catch (err) {
    append(`config: ${String(err)}`);
  }
  await pollStatus();
})();
