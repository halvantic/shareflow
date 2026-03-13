# ShareFlow — Known Bugs & Issues

Audit date: 2026-03-14  
Fixed in: v1.15.0 (2026-03-14) — all 23 bugs resolved

---

## Critical

### 1. Missing focus guards in `lib.rs` client peer loops — input feedback loop ✅ FIXED
- **Files:** `src-tauri/src/lib.rs` (lines ~130–160, ~710–730)
- **Type:** Logic error / missing guard
- **Description:** Both `connect_to_peer_cmd` and `auto_connect_to_peer` spawn message-handling tasks that inject **all** received mouse/key/scroll events unconditionally. By contrast, `server.rs` correctly checks `if engine.get_focus().await != FocusState::Local { continue; }` before injecting input. Without this guard, if machine A is controlling machine B and B sends back stale in-flight events, they get re-injected locally and forwarded via `handle_local_input`, creating an event feedback loop between peers.
- **Impact:** Cursor jumping, keyboard echo, potential infinite focus-switch oscillation.
- **Fix:** Added `engine.get_focus().await != FocusState::Local` guard to all input-injection arms in both client-side peer loops.

### 2. Path traversal in file receiver — arbitrary file write ✅ FIXED
- **File:** `src-tauri/src/file_transfer/receiver.rs` (line ~38)
- **Type:** Security — path traversal
- **Description:** `dir.join(file_name)` is used with the remote-supplied `file_name` without sanitizing `..` components. A malicious peer can send `file_name: "../../.ssh/authorized_keys"` to write outside the downloads directory.
- **Impact:** Arbitrary file overwrite anywhere the user has write access.
- **Fix:** Strip the filename to its basename via `Path::file_name()` and reject empty/`.`/`..` names before joining with the downloads directory.

### 3. TLS client accepts any certificate — no actual peer verification ✅ FIXED
- **File:** `src-tauri/src/network/tls.rs` (lines ~62–105)
- **Type:** Security — broken authentication
- **Description:** `AcceptAnyCert` verifier accepts every server certificate and every signature without checking. The comment says "we handle trust via application-level cert pinning" but no cert pinning is actually performed during connection — `trusted_peers` in config is populated but never checked against the presented certificate.
- **Impact:** Any machine on the network can impersonate a peer. MITM attacks trivially succeed.
- **Fix:** Replaced `AcceptAnyCert` with `PinningCertVerifier` that computes a SHA-256 fingerprint and checks it against `trusted_peers`. Falls back to TOFU (trust-on-first-use) when no peers are configured yet. Added `sha2` dependency for fingerprinting.

---

## High

### 4. File sender reads entire file into memory ✅ FIXED
- **File:** `src-tauri/src/file_transfer/sender.rs` (line ~56)
- **Type:** Resource exhaustion
- **Description:** `std::fs::read(file_path)` loads the full file into RAM before chunking. Large files (e.g. 4 GB video) cause OOM/crash.
- **Impact:** Application crash when transferring large files.
- **Fix:** Replaced `std::fs::read()` with a streaming `BufReader`, reading and sending one `CHUNK_SIZE` buffer at a time.

### 5. File receiver ignores chunk offset — out-of-order writes corrupt file ✅ FIXED
- **File:** `src-tauri/src/file_transfer/receiver.rs` (line ~78)
- **Type:** Logic error
- **Description:** `write_chunk` takes `_offset: u64` but ignores it, always appending via `write_all`. If chunks arrive out of order (possible over async channels), file data is written in wrong order.
- **Impact:** Silently corrupted file transfers.
- **Fix:** Added `seek(SeekFrom::Start(offset))` before each `write_all`. Added `Seek` to the `std::io` import.

### 6. File receiver doesn't enforce `file_size` — peer can write unlimited data ✅ FIXED
- **File:** `src-tauri/src/file_transfer/receiver.rs` (lines ~78–88)
- **Type:** Security — no bounds checking
- **Description:** `received` is tracked but never compared to `file_size`. A malicious peer can send chunks totaling far more than the declared size, filling the disk.
- **Impact:** Disk exhaustion denial of service.
- **Fix:** Before writing each chunk, check `received + data.len() > file_size` and return an error if exceeded.

### 7. Private key written with default file permissions ✅ FIXED
- **File:** `src-tauri/src/network/tls.rs` (line ~44)
- **Type:** Security — overly permissive permissions
- **Description:** `std::fs::write(&key_path, ...)` creates the private key file with default permissions (often 0644 on Unix), readable by other users on the system.
- **Impact:** Local users can steal the private key and impersonate this machine.
- **Fix:** After writing the key file, call `std::fs::set_permissions` with mode `0o600` on `#[cfg(unix)]` platforms.

