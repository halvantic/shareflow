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
  const logRef = useRef<HTMLDivElement>(null);
  const diagRef = useRef<HTMLDivElement>(null);

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

    addLog("Press Scroll Lock to toggle focus between PCs", "info");

    const interval = setInterval(() => {
      invoke<PeerInfo[]>("get_peers").then(setPeers);
      invoke<any>("get_focus_state").then(setFocus);
    }, 1000);

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
      }
    });

    return () => {
      clearInterval(interval);
      clearInterval(cleanupInterval);
      unlisten.then((f) => f());
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

      {/* Header */}
      <div className="header">
        <h1>ShareFlow {appVersion && <span style={{ fontSize: 12, fontWeight: 400, color: '#888' }}>v{appVersion}</span>} <span style={{ fontSize: 10, fontWeight: 400, color: '#666' }}>by Joshua Fourie</span></h1>
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
                  <button
                    onClick={() => handleConnect(d.address)}
                    style={{
                      marginTop: 6,
                      fontSize: 10,
                      padding: "3px 8px",
                    }}
                  >
                    Connect
                  </button>
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
              Controlling remote PC — press Scroll Lock or click "Return
              Focus" to switch back
            </div>
          )}

          {/* Screen Layout */}
          <div className="section">
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
          </div>

          {/* Edge Switching */}
          {peers.length > 0 && (
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
