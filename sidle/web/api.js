// Thin wrappers around the global Tauri API exposed via withGlobalTauri.

const TAURI = window.__TAURI__;
if (!TAURI) {
  console.error("Tauri global API is missing — withGlobalTauri must be true.");
}

window.api = {
  invoke: (cmd, args) => TAURI.core.invoke(cmd, args),
  listen: (event, handler) => TAURI.event.listen(event, handler),

  // Webview drag-drop. Returns an unlisten function.
  onDragDrop: async (handler) => {
    const webview = TAURI.webview.getCurrentWebview();
    return await webview.onDragDropEvent(handler);
  },

  // Convert a filesystem path into a URL the WebView can load (asset:// scheme).
  fileUrl: (path) => (path ? TAURI.core.convertFileSrc(path) : null),

  openFileDialog: async () => {
    return await TAURI.core.invoke("library_pick_files");
  },
};
