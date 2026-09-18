import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

const appWindow = getCurrentWindow();

export default function TitleBar() {
  const [isMaximized, setIsMaximized] = useState(false);

  useEffect(() => {
    appWindow.isMaximized().then(setIsMaximized);
    // Note: deliberately not re-querying isMaximized() on every resize.
    // On macOS, an undecorated window's isMaximized() call rebuilds the
    // window's theme frame to compute the answer, which itself fires a
    // resize notification — querying it here would recurse into an
    // infinite loop that pins the main thread and blocks all window input.
  }, []);

  return (
    <div className="titlebar" data-tauri-drag-region>
      <div className="titlebar-brand" data-tauri-drag-region>
        <img src="/shareflow-logo.png" alt="" className="titlebar-logo" />
        <span className="titlebar-title" data-tauri-drag-region>ShareFlow</span>
      </div>
      <div className="titlebar-controls">
        <button
          className="titlebar-btn"
          aria-label="Minimize"
          onClick={() => appWindow.minimize()}
        >
          <svg width="10" height="10" viewBox="0 0 10 10">
            <rect x="0" y="4.5" width="10" height="1" fill="currentColor" />
          </svg>
        </button>
        <button
          className="titlebar-btn"
          aria-label={isMaximized ? "Restore" : "Maximize"}
          onClick={async () => {
            await appWindow.toggleMaximize();
            setIsMaximized(await appWindow.isMaximized());
          }}
        >
          {isMaximized ? (
            <svg width="10" height="10" viewBox="0 0 10 10">
              <rect x="1.5" y="0.5" width="7" height="7" fill="none" stroke="currentColor" strokeWidth="1" />
              <path d="M0.5 2.5V9.5H7.5" fill="none" stroke="currentColor" strokeWidth="1" />
              <rect x="0.5" y="2.5" width="7" height="7" fill="var(--ballast-panel)" stroke="currentColor" strokeWidth="1" />
            </svg>
          ) : (
            <svg width="10" height="10" viewBox="0 0 10 10">
              <rect x="0.5" y="0.5" width="9" height="9" fill="none" stroke="currentColor" strokeWidth="1" />
            </svg>
          )}
        </button>
        <button
          className="titlebar-btn titlebar-close"
          aria-label="Close"
          onClick={() => appWindow.close()}
        >
          <svg width="10" height="10" viewBox="0 0 10 10">
            <path d="M0.5 0.5L9.5 9.5M9.5 0.5L0.5 9.5" stroke="currentColor" strokeWidth="1.2" />
          </svg>
        </button>
      </div>
    </div>
  );
}