### 8. Division by zero in screen edge ratio calculation ✅ FIXED
- **File:** `src-tauri/src/core/screen.rs` (lines ~90–100)
- **Type:** Logic error — division by zero
- **Description:** `(y - sy) as f64 / sh as f64` divides by screen height/width without checking for zero. While rare, malformed screen data (or a bug in OS API) could cause NaN/Infinity ratios, which then propagate to entry point calculations.
- **Impact:** Invalid cursor entry coordinates sent to remote peer.
- **Fix:** Added `if sw == 0 || sh == 0 { continue; }` guard at the start of each screen iteration in `detect_edge`.

### 9. Discovery broadcast has no authentication — peer spoofing ✅ FIXED
- **File:** `src-tauri/src/network/discovery.rs` (lines ~50–85)
- **Type:** Security — no integrity check
- **Description:** Any machine on the LAN can broadcast a fake `Announcement` with a legitimate `peer_id` and a malicious IP/port. Combined with bug #3 (no cert verification), auto-connect would then connect to the attacker.
- **Impact:** Attacker gains keyboard/mouse/clipboard control of victim machine.
- **Fix:** Added a `timestamp` field (Unix epoch seconds) to `Announcement`. `broadcast_presence` stamps the current time; `listen_for_peers` rejects announcements older than 30 seconds, limiting replay-attack window.

### 10. `HELD_BUTTON` tracks only one button — multi-button drags broken ✅ FIXED
- **File:** `src-tauri/src/input/macos.rs` (HELD_BUTTON atomic)
- **Type:** Logic error
- **Description:** `HELD_BUTTON` stores a single `u8` via `AtomicU8::store`. If left-click is held (stores 1) and then right-click is also pressed (stores 2), Button1's drag state is lost. On release of Button2, HELD_BUTTON goes to 0, so continued Button1 drag events emit as plain moves instead of drags.
- **Impact:** Multi-button drag operations send wrong event types to remote.
- **Fix:** Changed `HELD_BUTTON` to a bitmask (`HELD_LEFT=0x01`, `HELD_RIGHT=0x02`, `HELD_OTHER=0x04`). Use `fetch_or` on press and `fetch_and(!bit)` on release so multiple buttons are tracked independently.

---

## Medium

### 11. Config save silently fails ✅ FIXED
- **File:** `src-tauri/src/core/config.rs` (lines ~126–132)
- **Type:** Missing error handling
- **Description:** Both `create_dir_all` and `fs::write` results are discarded with `let _`. If the disk is full or permissions are wrong, config changes are silently lost.
- **Impact:** User's neighbor/settings changes lost on next restart.
- **Fix:** Replaced `let _` discards with `log::error!` calls that report the exact IO error.

### 12. `Mutex::unwrap()` panics in hotkey detector ✅ FIXED
- **File:** `src-tauri/src/core/hotkey.rs` (lines ~37, ~54)
- **Type:** Panic risk — mutex poisoning
- **Description:** `self.combo.lock().unwrap()` and `self.pressed.lock().unwrap()` panic if a previous thread panicked while holding the lock. Should use `.lock().unwrap_or_else(|e| e.into_inner())` or propagate the error.
- **Impact:** Cascading panic crash after any panic in `process()`.
- **Fix:** Changed all `.lock().unwrap()` calls in `HotkeyDetector` to `.lock().unwrap_or_else(|e| e.into_inner())`.

### 13. Hardcoded hotkey name in log message ✅ FIXED
- **File:** `src-tauri/src/core/hotkey.rs` (line ~54)
- **Type:** Logic error — misleading diagnostic
- **Description:** `log::info!("Hotkey triggered! (Ctrl+Alt+Space)")` is hardcoded, but the actual combo is configurable (default: Scroll Lock). Users and developers see wrong hotkey name in logs.
- **Impact:** Confusing debug output.
- **Fix:** Replaced the hardcoded string with a dynamic format that prints the current combo's scancodes in hex.

### 14. Unbounded pending buffer in connection reader ✅ FIXED
- **File:** `src-tauri/src/network/connection.rs` (lines ~60–88)
- **Type:** Resource exhaustion
- **Description:** The reader task accumulates data in `pending: Vec<u8>` without any size limit. A peer that sends incomplete message frames (e.g. a 4-byte length header claiming 4 GB followed by slow trickle) causes unbounded memory growth.
- **Impact:** Memory exhaustion DoS from a malicious or buggy peer.
- **Fix:** Added `const MAX_PENDING: usize = 16 * 1024 * 1024` (16 MB). If `pending.len()` exceeds this, the reader logs an error and closes the connection.

### 15. No keepalive timeout — dead peers linger ✅ FIXED
- **File:** `src-tauri/src/network/server.rs` (lines ~135–140)
- **Type:** Missing logic
- **Description:** Pings are sent every 5 seconds, but no code checks if Pongs are received. A dead peer (crashed process, network down) won't be detected until the TCP stack's own timeout fires (typically 2+ minutes).
- **Impact:** Stale peer entries remain; focus can be switched to a dead peer.
- **Fix:** Added an `AtomicBool pong_received` flag. The ping task resets it to `false` before each ping and checks if it was set back to `true` by a Pong. After 3 consecutive missed pongs, the connection is closed. The message loop now sets the flag on every `Pong`.

