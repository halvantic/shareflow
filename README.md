<div align="center">

# ShareFlow 🖱️⌨️

### Seamless software KVM to share one keyboard and mouse across multiple PCs.

[![Rust](https://img.shields.io/badge/Rust-1.77%2B-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org/)
[![Tauri](https://img.shields.io/badge/Tauri-v2-blue?style=for-the-badge&logo=tauri)](https://tauri.app/)
[![React](https://img.shields.io/badge/React-18-61DAFB?style=for-the-badge&logo=react)](https://react.dev/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=for-the-badge)](https://opensource.org/licenses/MIT)
[![GitHub Stars](https://img.shields.io/github/stars/halvantic/shareflow?style=social)](https://github.com/halvantic/shareflow)

<p align="center">
  <a href="#-overview">Overview</a> •
  <a href="#-key-features">Key Features</a> •
  <a href="#-how-it-works">How It Works</a> •
  <a href="#-protocol">Protocol</a> •
  <a href="#-installation--building">Installation</a> •
  <a href="#-security--networking">Security</a> •
  <a href="#-project-structure">Project Structure</a> •
  <a href="#-license">License</a>
</p>

<!-- Optional: Add a demo GIF or screenshot below once available -->
<!-- ![ShareFlow Demo](https://raw.githubusercontent.com/halvantic/shareflow/main/docs/demo.gif) -->

</div>

---

## ⚡ Overview

**ShareFlow** is a lightweight, high-performance software KVM switch built with **Rust** and **Tauri**. It allows you to control multiple Windows and macOS computers using a single keyboard and mouse over your local network.

Move your cursor to the edge of your screen, and it seamlessly transitions to the neighboring machine—no hardware switch, no extra dongles, and zero manual IP configuration required.

---

## ✨ Key Features

- **🎯 Directional Edge Switching:** Push the cursor to any screen edge to transfer focus to the next machine. Features a 45° directional gate to prevent accidental triggers during horizontal dragging.
- **📋 Automatic Clipboard Sync:** Text and images transfer automatically between machines as soon as focus switches.
- **📁 Drag-and-Drop File Transfer:** Stream files directly across machines via chunked network transfers.
- **🔍 Zero-Config LAN Discovery:** Peers appear automatically on your subnet using local UDP discovery.
- **🔒 Encrypted Traffic:** All TCP communication is encrypted using TLS with Trust-On-First-Use (TOFU) certificate pinning.
- **📐 Multi-Monitor Proportional Mapping:** Accurately maps cursor position across displays of different sizes and resolutions (e.g., crossing at 30% from the top on one screen places the cursor at 30% on the target screen).
- **⌨️ Hardware Hotkey Return:** Press `Scroll Lock` at any time to instantly snap control back to the local host.
- **🖥️ Cross-Platform Support:** Primary controller on Windows, peer on macOS (Linux experimental).

---

## 🛠️ How It Works

### Discovery
On launch, each machine broadcasts a UDP announcement on port `24801` with a magic header (`SFLO`). Other machines on the same subnet listen for these broadcasts and surface discovered peers in the UI. Announcements are timestamped to prevent replay attacks and expire after 30 seconds.

### Connection
Once a peer is accepted, a TLS TCP connection is established on port `24800`. On first connect, the certificate fingerprint is stored (Trust-On-First-Use). Subsequent connections verify against the stored fingerprint. All protocol messages are encoded as length-prefixed `bincode` frames.

### Focus & Edge Switching
Each machine tracks which machine currently has "focus" (the machine receiving physical input).
1. When the cursor reaches the desktop boundary, edge detection fires if movement is predominantly toward the edge (45° angle check).
2. A `SwitchFocus` message is sent to the target peer with cursor entry coordinates.
3. The sending machine suppresses local input (cursor hidden, events blocked).
4. The receiving machine injects the cursor at the mapped entry point and begins processing forwarded events.
5. A 300ms cooldown prevents oscillation after any switch.

### Input Capture & Injection
- **Windows (Sender/Primary):** Low-level `WH_MOUSE_LL` and `WH_KEYBOARD_LL` hooks capture input before it reaches applications. A warp-to-center technique keeps the physical cursor stationary while computing virtual deltas. The cursor is hidden via `ShowCursor`.
- **macOS (Peer/Receiver):** `CGEventTap` at `kCGHIDEventTap` captures input with full suppression capabilities. Injection uses `CGWarpMouseCursorPosition` and `CGEventCreateMouseEvent` with absolute and relative delta fields set (required for 3D viewports and dragging). Modifier key states are tracked independently to prevent stuck modifiers.

### Clipboard Sync
When focus switches to a remote machine, the local clipboard is pushed immediately so `Ctrl+V` works straight away on the remote machine. Both text and image clipboard content are supported.

---

## 📡 Protocol

All messages are serialised with `bincode` and framed with a 4-byte big-endian length prefix. Key message types include:

| Message | Description |
| :--- | :--- |
| `Hello` / `HelloAck` | Handshake, exchanges peer ID, name, and screen layout |
| `MouseMove` | Absolute cursor position |
| `MouseButton` | Button press/release |
| `MouseScroll` | Scroll delta |
| `Key` | Hardware scancode press/release |
| `SwitchFocus` | Trigger focus transition with entry coordinates |
| `ClipboardUpdate` | Clipboard content push |
| `FileStart` / `Chunk` / `Done` | Chunked file transfer |
| `Ping` / `Pong` | Keepalive (3 missed pongs closes the connection) |

---

## 🚀 Installation & Building

### Prerequisites

| Platform | Requirements |
| :--- | :--- |
| **All Platforms** | Rust (stable 1.77+), Node.js (18+) |
| **Windows** | Visual C++ Build Tools (Desktop development with C++), WebView2, WiX Toolset v3 *(for `.msi` builds only)* |
| **macOS** | Xcode Command Line Tools (`xcode-select --install`) |

### Development Setup

```bash
# Clone repository
git clone [https://github.com/halvantic/shareflow.git](https://github.com/halvantic/shareflow.git)
cd shareflow

# Install frontend dependencies
npm install

# Run in development mode
npm run tauri dev

🍏 macOS Accessibility PermissionOn macOS, ShareFlow requires Accessibility permission to capture and inject HID input events:Open System Settings > Privacy & Security > Accessibility.Add ShareFlow to the allowed list.Restart ShareFlow.Without this permission, the event tap will fail silently and input will not be captured or injected.🔒 Security & NetworkingShareFlow is designed for private local networks:PortProtocolPurpose24801UDPLAN Peer Broadcast & Discovery (magic header SFLO)24800TCP (TLS)Encrypted Peer Input, Clipboard & File Streaming (configurable)Replay Protection: Discovery broadcasts are timestamped and expire after 30 seconds.TOFU Pinning: Peer certificates are stored on first connection and verified continuously.   📁 Project Structure   Plaintextshareflow/
├── src/                        # React/TypeScript frontend (Tauri UI)
└── src-tauri/
    └── src/
        ├── core/
        │   ├── engine.rs       # Focus state machine, edge switching logic
        │   ├── screen.rs       # Edge detection, boundary validation
        │   ├── protocol.rs     # Message types, encode/decode
        │   ├── config.rs       # App configuration, neighbour layout
        │   └── hotkey.rs       # Scroll Lock hotkey detection
        ├── input/
        │   ├── windows.rs      # Windows low-level hooks (capture + injection)
        │   ├── macos.rs        # macOS CGEventTap (capture + injection)
        │   └── linux.rs        # Linux (experimental)
        ├── network/
        │   ├── discovery.rs    # UDP LAN broadcast discovery
        │   ├── server.rs       # TLS TCP server, message routing
        │   ├── connection.rs   # Framed message reader/writer
        │   └── tls.rs          # Certificate generation and pinning
        ├── clipboard/
        │   └── sync.rs         # Clipboard monitoring and sync
        └── file_transfer/
            ├── sender.rs       # Chunked streaming file sender
            └── receiver.rs     # File receiver with bounds checking
🤝 ContributingContributions are welcome! Please feel free to open an issue or submit a Pull Request.Fork the ProjectCreate your Feature Branch (git checkout -b feature/AmazingFeature)Commit your Changes (git commit -m 'Add some AmazingFeature')Push to the Branch (git push origin feature/AmazingFeature)Open a Pull Request📄 LicenseDistributed under the MIT License. See LICENSE for details.Enjoying ShareFlow? Give it a ⭐ on GitHub to support development!
