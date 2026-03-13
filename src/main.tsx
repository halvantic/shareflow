import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";

// Disable the native browser right-click context menu across the whole app.
document.addEventListener("contextmenu", (e) => e.preventDefault());

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
