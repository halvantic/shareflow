# ShareFlow — Code Review Issue Tracker

Generated: 2026-04-02  
Source: Multi-SME review (sme-macos · sme-windows · sme-rust-tauri · sme-security · sme-architecture)  
Status legend: `[ ]` Open · `[~]` In Progress · `[x]` Fixed

Fix order is sequenced so foundational changes come before dependent ones.

---

## Batch 1 — Quick wins (no cross-platform coordination needed)

### 1.1 · Re-enable Content Security Policy `[x]`
- **Severity:** High  
- **File:** `src-tauri/tauri.conf.json` line 24  
- **Issue:** `"csp": null` removes all XSS mitigation. Frontend XSS → unrestricted Tauri IPC → RCE.  
- **Fix:** Set `"csp": "default-src 'self'; script-src 'self'; object-src 'none'; base-uri 'self';"`. Test all frontend routes.

### 1.2 · macOS: move CFRelease out of unreachable code path `[x]`
- **Severity:** Critical  
- **File:** `src-tauri/src/input/macos.rs` lines 902–959 (`run_event_tap`)  
- **Issue:** `CFMachPortRef` and `CFRunLoopSourceRef` are released after `CFRunLoopRun()`, which never returns. Both are leaked on every capture session start.  
- **Fix:** Move `CFRelease` calls into `stop_capture()`, immediately after `CFRunLoopStop()`.

### 1.3 · macOS: remove panic-through-FFI in event tap callback `[x]`
- **Severity:** High  
- **File:** `src-tauri/src/input/macos.rs` line 812 and all uses of `unwrap_or_else(|e| e.into_inner())` inside `event_tap_callback`  
- **Issue:** Recovering a poisoned `Mutex` with `into_inner()` inside a C callback can unwind through the FFI boundary — undefined behaviour. Inconsistent modifier state causes stuck keys.  
- **Fix:** Replace every `unwrap_or_else(|e| e.into_inner())` inside the callback with `.try_lock()`; skip the update and log a warning if unavailable or poisoned.

### 1.4 · Windows: log SendInput failures `[x]`
- **Severity:** High  
- **File:** `src-tauri/src/input/windows.rs` lines 708, 729, 745, 781, 812  
- **Issue:** `SendInput()` return value is discarded; silent injection failures give no diagnostic signal.  
- **Fix:** Capture return value; `log::warn!` when fewer events were injected than requested.

### 1.5 · Windows: fix dangling mouse hook on keyboard hook install failure `[x]`
- **Severity:** High  
- **File:** `src-tauri/src/input/windows.rs` lines 129–145  
- **Issue:** If keyboard hook install fails, mouse hook is unregistered but `HOOK_ACTIVE` was already set `true`; the mouse hook leaks.  
- **Fix:** Set `HOOK_ACTIVE = true` only after both hooks succeed.

### 1.6 · Windows: add timeout to stop_capture join (prevent shutdown deadlock) `[ ]`
- **Severity:** High  
- **File:** `src-tauri/src/input/windows.rs` lines 224–226  
- **Issue:** `PostThreadMessageW` failure is silently swallowed; subsequent `join()` blocks forever if the thread is already gone.  
- **Fix:** Check `PostThreadMessageW` bool return; log on failure; use a cancel `AtomicBool` + short-timeout join rather than `join()` blocking indefinitely.

### 1.7 · Validate LZ4/message length before allocation `[ ]`
- **Severity:** High  
- **Files:** `src-tauri/src/network/connection.rs` line 91; `src-tauri/src/core/protocol.rs` line 174; `src-tauri/src/clipboard/sync.rs` lines 126–152  
- **Issue:** A `0xFFFFFFFF` length prefix triggers a multi-GB allocation before the 16 MB pending buffer cap is checked. Clipboard image width/height not validated before LZ4 decompress.  
- **Fix:** Reject length prefix > 64 MB immediately. Before decompressing clipboard images, validate `width × height × 4 ≤ 512 MB`.

### 1.8 · IPMI digest auth: random cnonce + validate algorithm `[ ]`
- **Severity:** Critical  
- **File:** `src-tauri/src/amt/ipmi.rs` lines 136–202 (line 177 for cnonce)  
- **Issue:** `cnonce` is hardcoded `"0a4f113b"`; `nc` is always `"00000001"`. Captured Authorization header is replayable indefinitely. `algorithm` field from server challenge is never parsed; always uses MD5.  
- **Fix:** Generate a 16-byte random `cnonce` per request. Increment `nc` per RFC 2617. Parse `algorithm`; support MD5 + SHA-256; reject unknown values.