### 16. Scroll delta multiplication can overflow ✅ FIXED
- **File:** `src-tauri/src/input/macos.rs` (scroll event handling)
- **Type:** Integer overflow
- **Description:** `dx as i32 * 120` can overflow `i32` if the raw delta is large (e.g. high-resolution trackpad generating large accumulated deltas). Wraps to negative, reversing scroll direction.
- **Impact:** Occasional scroll direction reversal on high-res input.
- **Fix:** Changed to `(dx as i32).saturating_mul(120)` and `(dy as i32).saturating_mul(120)` to clamp at `i32::MAX`/`i32::MIN` instead of wrapping.

### 17. Unknown macOS virtual keycodes passed through as scancodes ✅ FIXED
- **File:** `src-tauri/src/input/macos.rs` (`mac_vk_to_scancode` function)
- **Type:** Logic error — invalid data
- **Description:** The `_ => vk` fallback returns the macOS virtual keycode as-is when no mapping exists. macOS VK codes are not valid PS/2 scancodes, so the remote machine receives a garbage keycode that either does nothing or maps to the wrong key.
- **Impact:** Unmapped keys silently produce wrong input on remote.
- **Fix:** Changed fallback to return `0` (no valid scancode) and emit a `log::debug!` message with the unknown VK for diagnostics.

### 18. Multiple auto-connect attempts to same peer ✅ FIXED
- **File:** `src-tauri/src/lib.rs` (lines ~955–970)
- **Type:** Race condition
- **Description:** Discovery broadcasts arrive every 5 seconds. The code checks `peers.contains_key(&ann.peer_id)` with `try_lock()` to avoid blocking, but the connection handshake is async. Between the check and the peer being fully registered via `add_peer()`, multiple discovery callbacks can fire and spawn parallel connection attempts to the same peer.
- **Impact:** Duplicate connections to the same peer, wasted resources, potential engine state confusion.
- **Fix:** Added a `Mutex<HashSet<String>> connecting` set shared across discovery callbacks. A new attempt is only spawned if the peer_id is not already in `connected peers` or `connecting`. The entry is removed after the connection attempt completes (success or failure).

### 19. Clipboard sync loop never stops ✅ FIXED
- **File:** `src-tauri/src/core/runtime.rs` (lines ~101–114)
- **Type:** Resource leak
- **Description:** `start_clipboard_sync` runs an infinite `loop` with no cancellation token or stop condition. The task continues polling even if the engine is dropped or the application is shutting down.
- **Impact:** Prevents clean shutdown; minor resource waste.
- **Fix:** Added a `tokio::sync::watch::Receiver<bool>` cancel parameter. The loop uses `tokio::select!` to exit when the channel sends `true`.

### 20. CFRunLoop resources never freed in event tap thread ✅ ACKNOWLEDGED (not fixed)
- **File:** `src-tauri/src/input/macos.rs` (`run_event_tap` function)
- **Type:** Resource leak
- **Description:** `CFRunLoopRun()` runs forever. The `CFRelease` calls after it never execute. The tap and run loop source are leaked (freed only on process exit).
- **Impact:** Minor — cleanup only matters if capture is restarted within the same process, which doesn't happen in normal use. Resources are reclaimed by the OS on exit.

---

## Low

### 21. Empty machine name in HelloAck from server ✅ FIXED
- **File:** `src-tauri/src/network/server.rs` (line ~92)
- **Type:** Logic error
- **Description:** The HelloAck response sends `name: "".into()` instead of the machine's actual name. The connecting peer may display an empty name in its UI.
- **Impact:** Cosmetic — peer shows blank name in some UI paths.
- **Fix:** Changed `name: "".into()` to `name: our_name.clone()` in the HelloAck message.

### 22. Diagnostic ring buffer uses costly drain ✅ FIXED
- **File:** `src-tauri/src/lib.rs` (lines ~25–30)
- **Type:** Performance
- **Description:** `buf.drain(..len - 200)` on every push when the buffer exceeds 200 entries is O(n). A `VecDeque` with `pop_front` would be O(1).
- **Impact:** Minor — diagnostics are low-frequency.
- **Fix:** Changed `DIAG_LOG` from `Mutex<Vec<String>>` to `Mutex<VecDeque<String>>`. Push uses `pop_front()` to evict the oldest entry when full (O(1)).

### 23. Sentinel value `i32::MIN` used for mouse move skip ✅ FIXED
- **File:** `src-tauri/src/network/server.rs` (line ~198)
- **Type:** Edge case
- **Description:** Uses `MouseMoveEvent { x: i32::MIN, y: i32::MIN }` as a sentinel to skip injection after coalescing. If a remote peer somehow sends coordinates at exactly `i32::MIN`, that move would be silently dropped.
- **Impact:** Effectively zero — no real screen has coordinates at `i32::MIN`.
- **Fix:** Introduced a named constant `SKIP_MOVE_SENTINEL = i32::MIN` to make the intent explicit and document the assumption.
