# Hiderun (Client Apps)

The PARSEH user-facing apps. **One app per platform**, integrating browser, messenger, and wallet.

## What's in Hiderun

- **Browser**: web access tunneled through the PARSEH relay network
- **Messenger**: end-to-end encrypted chat (text, voice; eventually video)
- **Wallet**: view/send/receive PARSEH; NFC tap and QR pay
- **AI assistant** (V2): chat with local LLMs served by inference nodes

## Layout

```
hiderun/
  desktop/      Windows + Linux (Tauri: Rust + WebView2 / WebKitGTK)
  android/      Kotlin + Jetpack Compose + JNI to Rust core
  ios/          Swift + SwiftUI + FFI to Rust core
shared/         Cross-platform Rust core
  protocol/     Layer 4 service clients (relay, messaging, inference)
  wallet/       Wallet logic (key mgmt, tx signing, stealth addresses)
  crypto/       Signal protocol, libsodium bindings, key storage
  config/       App config, model preferences, bridge cache
  p2p/          libp2p client (or thin wrapper around server-side p2p)
```

## Why this layout

- **Single Rust core** across desktop and mobile reduces bugs and security surface
- **Native UI per platform** avoids Electron's footprint on desktop and gives full access to platform APIs (Android `VpnService`, iOS Network Extension, NFC HCE)
- **Tauri** for desktop: ~10 MB binary vs. ~200 MB Electron, smaller attack surface
- **Compose / SwiftUI** for mobile: modern toolkits, declarative UI, well-supported

## Tech stack per platform

| Platform | UI | Networking | NFC | Notes |
|---|---|---|---|---|
| Windows | Tauri + Webview2 | Rust core | Windows Hello / NFC SDK (limited) | MSI installer |
| Linux | Tauri + WebKitGTK | Rust core | libnfc / pcsclite | AppImage + .deb / .rpm |
| Android | Compose | Rust via JNI | Android `HostApduService` (HCE) | APK sideload + future Play Store |
| iOS | SwiftUI | Rust via Swift FFI | Core NFC (read only at user level; tap-to-pay needs Apple Pay entitlement) | TestFlight + future App Store |

## Wallet features

- View balance, transaction history
- Send / receive PARSEH
- **QR code** for receive (address + optional amount)
- **NFC tap** for in-person send:
  - Android: HCE emulates a payment card; recipient's phone acts as reader
  - iOS: read-only at user level; full tap-to-pay requires Apple entitlements
- Stealth-address support (default for received transfers)
- Backup: 24-word BIP-39 mnemonic, encrypted on-device

## Multi-mode behavior

The client adapts to network conditions automatically. Modes 0–4 described in the project notes. The user sees a status indicator:

```
🟢 Open Internet
🟡 Tunneled (via PARSEH bridge)
🟠 Throttled (low-bandwidth mode)
🔴 Local-only (no global net)
⚫ Offline / Mesh
```

Mode transitions are silent to the user except for the indicator change. Active sessions are migrated, not dropped.

## Build

When code exists:

- **Desktop**: `cargo tauri build`
- **Android**: `./gradlew assembleRelease` (Rust core built first via `cargo ndk`)
- **iOS**: `xcodebuild` (Rust core built first via `cargo lipo`)

## Status

Not started.

## Open questions

1. **iOS App Store distribution**: Apple's policies on circumvention tools are inconsistent. Sideloading via TestFlight is more reliable; full App Store presence may not be achievable.
2. **NFC peer-to-peer on iOS**: Apple restricts background NFC writes. Tap-to-pay between iOS users may require a workaround (BLE-confirmed QR exchange, for example).
3. **Browser engine**: WebView (per-platform) is simplest; bundling Chromium gives consistency at a binary-size cost.