---

## Batch 2 — Protocol foundation (coordinate both sides before dependent fixes)

### 2.1 · Add protocol version negotiation `[ ]`
- **Severity:** Medium  
- **Files:** `src-tauri/src/core/protocol.rs` lines 1–117; `src-tauri/src/network/server.rs` handshake  
- **Issue:** No version field in `Hello`. Old/new clients silently diverge; new message types cause one side to wait indefinitely.  
- **Fix:** Add `protocol_version: u16` to `Hello` and `HelloAck`. Both sides negotiate minimum. Reject connections below minimum supported version.  
- **Coordination:** Both peers must be updated in the same release.

---

## Batch 3 — Security: TLS trust + discovery (depends on Batch 2 for version field)

### 3.1 · Implement certificate fingerprint confirmation on first connect `[ ]`
- **Severity:** Critical  
- **Files:** `src-tauri/src/network/tls.rs` lines 104–107; `src-tauri/src/core/config.rs` lines 25–28  
- **Issue:** TOFU accepts any certificate without user confirmation. First-connect MITM is trivially possible. Auto-connect ignores certificate changes for known `peer_id`.  
- **Fix:** On unknown fingerprint, emit a UI event with the SHA-256 fingerprint and require explicit user approval before persisting. For known peers, compare live fingerprint to stored value; block and alert if changed.  
- **Coordination:** UI change required; both peers must run the same version.

### 3.2 · Sign discovery announcements with peer certificate `[ ]`
- **Severity:** High  
- **Files:** `src-tauri/src/network/discovery.rs` lines 6–90  
- **Issue:** Any LAN host can broadcast a crafted UDP packet to inject a fake peer. Timestamp window prevents replay but not continuous spoofing.  
- **Fix:** HMAC-sign announcement body with a key derived from the peer's TLS private key. Include the sender's cert fingerprint in the announcement. Verify on receive using stored fingerprint for known peers.  
- **Coordination:** Both peers must support signed announcements; use protocol version gate.

---

## Batch 4 — Security: authentication + code consolidation (depends on Batch 3)

### 4.1 · Deduplicate peer session message-handling code `[ ]`
- **Severity:** High  
- **Files:** `src-tauri/src/lib.rs` lines 78–268; `src-tauri/src/network/server.rs` lines 69–344  
- **Issue:** Inbound and outbound peer loops are near-identical. Security patches applied to one path do not automatically apply to the other.  
- **Fix:** Extract `async fn process_peer_messages(conn, engine, peer_id, ...)` used by both paths. All message validation and auth logic lives in one place.

### 4.2 · Implement application-layer pairing handshake `[ ]`
- **Severity:** High  
- **Files:** `src-tauri/src/core/protocol.rs` lines 23–26; `src-tauri/src/network/server.rs` handshake  
- **Issue:** `AuthChallenge`, `AuthResponse`, `AuthResult` are defined but never sent or handled. There is no challenge-response authentication after TLS.  
- **Fix:** After Hello/HelloAck, server sends `AuthChallenge(nonce: [u8; 32])`. Client responds with `AuthResponse(hmac: [u8; 32])` using a shared pairing code. Server sends `AuthResult`. Reject peer if auth fails. Do this in the shared handler from 4.1.  
- **Coordination:** Both peers must support the handshake.

---

## Batch 5 — Security: credentials + AMT hardening (independent, do with Batch 4)

### 5.1 · Encrypt AMT/IPMI credentials at rest `[ ]`
- **Severity:** Critical  
- **Files:** `src-tauri/src/core/config.rs` lines 30–45; `src-tauri/src/lib.rs` lines 555, 471–499  
- **Issue:** Username and password for every AMT computer are stored in plaintext in `config.json`. Any process with filesystem read access can steal credentials.  
- **Fix:** Encrypt credential fields using DPAPI on Windows and Keychain on macOS. Never write plaintext secrets to disk. Provide a migration path for existing configs.

