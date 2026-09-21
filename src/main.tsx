import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { QuickAdd } from "./components/QuickAdd";
import { TaskPeek } from "./components/TaskPeek";
import "./styles/tokens.css";
import "./styles/global.css";
import "./styles/animations.css";

// The shortcut-kit popups (src-tauri/src/shortcuts.rs) load this same bundle on
// a hash route; only the main window runs the full app + engine hook.
const route = window.location.hash;
if (route === "#/quick-add" || route === "#/peek") document.documentElement.classList.add("popup");

function Root() {
  if (route === "#/quick-add") return <QuickAdd />;
  if (route === "#/peek") return <TaskPeek />;
  return <App />;
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
