import { useState, useEffect, useRef, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import "./App.css";

interface ScreenInfo {
  id: string;
  x: number;
  y: number;
  width: number;
  height: number;
  primary: boolean;
}

interface PeerInfo {
  id: string;
  name: string;
  screens: number;
}

interface AppConfig {
  machine_name: string;
  peer_id: string;
  port: number;
  discovery_port: number;
  auto_connect: boolean;
  camera_sharing_enabled: boolean;
  audio_sharing_enabled: boolean;
  is_primary_km_device: boolean;
  clipboard_sync_enabled: boolean;
  agent_mode: boolean;
  host_address: string;
  is_first_run: boolean;
  trusted_hosts: { peer_id: string; name: string }[];
  neighbors: { peer_id: string; edge: string; screen_id?: string }[];
  trusted_peers: any[];
}

type FocusState = "Local" | { Remote: string };

interface FileTransfer {
  transfer_id: string;
  file_name: string;
  total_bytes: number;
  transferred_bytes: number;
  done: boolean;
  direction: string;
}

interface ReceivedFile {
  file_name: string;
  path: string;
  size: number;
}

interface DiscoveredPeer {
  id: string;
  name: string;
  address: string;
  lastSeen: number;
}

interface Toast {
  id: number;
  text: string;
  level: "info" | "success" | "error";
}

let toastCounter = 0;

function App() {
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [screens, setScreens] = useState<ScreenInfo[]>([]);
  const [peers, setPeers] = useState<PeerInfo[]>([]);
  const [localIp, setLocalIp] = useState("");
  const [connectAddr, setConnectAddr] = useState("");
  const [connectStatus, setConnectStatus] = useState("");
  const [focus, setFocus] = useState<FocusState>("Local");
  const [logs, setLogs] = useState<{ text: string; level: string }[]>([]);
  const [fileTransfers, setFileTransfers] = useState<Map<string, FileTransfer>>(
    new Map()
  );
  const [receivedFiles, setReceivedFiles] = useState<ReceivedFile[]>([]);
  const [discoveredPeers, setDiscoveredPeers] = useState<
    Map<string, DiscoveredPeer>
  >(new Map());
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [appVersion, setAppVersion] = useState("");
  const [showDiag, setShowDiag] = useState(false);
  const [diagLines, setDiagLines] = useState<string[]>([]);
  const [showSettings, setShowSettings] = useState(false);
  const [showPermissionsModal, setShowPermissionsModal] = useState(false);
  const [settingsPort, setSettingsPort] = useState("");
  const [settingsDiscoveryPort, setSettingsDiscoveryPort] = useState("");
  const [settingsAutoConnect, setSettingsAutoConnect] = useState(false);
  const [settingsMachineName, setSettingsMachineName] = useState("");
  const [settingsCameraEnabled, setSettingsCameraEnabled] = useState(false);
  const [settingsAudioEnabled, setSettingsAudioEnabled] = useState(false);
  const [settingsIsPrimaryKm, setSettingsIsPrimaryKm] = useState(true);
  const [settingsClipboardEnabled, setSettingsClipboardEnabled] = useState(true);
  // Setup wizard state
  const [isFirstRun, setIsFirstRun] = useState(false);
  const [wizardMode, setWizardMode] = useState<"host" | "agent">("host");
  const [wizardHostAddr, setWizardHostAddr] = useState("");
  // Camera KVM state
  const [cameraActive, setCameraActive] = useState(false);
  const [remoteCameras, setRemoteCameras] = useState<Map<string, string>>(new Map());
  // Audio KVM state
  const [audioActive, setAudioActive] = useState(false);
  const logRef = useRef<HTMLDivElement>(null);
  const diagRef = useRef<HTMLDivElement>(null);
  const videoRef = useRef<HTMLVideoElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const cameraIntervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const cameraStreamRef = useRef<MediaStream | null>(null);
  const audioRecorderRef = useRef<MediaRecorder | null>(null);
  const audioStreamRef = useRef<MediaStream | null>(null);
  const audioCtxRef = useRef<AudioContext | null>(null);
  const nextAudioTimeRef = useRef<number>(0);

  const addToast = useCallback(
    (text: string, level: "info" | "success" | "error" = "info") => {
      const id = ++toastCounter;
      setToasts((prev) => [...prev.slice(-4), { id, text, level }]);
      setTimeout(() => {
        setToasts((prev) => prev.filter((t) => t.id !== id));
      }, 4000);
    },
    []
  );

  const addLog = useCallback(
    (text: string, level: string = "info") => {
      const time = new Date().toLocaleTimeString();
      setLogs((prev) => [
        ...prev.slice(-200),
        { text: `[${time}] ${text}`, level },
      ]);
    },
    []
  );

  // Listen for macOS permissions-required event and show the setup modal.
  useEffect(() => {
    const unsub = listen<any>("permissions-required", () => {
      setShowPermissionsModal(true);
    });
    return () => { unsub.then((f) => f()); };
  }, []);

  const handleOpenAccessibilitySettings = async () => {
    await invoke("open_accessibility_settings");
  };

  const handleCheckAccessibilityAgain = async () => {
    const granted = await invoke<boolean>("check_accessibility_permission");
    if (granted) {
      setShowPermissionsModal(false);
      addToast("Accessibility granted — restart app to activate input capture", "success");
      addLog("Accessibility permission granted", "success");
    } else {
      addToast("Not yet granted — enable ShareFlow in System Settings", "info");
    }
  };

  useEffect(() => {
    if (logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight;
    }
  }, [logs]);

  // Poll diagnostics from Rust backend when panel is open
  useEffect(() => {
    if (!showDiag) return;
    let active = true;
    const poll = async () => {
      while (active) {
        try {
          const lines = await invoke<string[]>("get_diagnostics");
          setDiagLines(lines);
          if (diagRef.current) {
            diagRef.current.scrollTop = diagRef.current.scrollHeight;
          }
        } catch {}
        await new Promise((r) => setTimeout(r, 500));
      }
    };
    poll();
    return () => { active = false; };
  }, [showDiag]);

  useEffect(() => {
    getVersion().then(setAppVersion);
    invoke<any>("get_config").then((cfg) => {
      setConfig(cfg);
      setSettingsPort(String(cfg.port));
      setSettingsDiscoveryPort(String(cfg.discovery_port || 24801));
      setSettingsAutoConnect(cfg.auto_connect || false);
      setSettingsMachineName(cfg.machine_name || "");
      setSettingsCameraEnabled(cfg.camera_sharing_enabled || false);
      setSettingsAudioEnabled(cfg.audio_sharing_enabled || false);
      setSettingsIsPrimaryKm(cfg.is_primary_km_device !== false);
      setSettingsClipboardEnabled(cfg.clipboard_sync_enabled !== false);
      if (cfg.is_first_run) {
        setIsFirstRun(true);
      }
      addLog(
        `Machine: ${cfg.machine_name} (${cfg.peer_id.slice(0, 8)}...)`,
        "info"
      );
    });

    invoke<ScreenInfo[]>("get_screens_info").then((s) => {
      setScreens(s);
      addLog(`Detected ${s.length} display(s)`, "info");
    });

    invoke<string>("get_local_ip")
      .then((ip) => {
        setLocalIp(ip);
        addLog(`Local IP: ${ip}`, "info");
      })
      .catch(() => setLocalIp("unknown"));

    addLog("Move mouse to a configured screen edge to switch focus between PCs", "info");

    // Expire stale discovered peers every 10s
    const cleanupInterval = setInterval(() => {
      setDiscoveredPeers((prev) => {
        const now = Date.now();
        const next = new Map(prev);
        let changed = false;
        for (const [id, peer] of next) {
          if (now - peer.lastSeen > 15000) {
            next.delete(id);
            changed = true;
          }
        }
        return changed ? next : prev;
      });
    }, 10000);

    const unlisten = listen<any>("shareflow-event", (event) => {
      const data = event.payload;
      switch (data.type) {
        case "FocusChanged":
          setFocus(data.state);
          if (data.state === "Local") {
            addLog("Focus returned to local", "success");
          } else {
            addLog(
              `Focus switched to remote: ${data.state.Remote.slice(0, 8)}...`,
              "info"
            );
          }
          break;
        case "PeerConnected":
          addToast(`${data.name} connected`, "success");
          addLog(
            `Peer connected: ${data.name} (${data.id.slice(0, 8)}...)`,
            "success"
          );
          setPeers((prev) => [
            ...prev.filter((p) => p.id !== data.id),
            { id: data.id, name: data.name, screens: data.screens },
          ]);
          // Remove from discovered list once connected
          setDiscoveredPeers((prev) => {
            const next = new Map(prev);
            next.delete(data.id);
            return next;
          });
          break;
        case "PeerDisconnected":
          addToast(`Peer disconnected`, "error");
          addLog(`Peer disconnected: ${data.id.slice(0, 8)}...`, "error");
          setPeers((prev) => prev.filter((p) => p.id !== data.id));
          // Remove any camera feed from this peer
          setRemoteCameras((prev) => {
            const next = new Map(prev);
            next.delete(data.id);
            return next;
          });
          break;
        case "Log":
          addLog(data.message, data.level);
          break;
        case "FileProgress":
          setFileTransfers((prev) => {
            const next = new Map(prev);
            if (data.done) {
              next.delete(data.transfer_id);
            } else {
              next.set(data.transfer_id, data as FileTransfer);
            }
            return next;
          });
          break;
        case "FileReceived":
          setReceivedFiles((prev) => [
            ...prev.slice(-50),
            {
              file_name: data.file_name,
              path: data.path,
              size: data.size,
            },
          ]);
          addToast(`Received: ${data.file_name}`, "success");
          addLog(
            `File received: ${data.file_name} (${formatBytes(data.size)})`,
            "success"
          );
          break;
        case "PeerDiscovered":
          setDiscoveredPeers((prev) => {
            const next = new Map(prev);
            next.set(data.id, {
              id: data.id,
              name: data.name,
              address: data.address,
              lastSeen: Date.now(),
            });
            return next;
          });
          break;
        case "CameraFrame":
          setRemoteCameras((prev) => {
            const next = new Map(prev);
            next.set(data.peer_id, `data:image/jpeg;base64,${data.data_b64}`);
            return next;
          });
          break;
        case "AudioChunk":
          // Decode and schedule audio playback via Web Audio API
          (async () => {
            try {
              if (!audioCtxRef.current) {
                audioCtxRef.current = new AudioContext();
              }
              const ctx = audioCtxRef.current;
              if (ctx.state === "suspended") await ctx.resume();
              const raw = atob(data.data_b64);
              const bytes = new Uint8Array(raw.length);
              for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
              const audioBuffer = await ctx.decodeAudioData(bytes.buffer);
              const source = ctx.createBufferSource();
              source.buffer = audioBuffer;
              source.connect(ctx.destination);
              // Schedule to avoid gaps/overlaps between chunks
              const now = ctx.currentTime;
              if (nextAudioTimeRef.current < now + 0.05) {
                nextAudioTimeRef.current = now + 0.05;
              }
              source.start(nextAudioTimeRef.current);
              nextAudioTimeRef.current += audioBuffer.duration;
            } catch {
              // Silently ignore decode errors (partial chunks, unsupported codec)
            }
          })();
          break;
      }
    });

    return () => {
      clearInterval(cleanupInterval);
      unlisten.then((f) => f());
      // Stop camera if active
      if (cameraIntervalRef.current) clearInterval(cameraIntervalRef.current);
      if (cameraStreamRef.current) cameraStreamRef.current.getTracks().forEach((t) => t.stop());
      // Stop audio if active
      if (audioRecorderRef.current && audioRecorderRef.current.state !== "inactive") audioRecorderRef.current.stop();
      if (audioStreamRef.current) audioStreamRef.current.getTracks().forEach((t) => t.stop());
      if (audioCtxRef.current) audioCtxRef.current.close();
    };
  }, [addLog, addToast]);

  const handleConnect = async (address?: string) => {
    const addr = address || connectAddr;
    if (!addr) return;
    setConnectStatus("Connecting...");
    addLog(`Connecting to ${addr}...`);
    try {
      const result = await invoke<string>("connect_to_peer_cmd", {
        address: addr,
      });
      setConnectStatus(result);
      addLog(result, "success");
      setConnectAddr("");
    } catch (e: any) {
      const err = e.toString();
      setConnectStatus(`Error: ${err}`);
      addLog(`Connection failed: ${err}`, "error");
      addToast(`Connection failed`, "error");
    }
  };

  const handleSwitchTo = async (peerId: string) => {
    try {
      await invoke("switch_focus_to", { peerId });
    } catch (e: any) {
      addLog(`Switch failed: ${e}`, "error");
    }
  };

  const handleSwitchLocal = async () => {
    try {
      await invoke("switch_focus_local");
    } catch (e: any) {
      addLog(`Switch failed: ${e}`, "error");
    }
  };

  const handleSetNeighbor = async (
    peerId: string,
    edge: string,
    screenId?: string
  ) => {
    try {
      await invoke("set_neighbor", {
        peerId,
        edge,
        screenId: screenId || null,
      });
      addLog(`Set ${edge} neighbor${screenId ? ` (${screenId})` : ""}`, "success");
      const cfg = await invoke<any>("get_config");
      setConfig(cfg);
    } catch (e: any) {
      addLog(`Set neighbor failed: ${e}`, "error");
    }
  };

  const handleSendFile = async (peerId: string) => {
    try {
      const filePath = await open({
        multiple: false,
        title: "Select file to send",
      });
      if (!filePath) return;
      addLog(`Sending file to ${peerId.slice(0, 8)}...`);
      const result = await invoke<string>("send_file_to_peer", {
        peerId,
        filePath: filePath as string,
      });
      addLog(`File transfer started: ${result.slice(0, 8)}...`, "success");
    } catch (e: any) {
      addLog(`Send file failed: ${e}`, "error");
      addToast(`Send file failed`, "error");
    }
  };

  const handleSaveSettings = async () => {
    const port = parseInt(settingsPort, 10);
    const discoveryPort = parseInt(settingsDiscoveryPort, 10);
    if (!port || port < 1 || port > 65535) {
      addToast("Invalid port number", "error");
      return;
    }
    if (!discoveryPort || discoveryPort < 1 || discoveryPort > 65535) {
      addToast("Invalid discovery port", "error");
      return;
    }
    try {
      await invoke("update_settings", {
        port,
        discoveryPort,
        autoConnect: settingsAutoConnect,
        machineName: settingsMachineName,
        cameraSharingEnabled: settingsCameraEnabled,
        audioSharingEnabled: settingsAudioEnabled,
        isPrimaryKmDevice: settingsIsPrimaryKm,
        clipboardSyncEnabled: settingsClipboardEnabled,
      });
      const cfg = await invoke<any>("get_config");
      setConfig(cfg);
      addLog("Settings saved (restart app for port changes to take effect)", "success");
      addToast("Settings saved", "success");
    } catch (e: any) {
      addLog(`Save settings failed: ${e}`, "error");
      addToast("Failed to save settings", "error");
    }
  };

  const handleAddTrustedHost = async (peerId: string, name: string) => {
    try {
      await invoke("add_trusted_host", { peerId, name });
      const cfg = await invoke<any>("get_config");
      setConfig(cfg);
      addLog(`Added ${name} to trusted hosts`, "success");
      addToast(`${name} trusted`, "success");
    } catch (e: any) {
      addLog(`Failed to add trusted host: ${e}`, "error");
    }
  };

  const handleRemoveTrustedHost = async (peerId: string) => {
    try {
      await invoke("remove_trusted_host", { peerId });
      const cfg = await invoke<any>("get_config");
      setConfig(cfg);
      addLog("Removed from trusted hosts", "success");
    } catch (e: any) {
      addLog(`Failed to remove trusted host: ${e}`, "error");
    }
  };

  const handleCompleteSetup = async () => {
    if (wizardMode === "agent" && !wizardHostAddr.trim()) {
      addToast("Please enter the host address", "error");
      return;
    }
    try {
      await invoke("complete_setup", {
        agentMode: wizardMode === "agent",
        hostAddress: wizardMode === "agent" ? wizardHostAddr.trim() : "",
      });
      const cfg = await invoke<any>("get_config");
      setConfig(cfg);
      setIsFirstRun(false);
      if (wizardMode === "agent" && wizardHostAddr.trim()) {
        addLog(`Agent mode: connecting to ${wizardHostAddr.trim()}...`);
        handleConnect(wizardHostAddr.trim());
      }
    } catch (e: any) {
      addToast(`Setup failed: ${e}`, "error");
    }
  };

  const handleStartCamera = async () => {
    try {
      const stream = await navigator.mediaDevices.getUserMedia({
        video: { width: { ideal: 640 }, height: { ideal: 480 }, frameRate: { ideal: 10 } },
      });
      cameraStreamRef.current = stream;
      if (videoRef.current) {
        videoRef.current.srcObject = stream;
        await videoRef.current.play();
      }
      setCameraActive(true);
      addLog("Camera sharing started", "success");
      addToast("Camera sharing on", "success");

      // Capture and broadcast frames at ~10fps
      cameraIntervalRef.current = setInterval(async () => {
        const video = videoRef.current;
        const canvas = canvasRef.current;
        if (!video || !canvas || video.readyState < 2) return;
        const ctx = canvas.getContext("2d");
        if (!ctx) return;
        canvas.width = video.videoWidth || 640;
        canvas.height = video.videoHeight || 480;
        ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
        canvas.toBlob(async (blob) => {
          if (!blob) return;
          const reader = new FileReader();
          reader.onload = async () => {
            const result = reader.result as string;
            const b64 = result.split(",")[1];
            if (b64) {
              try {
                await invoke("send_camera_frame", { dataB64: b64 });
              } catch {
                // Silently ignore send errors (e.g. no peers connected)
              }
            }
          };
          reader.readAsDataURL(blob);
        }, "image/jpeg", 0.5);
      }, 100);
    } catch (e: any) {
      addLog(`Camera error: ${e}`, "error");
      addToast("Camera access denied", "error");
    }
  };

  const handleStopCamera = () => {
    if (cameraIntervalRef.current) {
      clearInterval(cameraIntervalRef.current);
      cameraIntervalRef.current = null;
    }
    if (cameraStreamRef.current) {
      cameraStreamRef.current.getTracks().forEach((t) => t.stop());
      cameraStreamRef.current = null;
    }
    if (videoRef.current) {
      videoRef.current.srcObject = null;
    }
    setCameraActive(false);
    addLog("Camera sharing stopped", "info");
  };

  const handleStartAudio = async () => {
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true, video: false });
      audioStreamRef.current = stream;

      // Prefer Opus in WebM for wide browser support and good compression
      const mimeType = MediaRecorder.isTypeSupported("audio/webm;codecs=opus")
        ? "audio/webm;codecs=opus"
        : "audio/webm";

      const recorder = new MediaRecorder(stream, { mimeType, audioBitsPerSecond: 32000 });
      audioRecorderRef.current = recorder;

      recorder.ondataavailable = async (e) => {
        if (!e.data || e.data.size === 0) return;
        const reader = new FileReader();
        reader.onload = async () => {
          const result = reader.result as string;
          const b64 = result.split(",")[1];
          if (b64) {
            try {
              await invoke("send_audio_chunk", { dataB64: b64 });
            } catch {
              // Silently ignore — no peers connected yet
            }
          }
        };
        reader.readAsDataURL(e.data);
      };

      // Emit a chunk every 100ms for low-latency streaming
      recorder.start(100);
      setAudioActive(true);
      addLog("Audio sharing started", "success");
      addToast("Audio sharing on", "success");
    } catch (e: any) {
      addLog(`Microphone error: ${e}`, "error");
      addToast("Microphone access denied", "error");
    }
  };

  const handleStopAudio = () => {
    if (audioRecorderRef.current && audioRecorderRef.current.state !== "inactive") {
      audioRecorderRef.current.stop();
      audioRecorderRef.current = null;
    }
    if (audioStreamRef.current) {
      audioStreamRef.current.getTracks().forEach((t) => t.stop());
      audioStreamRef.current = null;
    }
    setAudioActive(false);
    addLog("Audio sharing stopped", "info");
  };

  const formatBytes = (bytes: number) => {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    if (bytes < 1024 * 1024 * 1024)
      return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  };

  const activeTransfers = Array.from(fileTransfers.values());
  const discovered = Array.from(discoveredPeers.values()).filter(
    (d) => !peers.some((p) => p.id === d.id)
  );

  const isRemote = focus !== "Local";
  const remotePeerId = isRemote
    ? (focus as { Remote: string }).Remote
    : null;

  const isNeighborSet = (
    peerId: string,
    edge: string,
    screenId?: string
  ) => {
    return config?.neighbors?.some(
      (n) =>
        n.peer_id === peerId &&
        n.edge === edge.charAt(0).toUpperCase() + edge.slice(1) &&
        (screenId ? n.screen_id === screenId : !n.screen_id)
    );
  };

  return (
    <div className="app">
      {/* Toast notifications */}
      <div className="toast-container">
        {toasts.map((t) => (
          <div key={t.id} className={`toast toast-${t.level}`}>
            {t.text}
          </div>
        ))}
      </div>

      {/* First-Run Setup Wizard */}
      {isFirstRun && (
        <div className="permissions-overlay">
          <div className="permissions-modal" style={{ maxWidth: 480 }}>
            <h2 style={{ marginBottom: 6, fontSize: 20 }}>Welcome to ShareFlow</h2>
            <p style={{ color: "#aaa", marginBottom: 24, fontSize: 13, lineHeight: 1.5 }}>
              Choose how this machine will be used. You can change this later in Settings.
            </p>

            <div style={{ display: "flex", gap: 12, marginBottom: 20 }}>
              {/* Host card */}
              <div
                onClick={() => setWizardMode("host")}
                style={{
                  flex: 1,
                  padding: "16px 14px",
                  borderRadius: 8,
                  border: `2px solid ${wizardMode === "host" ? "#e94560" : "#333"}`,
                  cursor: "pointer",
                  background: wizardMode === "host" ? "rgba(233,69,96,0.08)" : "#1a1a2e",
                  transition: "border-color 0.15s",
                }}
              >
                <div style={{ fontSize: 28, marginBottom: 8 }}>🖥️</div>
                <div style={{ fontWeight: 600, fontSize: 14, marginBottom: 6 }}>Host</div>
                <div style={{ fontSize: 12, color: "#888", lineHeight: 1.5 }}>
                  Full app. Owns settings, controls connected agents. Use this on your main machine.
                </div>
              </div>

              {/* Agent card */}
              <div
                onClick={() => setWizardMode("agent")}
                style={{
                  flex: 1,
                  padding: "16px 14px",
                  borderRadius: 8,
                  border: `2px solid ${wizardMode === "agent" ? "#e94560" : "#333"}`,
                  cursor: "pointer",
                  background: wizardMode === "agent" ? "rgba(233,69,96,0.08)" : "#1a1a2e",
                  transition: "border-color 0.15s",
                }}
              >
                <div style={{ fontSize: 28, marginBottom: 8 }}>📡</div>
                <div style={{ fontWeight: 600, fontSize: 14, marginBottom: 6 }}>Agent</div>
                <div style={{ fontSize: 12, color: "#888", lineHeight: 1.5 }}>
                  Minimal mode. Follows host settings automatically. Use this on secondary machines.
                </div>
              </div>
            </div>

            {wizardMode === "agent" && (
              <div className="settings-group" style={{ marginBottom: 20 }}>
                <label className="settings-label">Host Address</label>
                <input
                  type="text"
                  placeholder="e.g. 192.168.1.100:24800"
                  value={wizardHostAddr}
                  onChange={(e) => setWizardHostAddr(e.target.value)}
                  style={{ width: "100%", boxSizing: "border-box" }}
                  autoFocus
                />
                <span className="settings-hint">IP and port of the host machine</span>
              </div>
            )}

            <button
              onClick={handleCompleteSetup}
              style={{ width: "100%", padding: "10px 0", fontSize: 14 }}
            >
              Get Started
            </button>
          </div>
        </div>
      )}

      {/* macOS Permissions Setup Modal */}
      {showPermissionsModal && (
        <div className="permissions-overlay">
          <div className="permissions-modal">
            <h2 style={{ marginBottom: 6, fontSize: 18 }}>Permissions Required</h2>
            <p style={{ color: "#aaa", marginBottom: 20, fontSize: 13, lineHeight: 1.5 }}>
              ShareFlow needs the following permissions to work correctly on macOS.
            </p>

            <div className="perm-section">
              <div className="perm-title">
                <span>Accessibility</span>
                <span className="perm-badge perm-required">Required</span>
              </div>
              <p className="perm-desc">
                Allows ShareFlow to capture and inject keyboard &amp; mouse events — the core KVM feature.
                Without this, input sharing will not work.
              </p>
              <ol className="perm-steps">
                <li>Click <strong>Open System Settings</strong> below</li>
                <li>Scroll to find <strong>ShareFlow</strong> and toggle it on</li>
                <li>Return here and click <strong>I've Granted Access</strong></li>
              </ol>
              <div className="perm-actions">
                <button onClick={handleOpenAccessibilitySettings}>
                  Open System Settings
                </button>
                <button className="secondary" onClick={handleCheckAccessibilityAgain}>
                  I've Granted Access
                </button>
              </div>
            </div>

            <div className="perm-section">
              <div className="perm-title">
                <span>Network (Firewall)</span>
                <span className="perm-badge perm-auto">Auto-prompted</span>
              </div>
              <p className="perm-desc">
                macOS will automatically show a firewall dialog the first time ShareFlow listens
                for peer connections. Click <strong>Allow</strong> when that dialog appears.
              </p>
            </div>

            <button
              className="secondary"
              style={{ marginTop: 20, fontSize: 12, opacity: 0.7 }}
              onClick={() => setShowPermissionsModal(false)}
            >
              Skip for now (input sharing may not work)
            </button>
          </div>
        </div>
      )}

      {/* Header */}
      <div className="header">
        <h1>ShareFlow {appVersion && <span style={{ fontSize: 12, fontWeight: 400, color: '#888' }}>v{appVersion}</span>} {config?.agent_mode && <span style={{ fontSize: 11, fontWeight: 500, color: '#e94560', background: 'rgba(233,69,96,0.15)', padding: '2px 8px', borderRadius: 4 }}>Agent</span>} <span style={{ fontSize: 10, fontWeight: 400, color: '#666' }}>by Joshua Fourie</span></h1>
        <div className="header-right">
          <div className="status">
            <span
              className={`status-dot ${
                isRemote ? "remote" : peers.length > 0 ? "" : "offline"
              }`}
            />
            {isRemote
              ? "Controlling remote PC"
              : peers.length > 0
              ? `${peers.length} peer(s) connected`
              : "No peers connected"}
          </div>
          {!config?.agent_mode && (
            <button
              className="quit-btn"
              onClick={() => setShowSettings((v) => !v)}
              title="Settings"
              style={{ marginRight: 4 }}
            >
              {showSettings ? "Close Settings" : "Settings"}
            </button>
          )}
          <button
            className="quit-btn"
            onClick={() => invoke("quit_app")}
            title="Quit ShareFlow"
          >
            Quit
          </button>
        </div>
      </div>

      <div className="main">
        {/* Sidebar */}
        <div className="sidebar">
          <div className="sidebar-section">
            <h3>This Machine</h3>
            <div className={`machine-card ${!isRemote ? "self" : ""}`}>
              <div className="name">
                {config?.machine_name || "Loading..."}
              </div>
              <div className="info">
                {localIp}:{config?.port}
              </div>
              <div className="info">{screens.length} display(s)</div>
              {isRemote && (
                <button
                  onClick={handleSwitchLocal}
                  style={{ marginTop: 6, fontSize: 11, padding: "4px 8px" }}
                >
                  Return Focus Here
                </button>
              )}
            </div>
          </div>

          <div className="sidebar-section">
            <h3>Connected Peers</h3>
            {peers.length === 0 && (
              <div style={{ fontSize: 12, color: "#555" }}>No peers yet.</div>
            )}
            {peers.map((peer) => (
              <div
                key={peer.id}
                className={`machine-card ${
                  remotePeerId === peer.id ? "self" : ""
                }`}
              >
                <div className="peer-header">
                  <span className="status-dot connected" />
                  <span className="name">{peer.name}</span>
                </div>
                <div className="info">{peer.screens} display(s)</div>
                <div className="info">{peer.id.slice(0, 8)}...</div>
                <div className="peer-actions">
                  {remotePeerId !== peer.id ? (
                    <button
                      onClick={() => handleSwitchTo(peer.id)}
                      style={{ fontSize: 10, padding: "3px 6px" }}
                    >
                      Switch To
                    </button>
                  ) : (
                    <span style={{ fontSize: 10, color: "#e94560" }}>
                      Active
                    </span>
                  )}
                  <button
                    onClick={() => handleSendFile(peer.id)}
                    className="secondary"
                    style={{ fontSize: 10, padding: "3px 6px" }}
                  >
                    Send File
                  </button>
                </div>
              </div>
            ))}
          </div>

          {/* Discovered Peers */}
          {discovered.length > 0 && (
            <div className="sidebar-section">
              <h3>Discovered on LAN</h3>
              {discovered.map((d) => (
                <div key={d.id} className="machine-card discovered">
                  <div className="name">{d.name}</div>
                  <div className="info">{d.address}</div>
                  <div className="peer-actions" style={{ marginTop: 6 }}>
                    <button
                      onClick={() => handleConnect(d.address)}
                      style={{
                        fontSize: 10,
                        padding: "3px 8px",
                      }}
                    >
                      Connect
                    </button>
                    {config?.trusted_hosts?.some((h) => h.peer_id === d.id) ? (
                      <span style={{ fontSize: 10, color: "#4caf50" }}>Trusted</span>
                    ) : (
                      <button
                        className="secondary"
                        onClick={() => handleAddTrustedHost(d.id, d.name)}
                        style={{ fontSize: 10, padding: "3px 8px" }}
                      >
                        Trust
                      </button>
                    )}
                  </div>
                </div>
              ))}
            </div>
          )}

          {/* Quick info */}
          <div className="sidebar-section">
            <h3>Quick Info</h3>
            <div style={{ fontSize: 11, color: "#777", lineHeight: 1.8 }}>
              <div>
                Focus:{" "}
                <span style={{ color: isRemote ? "#e94560" : "#4caf50" }}>
                  {isRemote ? "Remote" : "Local"}
                </span>
              </div>
              <div>Peers: {peers.length}</div>
              <div>Displays: {screens.length}</div>
            </div>
          </div>
        </div>

        {/* Content */}
        <div className="content">
          {/* Focus Banner */}
          {isRemote && (
            <div className="focus-banner">
              Controlling remote PC — click "Return
              Focus" to switch back
            </div>
          )}

          {/* Settings Panel — hidden in agent mode */}
          {showSettings && !config?.agent_mode && (
            <div className="section settings-panel">
              <h2>Settings</h2>

              {config?.agent_mode && (
                <div style={{ marginBottom: 16, padding: "10px 14px", background: "rgba(233,69,96,0.08)", border: "1px solid rgba(233,69,96,0.3)", borderRadius: 6 }}>
                  <div style={{ fontSize: 12, fontWeight: 600, color: "#e94560", marginBottom: 4 }}>Agent Mode</div>
                  <div style={{ fontSize: 12, color: "#aaa" }}>
                    This machine is running as an agent. Settings like clipboard sync are pushed by the host.
                  </div>
                  <div style={{ fontSize: 12, color: "#888", marginTop: 6 }}>
                    Host: <span style={{ color: "#ccc" }}>{config.host_address || "not set"}</span>
                  </div>
                </div>
              )}

              <div className="settings-group">
                <label className="settings-label">Machine Name</label>
                <input
                  type="text"
                  value={settingsMachineName}
                  onChange={(e) => setSettingsMachineName(e.target.value)}
                  style={{ width: 220 }}
                />
              </div>

              <div className="settings-group">
                <label className="settings-label">Server Port</label>
                <input
                  type="text"
                  value={settingsPort}
                  onChange={(e) => setSettingsPort(e.target.value.replace(/\D/g, ""))}
                  style={{ width: 100 }}
                  placeholder="24800"
                />
                <span className="settings-hint">Port for peer connections (default: 24800)</span>
              </div>

              <div className="settings-group">
                <label className="settings-label">Discovery Port</label>
                <input
                  type="text"
                  value={settingsDiscoveryPort}
                  onChange={(e) => setSettingsDiscoveryPort(e.target.value.replace(/\D/g, ""))}
                  style={{ width: 100 }}
                  placeholder="24801"
                />
                <span className="settings-hint">UDP port for LAN discovery broadcasts (default: 24801)</span>
              </div>

              <div className="settings-group">
                <label className="settings-label" style={{ display: "flex", alignItems: "center", gap: 8 }}>
                  <input
                    type="checkbox"
                    checked={settingsAutoConnect}
                    onChange={(e) => setSettingsAutoConnect(e.target.checked)}
                  />
                  Auto-Connect to Trusted Hosts
                </label>
                <span className="settings-hint">
                  Automatically connect when a trusted host is discovered on the network
                </span>
              </div>

              {/* Primary Keyboard & Mouse */}
              {!config?.agent_mode && (
                <div className="settings-group">
                  <label className="settings-label" style={{ display: "flex", alignItems: "center", gap: 8 }}>
                    <input
                      type="checkbox"
                      checked={settingsIsPrimaryKm}
                      onChange={(e) => setSettingsIsPrimaryKm(e.target.checked)}
                    />
                    Primary Keyboard &amp; Mouse Device
                  </label>
                  <span className="settings-hint">
                    Enable on the machine whose keyboard and mouse controls others. Disable on all secondary machines.
                  </span>
                  {!settingsIsPrimaryKm && (
                    <span className="settings-hint" style={{ color: "#f39c12" }}>
                      This machine will not be able to control other devices.
                    </span>
                  )}
                </div>
              )}

              {/* Sharing features */}
              <div style={{ marginTop: 16, marginBottom: 4, fontSize: 13, color: "#e94560", fontWeight: 600 }}>
                Sharing Features
              </div>

              <div className="settings-group">
                <label className="settings-label" style={{ display: "flex", alignItems: "center", gap: 8 }}>
                  <input
                    type="checkbox"
                    checked={settingsCameraEnabled}
                    onChange={(e) => setSettingsCameraEnabled(e.target.checked)}
                  />
                  Enable Camera KVM
                </label>
                <span className="settings-hint">
                  Allow sharing your webcam feed with connected peers
                </span>
              </div>

              <div className="settings-group">
                <label className="settings-label" style={{ display: "flex", alignItems: "center", gap: 8 }}>
                  <input
                    type="checkbox"
                    checked={settingsAudioEnabled}
                    onChange={(e) => setSettingsAudioEnabled(e.target.checked)}
                  />
                  Enable Audio KVM
                </label>
                <span className="settings-hint">
                  Allow sharing your microphone audio with connected peers (WebM/Opus streaming)
                </span>

              </div>

              <div className="settings-group">
                <label className="settings-label" style={{ display: "flex", alignItems: "center", gap: 8 }}>
                  <input
                    type="checkbox"
                    checked={settingsClipboardEnabled}
                    onChange={(e) => setSettingsClipboardEnabled(e.target.checked)}
                  />
                  Enable Clipboard Sync
                </label>
                <span className="settings-hint">
                  Automatically sync clipboard content between this machine and connected peers. Disable if clipboard sync causes issues with local apps (e.g. screenshot tools).
                </span>
              </div>

              <button onClick={handleSaveSettings} style={{ marginTop: 8, marginBottom: 16 }}>
                Save Settings
              </button>
              <span className="settings-hint" style={{ marginLeft: 12 }}>
                Port changes require app restart
              </span>

              {/* Trusted Hosts */}
              <div style={{ marginTop: 20 }}>
                <h3 style={{ fontSize: 14, color: "#e94560", marginBottom: 10 }}>
                  Trusted Hosts
                </h3>
                <p className="settings-hint" style={{ marginBottom: 10 }}>
                  Peers in this list will be auto-connected when discovered (if enabled above).
                  Add peers from the "Discovered on LAN" sidebar or from connected peers below.
                </p>

                {config?.trusted_hosts && config.trusted_hosts.length > 0 ? (
                  <div className="trusted-hosts-list">
                    {config.trusted_hosts.map((host) => (
                      <div key={host.peer_id} className="trusted-host-item">
                        <div>
                          <span className="trusted-host-name">{host.name}</span>
                          <span className="trusted-host-id">{host.peer_id.slice(0, 12)}...</span>
                        </div>
                        <button
                          className="secondary"
                          onClick={() => handleRemoveTrustedHost(host.peer_id)}
                          style={{ fontSize: 10, padding: "3px 8px" }}
                        >
                          Remove
                        </button>
                      </div>
                    ))}
                  </div>
                ) : (
                  <div style={{ fontSize: 12, color: "#555" }}>No trusted hosts configured.</div>
                )}

                {/* Add connected peers to trusted list */}
                {peers.length > 0 && (
                  <div style={{ marginTop: 12 }}>
                    <span style={{ fontSize: 12, color: "#888" }}>Add connected peer:</span>
                    <div style={{ display: "flex", flexWrap: "wrap", gap: 6, marginTop: 6 }}>
                      {peers
                        .filter((p) => !config?.trusted_hosts?.some((h) => h.peer_id === p.id))
                        .map((p) => (
                          <button
                            key={p.id}
                            className="secondary"
                            onClick={() => handleAddTrustedHost(p.id, p.name)}
                            style={{ fontSize: 10, padding: "3px 8px" }}
                          >
                            + {p.name}
                          </button>
                        ))}
                      {peers.every((p) =>
                        config?.trusted_hosts?.some((h) => h.peer_id === p.id)
                      ) && (
                        <span style={{ fontSize: 11, color: "#555" }}>
                          All connected peers are already trusted
                        </span>
                      )}
                    </div>
                  </div>
                )}
              </div>
            </div>
          )}

          {/* Camera KVM — only shown when enabled in Settings */}
          {config?.camera_sharing_enabled && <div className="section">
            <h2>Camera KVM</h2>
            <p style={{ fontSize: 12, color: "#888", marginBottom: 12 }}>
              Share your webcam across connected machines. Peers will see your
              camera feed in real time.
            </p>
            <div style={{ display: "flex", alignItems: "center", gap: 12, marginBottom: 12 }}>
              <button
                onClick={cameraActive ? handleStopCamera : handleStartCamera}
                style={{ minWidth: 130 }}
              >
                {cameraActive ? "Stop Camera Share" : "Share My Camera"}
              </button>
              {cameraActive && (
                <span style={{ fontSize: 11, color: "#4caf50" }}>
                  Broadcasting to {peers.length} peer(s)
                </span>
              )}
            </div>

            {/* Hidden video + canvas used for frame capture */}
            <video
              ref={videoRef}
              style={{ display: "none" }}
              muted
              playsInline
            />
            <canvas ref={canvasRef} style={{ display: "none" }} />

            {/* Local camera preview */}
            {cameraActive && (
              <div style={{ marginBottom: 16 }}>
                <div style={{ fontSize: 12, color: "#aaa", marginBottom: 6 }}>
                  Your Camera (preview)
                </div>
                <video
                  ref={(el) => {
                    if (el && cameraStreamRef.current) {
                      el.srcObject = cameraStreamRef.current;
                      el.play().catch(() => {});
                    }
                  }}
                  muted
                  playsInline
                  autoPlay
                  style={{
                    width: 240,
                    borderRadius: 6,
                    border: "1px solid #333",
                    background: "#000",
                  }}
                />
              </div>
            )}

            {/* Remote camera feeds */}
            {remoteCameras.size > 0 && (
              <div>
                <div style={{ fontSize: 12, color: "#aaa", marginBottom: 8 }}>
                  Remote Cameras
                </div>
                <div style={{ display: "flex", flexWrap: "wrap", gap: 12 }}>
                  {Array.from(remoteCameras.entries()).map(([peerId, src]) => {
                    const peerName =
                      peers.find((p) => p.id === peerId)?.name ||
                      peerId.slice(0, 8) + "...";
                    return (
                      <div key={peerId}>
                        <div style={{ fontSize: 11, color: "#888", marginBottom: 4 }}>
                          {peerName}
                        </div>
                        <img
                          src={src}
                          alt={peerName}
                          style={{
                            width: 240,
                            borderRadius: 6,
                            border: "1px solid #333",
                            background: "#000",
                            display: "block",
                          }}
                        />
                      </div>
                    );
                  })}
                </div>
              </div>
            )}

            {remoteCameras.size === 0 && !cameraActive && (
              <div style={{ fontSize: 12, color: "#555" }}>
                No camera feeds active. Click "Share My Camera" to broadcast yours, or
                wait for a connected peer to share theirs.
              </div>
            )}
          </div>}

          {/* Audio KVM — only shown when enabled in Settings */}
          {config?.audio_sharing_enabled && (
            <div className="section">
              <h2>Audio KVM</h2>
              <p style={{ fontSize: 12, color: "#888", marginBottom: 12 }}>
                Stream your microphone to connected peers. Their audio will play
                through your speakers via Web Audio. ~100ms latency, WebM/Opus codec.
              </p>
              <div style={{ display: "flex", alignItems: "center", gap: 12, marginBottom: 12 }}>
                <button
                  onClick={audioActive ? handleStopAudio : handleStartAudio}
                  style={{ minWidth: 130 }}
                >
                  {audioActive ? "Stop Audio Share" : "Share My Mic"}
                </button>
                {audioActive && (
                  <span style={{ fontSize: 11, color: "#4caf50" }}>
                    Broadcasting to {peers.length} peer(s)
                  </span>
                )}
              </div>
              {!audioActive && (
                <div style={{ fontSize: 12, color: "#555" }}>
                  Click "Share My Mic" to start streaming your microphone.
                  Connected peers sharing audio will play automatically through
                  your speakers.
                </div>
              )}
            </div>
          )}

          {/* Screen Layout — hidden in agent mode */}
          {!config?.agent_mode && <div className="section">
            <h2>Screen Arrangement</h2>
            <div className="screen-layout">
              {screens.map((s) => (
                <div
                  key={s.id}
                  className={`screen-box ${s.primary ? "active" : ""}`}
                >
                  <div className="label">
                    {config?.machine_name || "This PC"}
                  </div>
                  <div className="res">
                    {s.width}x{s.height}
                  </div>
                  {s.primary && (
                    <div className="res" style={{ color: "#e94560" }}>
                      Primary
                    </div>
                  )}
                </div>
              ))}
              {peers.map((peer) => (
                <div key={peer.id} className="screen-box peer">
                  <div className="label">{peer.name}</div>
                  <div className="res">{peer.screens} screen(s)</div>
                </div>
              ))}
            </div>
          </div>}

          {/* Edge Switching — hidden in agent mode */}
          {peers.length > 0 && !config?.agent_mode && (
            <div className="section">
              <h2>Edge Switching</h2>
              <p style={{ fontSize: 12, color: "#888", marginBottom: 12 }}>
                Assign a peer to a screen edge. Move your mouse to that edge
                to switch control.
                {screens.length > 1 &&
                  " With multiple monitors, only boundary edges trigger switching."}
              </p>
              {peers.map((peer) => (
                <div key={peer.id} className="edge-config-block">
                  <div className="edge-config-label">{peer.name}</div>
                  {screens.length > 1 ? (
                    // Per-monitor edge config
                    screens.map((s) => (
                      <div key={s.id} className="edge-monitor-row">
                        <span className="edge-monitor-name">
                          {s.id.replace(/\\\\.\\/, "")}
                          {s.primary ? " (Primary)" : ""}:
                        </span>
                        {["left", "right", "top", "bottom"].map((edge) => (
                          <button
                            key={edge}
                            className={
                              isNeighborSet(peer.id, edge, s.id)
                                ? ""
                                : "secondary"
                            }
                            onClick={() =>
                              handleSetNeighbor(peer.id, edge, s.id)
                            }
                            style={{
                              fontSize: 10,
                              padding: "3px 8px",
                              marginRight: 3,
                            }}
                          >
                            {edge.charAt(0).toUpperCase() + edge.slice(1)}
                            {isNeighborSet(peer.id, edge, s.id) ? " *" : ""}
                          </button>
                        ))}
                      </div>
                    ))
                  ) : (
                    // Single monitor — global edge config
                    <div className="edge-monitor-row">
                      {["left", "right", "top", "bottom"].map((edge) => (
                        <button
                          key={edge}
                          className={
                            isNeighborSet(peer.id, edge) ? "" : "secondary"
                          }
                          onClick={() => handleSetNeighbor(peer.id, edge)}
                          style={{
                            fontSize: 11,
                            padding: "4px 10px",
                            marginRight: 4,
                          }}
                        >
                          {edge.charAt(0).toUpperCase() + edge.slice(1)}
                          {isNeighborSet(peer.id, edge) ? " *" : ""}
                        </button>
                      ))}
                    </div>
                  )}
                </div>
              ))}
            </div>
          )}

          {/* File Transfers */}
          {(activeTransfers.length > 0 || receivedFiles.length > 0) && (
            <div className="section">
              <h2>File Transfers</h2>
              {activeTransfers.length > 0 && (
                <div className="file-transfers">
                  {activeTransfers.map((t) => (
                    <div key={t.transfer_id} className="transfer-item">
                      <div className="transfer-header">
                        <span className="transfer-name">{t.file_name}</span>
                        <span className="transfer-dir">
                          {t.direction === "send" ? "Sending" : "Receiving"}
                        </span>
                      </div>
                      <div className="progress-bar">
                        <div
                          className="progress-fill"
                          style={{
                            width: `${
                              t.total_bytes > 0
                                ? (t.transferred_bytes / t.total_bytes) * 100
                                : 0
                            }%`,
                          }}
                        />
                      </div>
                      <div className="transfer-info">
                        {formatBytes(t.transferred_bytes)} /{" "}
                        {formatBytes(t.total_bytes)}
                        {t.total_bytes > 0 &&
                          ` (${Math.round(
                            (t.transferred_bytes / t.total_bytes) * 100
                          )}%)`}
                      </div>
                    </div>
                  ))}
                </div>
              )}
              {receivedFiles.length > 0 && (
                <div style={{ marginTop: 12 }}>
                  <h3
                    style={{ fontSize: 13, color: "#888", marginBottom: 8 }}
                  >
                    Received Files
                  </h3>
                  {receivedFiles
                    .slice()
                    .reverse()
                    .map((f, i) => (
                      <div key={i} className="received-file">
                        <span className="received-name">{f.file_name}</span>
                        <span className="received-size">
                          {formatBytes(f.size)}
                        </span>
                      </div>
                    ))}
                </div>
              )}
            </div>
          )}

          {/* Connect */}
          <div className="section">
            <h2>Connect to Peer</h2>
            {config?.agent_mode && config.host_address && peers.length === 0 && (
              <div style={{ marginBottom: 12 }}>
                <button onClick={() => handleConnect(config.host_address)}>
                  Reconnect to Host ({config.host_address})
                </button>
              </div>
            )}
            <div className="connect-form">
              <input
                type="text"
                placeholder="IP:Port (e.g. 192.168.1.100:24800)"
                value={connectAddr}
                onChange={(e) => setConnectAddr(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleConnect()}
                style={{ width: 280 }}
              />
              <button onClick={() => handleConnect()} disabled={!connectAddr}>
                Connect
              </button>
            </div>
            {connectStatus && (
              <div
                style={{
                  marginTop: 8,
                  fontSize: 12,
                  color: connectStatus.startsWith("Error")
                    ? "#f44336"
                    : "#4caf50",
                }}
              >
                {connectStatus}
              </div>
            )}
          </div>

          {/* Network Info */}
          <div className="section">
            <h2>Network Info</h2>
            <div className="info-grid">
              <span className="label">Local IP</span>
              <span className="value">{localIp || "..."}</span>
              <span className="label">Port</span>
              <span className="value">{config?.port || "..."}</span>
              <span className="label">Peer ID</span>
              <span className="value">
                {config?.peer_id?.slice(0, 16) || "..."}...
              </span>
              <span className="label">Focus</span>
              <span
                className="value"
                style={{ color: isRemote ? "#e94560" : "#4caf50" }}
              >
                {isRemote
                  ? `Remote (${remotePeerId?.slice(0, 8)}...)`
                  : "Local"}
              </span>
            </div>
          </div>

          {/* Log */}
          <div className="section">
            <h2>Activity Log</h2>
            <div className="log" ref={logRef}>
              {logs.length === 0 && (
                <div className="log-entry">Waiting for activity...</div>
              )}
              {logs.map((entry, i) => (
                <div key={i} className={`log-entry ${entry.level}`}>
                  {entry.text}
                </div>
              ))}
            </div>
          </div>

          {/* Diagnostics */}
          <div className="section">
            <button
              className="secondary"
              onClick={() => setShowDiag((v) => !v)}
              style={{ fontSize: 12, padding: "6px 12px", marginBottom: showDiag ? 8 : 0 }}
            >
              {showDiag ? "Hide Diagnostics" : "Show Diagnostics"}
            </button>
            {showDiag && (
              <div className="log" ref={diagRef} style={{ maxHeight: 300 }}>
                {diagLines.length === 0 && (
                  <div className="log-entry">No diagnostic events yet...</div>
                )}
                {diagLines.map((line, i) => (
                  <div key={i} className="log-entry info">
                    {line}
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

export default App;