### 5.2 · File transfer: add SHA-256 integrity verification `[ ]`
- **Severity:** High  
- **Files:** `src-tauri/src/file_transfer/receiver.rs`; `src-tauri/src/file_transfer/sender.rs`; `src-tauri/src/core/protocol.rs`  
- **Issue:** No checksum on received files. MITM can silently corrupt or replace file contents. Overlapping chunk writes can corrupt files.  
- **Fix:** Add `sha256: [u8; 32]` to `FileDone`. Sender computes hash over entire file. Receiver verifies before promoting temp file. Add per-chunk hash or enforce strict sequential ordering.  
- **Coordination:** Protocol change — both sides must support the hash field.

---

## Batch 6 — Input reliability + rate limiting

### 6.1 · Add per-peer input event rate limiting `[ ]`
- **Severity:** Critical  
- **Files:** `src-tauri/src/network/server.rs` lines 194–340  
- **Issue:** No cap on injected input events per peer. Malicious/compromised peer can flood the machine.  
- **Fix:** Token-bucket rate limiter per peer: 200 input events/sec, burst 50. Drop excess; log violations with peer ID.

### 6.2 · Fix message queue lo-priority starvation `[ ]`
- **Severity:** High  
- **Files:** `src-tauri/src/network/connection.rs` lines 38–83  
- **Issue:** Biased `select!` starves clipboard/file messages whenever mouse traffic is continuous.  
- **Fix:** After every 5 high-priority messages, yield to low-priority queue unconditionally. Add queue-depth diagnostic logging.

### 6.3 · macOS: fix KEYBOARD_PRIMED startup race condition `[ ]`
- **Severity:** Medium  
- **Files:** `src-tauri/src/input/macos.rs` lines 873–880, 1089–1107  
- **Issue:** Injector may attempt key injection before the event tap thread is fully running; first keystroke is silently dropped.  
- **Fix:** Add a `oneshot` channel between event tap initialisation and injector constructor. Block injector creation until event tap signals ready.

### 6.4 · macOS: fix modifier key tracking (Caps Lock mapping + synthetic release race) `[ ]`
- **Severity:** Medium  
- **Files:** `src-tauri/src/input/macos.rs` lines 583–595, 110–157, 1365–1416  
- **Issue:** Caps Lock is absent from `modifier_flags_for_vk`; synthetic modifier-up events race with physically held keys during focus transition.  
- **Fix:** Add Caps Lock to `modifier_flags_for_vk`. Read HID physical state before injecting synthetic releases; only release modifiers that are not currently physically held.

### 6.5 · macOS: fix scroll delta precision loss `[ ]`
- **Severity:** Medium  
- **Files:** `src-tauri/src/input/macos.rs` lines 1281–1307  
- **Issue:** Deltas 1–119 become 0 after `÷120` round-trip; trackpad precision is lost.  
- **Fix:** Accumulate fractional scroll deltas in a thread-local static; emit only when accumulated ≥ 1 line.

### 6.6 · macOS: log unmapped keycode instead of sending scancode 0 `[ ]`
- **Severity:** Medium  
- **File:** `src-tauri/src/input/macos.rs` line 454  
- **Issue:** Unknown VK → scancode 0 is silently forwarded; remote receives meaningless event.  
- **Fix:** Emit `log::warn!("Unmapped macOS VK 0x{:X} — dropping", vk)` and skip the send.

### 6.7 · Windows: fix remote bounds race condition `[ ]`
- **Severity:** Medium  
- **File:** `src-tauri/src/input/windows.rs` lines 381–414  
- **Issue:** Four separate atomics for the remote bounding rect can give an inconsistent rectangle if a concurrent update races with the hook read.  
- **Fix:** Replace the four atomics with a `Mutex<RemoteBounds>` struct or a single `AtomicU64` packed representation.

### 6.8 · Windows: fix SUPPRESS / ShowCursor ordering `[ ]`
- **Severity:** Medium  
- **File:** `src-tauri/src/input/windows.rs` lines 260–275  
- **Issue:** `SUPPRESS` is set before `ShowCursor` loop completes; hook can see the flag as true while cursor is still visible.  
- **Fix:** Set `SUPPRESS` only after the `ShowCursor` loop confirms the target state.

### 6.9 · Fix atomic ordering for primary_km and is_remote flags `[ ]`
- **Severity:** Medium  
- **Files:** `src-tauri/src/core/engine.rs` lines 121, 262, 298; `src-tauri/src/core/runtime.rs` lines 36, 113; `src-tauri/src/input/windows.rs`  
- **Issue:** `primary_km` uses `Relaxed` load/store; `is_remote` uses `Release/Acquire` but Windows hooks read `SUPPRESS` with `SeqCst` — inconsistent ordering hierarchy.  
- **Fix:** Use `Release` for all stores, `Acquire` for all loads on `is_remote`. Use `SeqCst` for `primary_km` (safety-critical gate). Document the ordering model.

