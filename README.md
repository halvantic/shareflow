<div align="center">

# ShareFlow 🖱️⌨️

### Seamless software KVM to share one keyboard and mouse across multiple PCs.

[![Rust](https://img.shields.io/badge/Rust-1.77%2B-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org/)
[![Tauri](https://img.shields.io/badge/Tauri-v2-blue?style=for-the-badge&logo=tauri)](https://tauri.app/)
[![React](https://img.shields.io/badge/React-18-61DAFB?style=for-the-badge&logo=react)](https://react.dev/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=for-the-badge)](https://opensource.org/licenses/MIT)
[![GitHub Stars](https://img.shields.io/github/stars/halvantic/shareflow?style=social)](https://github.com/halvantic/shareflow)

<p align="center">
  <a href="#key-features">Key Features</a> •
  <a href="#how-it-works">How It Works</a> •
  <a href="#installation--building">Installation</a> •
  <a href="#security">Security</a> •
  <a href="#license">License</a>
</p>

<!-- REPLACE THIS WITH A 15-SECOND GIF SHOWING CROSS-SCREEN CURSOR MOVEMENT -->
![ShareFlow Demo](https://raw.githubusercontent.com/halvantic/shareflow/main/docs/demo.gif)

</div>

---

## ⚡ Overview

**ShareFlow** is a lightweight, high-performance software KVM switch built with **Rust** and **Tauri**. It allows you to control multiple Windows and macOS computers using a single keyboard and mouse over your local network. 

Move your cursor to the edge of your monitor, and it seamlessly transitions to the neighboring machine—no hardware switches, no dongles, and zero manual IP configuration.

---

## ✨ Key Features

- **🎯 Directional Edge Switching:** Push your cursor past any screen edge to transfer focus instantly. Features a smart 45° directional gate to prevent accidental triggers during horizontal drags.
- **📋 Automatic Clipboard Sync:** Text and images transfer instantly between machines when focus changes.
- **📁 Drag-and-Drop File Transfer:** Stream files directly across network peers via chunked binary transfers.
- **🔍 Zero-Config LAN Discovery:** Automatic UDP peer discovery on your local network.
- **🔒 Encrypted Communication:** All TCP traffic is encrypted via TLS with Trust-On-First-Use (TOFU) certificate pinning.
- **📐 Multi-Monitor Proportional Mapping:** Accurately maps entry positions across displays with different resolutions and aspect ratios.
- **⌨️ Hardware Hotkey Return:** Press `Scroll Lock` at any time to instantly snap control back to the local host.
- **🖥️ Cross-Platform Support:** Windows as primary controller, macOS as peer (Linux experimental).

---

## 🛠️ Tech Stack & Architecture

- **Core Engine:** Written in **Rust** for near-zero runtime latency and memory safety.
- **UI Shell:** Built with **Tauri v2** + **React / TypeScript**.
- **Low-Level Hooks:**
  - **Windows:** Low-level `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hooks with warp-to-center delta tracking.
  - **macOS:** `CGEventTap` at HID level with absolute and relative delta injection.
- **Network & Serialization:** Length-prefixed `bincode` framing over TLS-encrypted TCP + UDP broadcast discovery.

---

## 🚀 Installation & Building

### Prerequisites

| Platform | Dependencies |
| :--- | :--- |
| **All Platforms** | Rust (stable 1.77+), Node.js (18+) |
| **Windows** | Visual C++ Build Tools, WebView2, WiX Toolset v3 (for `.msi` builds) |
| **macOS** | Xcode Command Line Tools (`xcode-select --install`) |

### Development Setup

```bash
# Clone repository
git clone [https://github.com/halvantic/shareflow.git](https://github.com/halvantic/shareflow.git)
cd shareflow

# Install frontend dependencies
npm install

# Launch Tauri dev environment
npm run tauri dev