### 6.10 · Fix focus state TOCTOU for non-primary K+M device `[ ]`
- **Severity:** High  
- **File:** `src-tauri/src/core/engine.rs` lines 114–121; `src-tauri/src/core/runtime.rs` line 113  
- **Issue:** `is_primary_km` is read atomically then the focus lock is acquired separately. Race between these two can leave a non-primary device stuck in Remote focus with suppression ON permanently.  
- **Fix:** Read `is_primary_km` inside the focus lock. Ensure `update_settings` forces a Local switch before returning when `is_primary_km` is set false.

---

## Batch 7 — Architecture + reliability

### 7.1 · Config deserialization: log and alert on silent reset `[ ]`
- **Severity:** Medium  
- **Files:** `src-tauri/src/core/config.rs` line 160; `src-tauri/src/lib.rs`  
- **Issue:** Failed JSON parse silently returns default config, erasing all trusted peers without user notification.  
- **Fix:** Log `log::error!` on parse failure; emit a UI event notifying the user their config was reset due to corruption.

### 7.2 · Windows private key file permissions `[ ]`
- **Severity:** Medium  
- **File:** `src-tauri/src/network/tls.rs` line 47  
- **Issue:** `#[cfg(unix)]` restricts `set_permissions(0o600)` to Unix only; Windows private key is saved with default ACL.  
- **Fix:** On Windows, apply a restrictive DACL using `icacls` or encrypt the key file with DPAPI.

### 7.3 · Add startup initialisation barrier `[ ]`
- **Severity:** Medium  
- **Files:** `src-tauri/src/lib.rs` lines 1205–1365  
- **Issue:** Input capture, peer server, clipboard sync, and agent auto-connect are spawned as fire-and-forget with no ordering. The 1500 ms magic delay is fragile.  
- **Fix:** Use a `Notify` or `oneshot` per service. Emit `AppState::Ready` only after all critical services have confirmed binding. Remove the hardcoded delay.

### 7.4 · Unsafe Objective-C FFI: add null guards `[ ]`
- **Severity:** Low  
- **File:** `src-tauri/src/clipboard/sync.rs` lines 32–49  
- **Issue:** Raw `objc_msgSend` via `transmute` without null-return validation; no error handling if NSPasteboard is unavailable.  
- **Fix:** Add null-pointer checks after each `objc_msgSend` call. Return `Result` from the function. Consider switching to the `objc2` crate.

### 7.5 · Commit Cargo.lock and add cargo audit `[ ]`
- **Severity:** Medium  
- **File:** `Cargo.lock` (missing from version control consideration); CI config  
- **Issue:** Floating semver ranges allow transient CVE exposure without notice.  
- **Fix:** Ensure `Cargo.lock` is committed. Add `cargo audit` and `cargo deny` to CI pipeline.

### 7.6 · Linux data directory path: follow XDG spec `[ ]`
- **Severity:** Low  
- **File:** `src-tauri/src/network/tls.rs` lines 170–174  
- **Issue:** Linux data dir resolves to `./shareflow` (current working directory) instead of `$HOME/.local/share/shareflow`.  
- **Fix:** Use `dirs::data_local_dir()` or manually construct `$HOME/.local/share/shareflow` on Linux, matching what `config.rs` already does.

### 7.7 · Discovery replay: add per-announcement nonce `[ ]`
- **Severity:** Low  
- **File:** `src-tauri/src/network/discovery.rs` lines 60–107  
- **Issue:** 30-second timestamp window prevents cold replay but not hot replay (attacker broadcasts fresh packets every 25 s).  
- **Fix:** Add a `nonce: [u8; 16]` field to `Announcement`. Track (peer_id, nonce) tuples seen in the last 60 s; reject exact duplicates.

### 7.8 · Restrict crash log file permissions `[ ]`
- **Severity:** Low  
- **File:** `src-tauri/src/lib.rs` lines 1071–1143  
- **Issue:** Crash log is written without restricting read permissions; may expose peer IDs and file paths to other local users.  
- **Fix:** Set permissions to `0o600` on Unix / restrictive DACL on Windows after writing the crash log.

### 7.9 · macOS: re-check Accessibility permission at runtime `[ ]`
- **Severity:** Low  
- **File:** `src-tauri/src/input/macos.rs` lines 873–880  
- **Issue:** `AXIsProcessTrusted()` is called once at init. If permission is revoked, input silently stops with no user feedback.  
- **Fix:** Check `AXIsProcessTrusted()` on CGEventTap creation failure; emit a UI event prompting the user to re-grant permission.

---

## Batch 8 — Design improvements (lower urgency)

### 8.1 · AMT: add status tracking, HTTPS, and audit logging `[ ]`
- **Severity:** Design  
- **Files:** `src-tauri/src/amt/ipmi.rs`; `src-tauri/src/lib.rs` lines 560–614  
- **Issue:** No online/offline status per AMT computer, hardcoded HTTP (credentials transmitted in clear on non-TLS AMT), no audit log of who triggered power commands.  
- **Fix:** Add `AmtComputerStatus` enum; implement periodic heartbeat task; support HTTPS with certificate option; emit `UiEvent::AmtStatusChanged`; log power commands with timestamp.

### 8.2 · Implement per-peer input event rate limiting `[ ]`
- (See 6.1 — listed again as design reference)

### 8.3 · Decompose lib.rs God module `[ ]`
- **Severity:** Design  
- **File:** `src-tauri/src/lib.rs` (~1509 lines)  
- **Issue:** lib.rs mixes Tauri command handlers, business logic, startup orchestration, peer management, and AMT integration. Difficult to test, navigate, and modify safely.  
- **Fix:** Split into `tauri_commands.rs`, `app_orchestrator.rs`, `peer_manager.rs`, `agent_mode.rs`. Keep only `run()` entry point in lib.rs.

### 8.4 · Implement Linux input capture and injection `[ ]`
- **Severity:** Design  
- **File:** `src-tauri/src/input/linux.rs` (all stubs)  
- **Issue:** All functions return `Err("not yet implemented")`. Screen resolution hardcoded 1920×1080.  
- **Fix:** Implement evdev capture + uinput injection following the same trait pattern as `input/windows.rs`. Add `get_screens_linux()` via Xlib or Wayland. Or surface a clear "Linux not yet supported" message in the UI.

### 8.5 · Make config struct self-enforcing (invalid state prevention) `[ ]`
- **Severity:** Design  
- **File:** `src-tauri/src/core/config.rs` lines 54–115  
- **Issue:** `{ agent_mode: true, is_primary_km_device: true }` is representable and silently corrected at runtime. No schema validation on load.  
- **Fix:** Introduce `SetupState` enum; add a `validate()` method called on load that returns `Err` for invalid combinations.

### 8.6 · Implement hotkey configuration and wire up detector `[ ]`
- **Severity:** Design  
- **File:** `src-tauri/src/core/hotkey.rs`; `src-tauri/src/core/runtime.rs`  
- **Issue:** `HotkeyDetector` is instantiated and configured but never consulted in the input loop. `set_combo` is never called at runtime. The feature is dead code.  
- **Fix:** Pass `HotkeyDetector` into the runtime input loop; call `process()` on every event; trigger configured action on match. Expose hotkey config in UI settings. Load from `AppConfig`.

### 8.7 · File transfer: add resume support `[ ]`
- **Severity:** Design  
- **Files:** `src-tauri/src/file_transfer/receiver.rs`; `src-tauri/src/file_transfer/sender.rs`  
- **Issue:** Interrupted transfers cannot be resumed; the partial file is deleted and the full transfer must restart.  
- **Fix:** Persist transfer metadata (transfer_id, file_name, last_offset) in a `transfers.json`. On new `FileStart` matching an existing partial file, offer to resume. Sender: retry failed chunks with exponential backoff.

---

## Summary stats

| Severity | Count | Batches |
|---|---|---|
| Critical | 5 | 1.2, 1.8, 3.1, 5.1, 6.1 |
| High | 11 | 1.1, 1.3–1.6, 3.2, 4.1–4.2, 5.2, 6.2, 6.10 |
| Medium | 12 | 1.7, 2.1, 6.3–6.9, 7.1–7.3 |
| Low | 7 | 7.4–7.9, 8.x |
| Design | 7 | 8.1–8.7 |
| **Total** | **42** | |
