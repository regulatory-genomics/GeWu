// gewu-web / static / canvas.js
//
// Frontend canvas controller. Listens for canvas events that ride on the
// existing GeWu tool-event SSE stream (see tachyon-canvas/docs/sse-bridge.md)
// and renders into either the Work Panel canvas host (default) or an optional popup.
//
// MVP renders a kind-tagged placeholder body. The real GenomeCanvas component
// from tachyon-canvas mounts here in Phase 3 by replacing the body builder
// for kind === 'genome'.
//
// Public API:
//   window.canvasController.handle(eventPayload)
//   window.canvasController.getCurrent() -> {canvas_id, kind, ...} | null
//   window.canvasController.registerRenderer(kind, fn(state, host) -> void)
//   window.canvasController.mountDock()
//   window.canvasController.refresh()
//
// The page MUST contain a <div id="canvas-overlay" hidden> element.

(function () {
  const overlay = document.getElementById('canvas-overlay');
  if (!overlay) {
    console.warn('[canvas] missing #canvas-overlay; canvas events will be ignored');
    return;
  }

  // canvas_id -> {state}
  const canvases = new Map();
  // kind -> render fn
  const renderers = new Map();
  let activeCanvasId = null;
  let activeHost = 'work-panel';
  let currentSessionId = null;
  let trayEl = null;
  let popupResizeObserver = null;

  function ensureShell() {
    if (overlay.dataset.shell === 'ready') return;
    overlay.innerHTML = `
      <div class="canvas-overlay-backdrop" data-action="return-work-panel"></div>
      <div class="canvas-overlay-frame" role="dialog" aria-modal="true">
        <header class="canvas-overlay-header">
          <span class="canvas-overlay-kind"></span>
          <span class="canvas-overlay-title"></span>
          <span class="canvas-overlay-id"></span>
          <span class="canvas-overlay-flex"></span>
          <button type="button" class="canvas-overlay-dock" data-action="return-work-panel" title="Return to Work Panel">Work Panel</button>
          <button type="button" class="canvas-overlay-close" data-action="return-work-panel" title="Close popup">×</button>
        </header>
        <div class="canvas-overlay-body"></div>
        <footer class="canvas-overlay-footer">
          <span class="canvas-overlay-status"></span>
        </footer>
      </div>
    `;
    overlay.dataset.shell = 'ready';
    overlay.addEventListener('click', (e) => {
      const action = e.target?.dataset?.action;
      if (action === 'return-work-panel') returnToDock();
    });
  }

  function getDock() {
    return document.getElementById('canvas-dock');
  }

  function getDockBody() {
    return getDock()?.querySelector('.canvas-dock-body') || null;
  }

  function updateDockHeader(state) {
    const dock = getDock();
    if (!dock) return;
    const kindEl = dock.querySelector('.canvas-dock-kind');
    const titleEl = dock.querySelector('.canvas-dock-title');
    const idEl = dock.querySelector('.canvas-dock-id');
    const statusEl = dock.querySelector('.canvas-dock-status');
    const popupButton = dock.querySelector('.canvas-dock-popup');
    if (kindEl) kindEl.textContent = state?.kind || '';
    if (titleEl) titleEl.textContent = state?.title || 'Canvas';
    if (idEl) idEl.textContent = state?.canvas_id || '';
    if (statusEl) statusEl.textContent = state ? `updated ${state.updated_at}` : 'Persistent Work Panel canvas';
    if (popupButton) popupButton.hidden = !state;
  }

  function renderDockEmpty(message = 'Canvas shell ready. Open a canvas to load content here.') {
    const bodyEl = getDockBody();
    if (!bodyEl) return;
    bodyEl.innerHTML = '';
    const empty = document.createElement('div');
    empty.className = 'canvas-dock-empty';
    empty.textContent = message;
    bodyEl.appendChild(empty);
  }

  function renderDockActive(state, entry) {
    const bodyEl = getDockBody();
    if (!bodyEl) return;
    if (!state) {
      renderDockEmpty();
      return;
    }
    bodyEl.innerHTML = '';
    const renderer = renderers.get(state.kind);
    if (renderer) {
      try {
        renderer(state, bodyEl);
      } catch (err) {
        console.error('[canvas] renderer error', err);
        bodyEl.appendChild(buildPlaceholder(state, err.message));
      }
    } else {
      bodyEl.appendChild(buildPlaceholder(state));
    }
    if (entry) entry.state.visible = true;
  }

  function renderPopupActive(state) {
    const bodyEl = overlay.querySelector('.canvas-overlay-body');
    if (!bodyEl) return;
    const kindEl = overlay.querySelector('.canvas-overlay-kind');
    const titleEl = overlay.querySelector('.canvas-overlay-title');
    const idEl = overlay.querySelector('.canvas-overlay-id');
    const statusEl = overlay.querySelector('.canvas-overlay-status');
    if (kindEl) kindEl.textContent = state.kind;
    if (titleEl) titleEl.textContent = state.title || '(untitled)';
    if (idEl) idEl.textContent = state.canvas_id;
    if (statusEl) statusEl.textContent = `updated ${state.updated_at}`;
    bodyEl.innerHTML = '';
    const renderer = renderers.get(state.kind);
    if (renderer) {
      try {
        renderer(state, bodyEl);
      } catch (err) {
        console.error('[canvas] renderer error', err);
        bodyEl.appendChild(buildPlaceholder(state, err.message));
      }
    } else {
      bodyEl.appendChild(buildPlaceholder(state));
    }
  }

  function ensurePopupSize() {
    try {
      const raw = localStorage.getItem('gewu_canvas_popup_size');
      if (!raw) return;
      const parsed = JSON.parse(raw);
      if (Number.isFinite(parsed.width)) {
        document.documentElement.style.setProperty('--canvas-popup-width', `${Math.min(Math.max(parsed.width, 520), 1600)}px`);
      }
      if (Number.isFinite(parsed.height)) {
        document.documentElement.style.setProperty('--canvas-popup-height', `${Math.min(Math.max(parsed.height, 360), 1000)}px`);
      }
    } catch (e) {
      console.warn('[canvas] failed to restore popup size', e);
    }
  }

  function observePopupSize() {
    if (typeof ResizeObserver === 'undefined') return;
    const frame = overlay.querySelector('.canvas-overlay-frame');
    if (!frame || popupResizeObserver) return;
    popupResizeObserver = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect;
      if (!rect) return;
      try {
        localStorage.setItem('gewu_canvas_popup_size', JSON.stringify({
          width: Math.round(rect.width),
          height: Math.round(rect.height),
        }));
      } catch (e) {
        console.warn('[canvas] failed to persist popup size', e);
      }
    });
    popupResizeObserver.observe(frame);
  }

  function activateCanvasTab() {
    const tab = document.querySelector('.work-tab[data-work-tab="canvas"]');
    if (tab) tab.click();
    if (window.state) window.state.workTab = 'canvas';
  }

  function returnToDock(shouldRender = true) {
    activeHost = 'work-panel';
    overlay.hidden = true;
    overlay.classList.remove('visible');
    document.body.classList.remove('canvas-overlay-open');
    activateCanvasTab();
    if (activeCanvasId && canvases.has(activeCanvasId)) {
      const entry = canvases.get(activeCanvasId);
      entry.state.visible = true;
      entry.state.focused = true;
      updateDockHeader(entry.state);
      if (shouldRender) renderDockActive(entry.state, entry);
    } else {
      updateDockHeader(null);
      renderDockEmpty();
    }
    renderTray();
  }

  function closeActiveCanvas() {
    if (activeCanvasId && canvases.has(activeCanvasId)) {
      const state = canvases.get(activeCanvasId).state;
      canvases.set(activeCanvasId, { state: { ...state, visible: false, focused: false } });
    }
    activeCanvasId = null;
    activeHost = 'work-panel';
    overlay.hidden = true;
    overlay.classList.remove('visible');
    document.body.classList.remove('canvas-overlay-open');
    updateDockHeader(null);
    renderDockEmpty();
    renderTray();
  }

  function resetSession(sessionId) {
    currentSessionId = sessionId || null;
    canvases.clear();
    activeCanvasId = null;
    activeHost = 'work-panel';
    if (popupResizeObserver) {
      popupResizeObserver.disconnect();
      popupResizeObserver = null;
    }
    overlay.hidden = true;
    overlay.classList.remove('visible');
    document.body.classList.remove('canvas-overlay-open');
    updateDockHeader(null);
    renderDockEmpty();
    renderTray();
  }

  function ensureTray() {
    if (trayEl) return trayEl;
    trayEl = document.createElement('div');
    trayEl.className = 'canvas-tray';
    trayEl.setAttribute('aria-label', 'Hidden canvases');
    document.body.appendChild(trayEl);
    return trayEl;
  }

  function renderTray() {
    const tray = ensureTray();
    const hidden = Array.from(canvases.values())
      .map((entry) => entry.state)
      .filter((state) => state.visible === false);
    tray.innerHTML = '';
    tray.hidden = hidden.length === 0;
    if (hidden.length > 0) {
      const heading = document.createElement('div');
      heading.className = 'canvas-tray-heading';
      heading.textContent = 'Canvas hidden — click to reopen';
      tray.appendChild(heading);
    }
    for (const state of hidden) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'canvas-tray-item';
      button.textContent = `↗ Reopen ${state.kind}: ${state.title || state.canvas_id}`;
      button.title = `Reopen ${state.canvas_id}`;
      button.addEventListener('click', () => show(state.canvas_id));
      tray.appendChild(button);
    }
  }

  function show(canvasId, host = 'work-panel') {
    activeCanvasId = canvasId;
    activeHost = host === 'popup' ? 'popup' : 'work-panel';
    const entry = canvases.get(canvasId);
    if (!entry) {
      updateDockHeader(null);
      renderDockEmpty('Canvas not found.');
      renderTray();
      return;
    }
    if (entry) {
      entry.state.visible = true;
      entry.state.focused = true;
    }
    for (const [id, entry] of canvases.entries()) {
      if (id !== canvasId) entry.state.focused = false;
    }
    if (activeHost === 'popup') {
      ensureShell();
      ensurePopupSize();
      observePopupSize();
      overlay.hidden = false;
      overlay.classList.add('visible');
      document.body.classList.add('canvas-overlay-open');
      updateDockHeader(entry.state);
      renderDockEmpty('Canvas is open in popup. Use the Work Panel button to return it to the persistent view.');
    } else {
      overlay.hidden = true;
      overlay.classList.remove('visible');
      document.body.classList.remove('canvas-overlay-open');
      activateCanvasTab();
      updateDockHeader(entry?.state || null);
      renderDockActive(entry?.state, entry);
    }
    render(canvasId);
    renderTray();
  }

  function hide() {
    returnToDock();
  }

  function render(canvasId) {
    const entry = canvases.get(canvasId);
    if (!entry) return;
    const { state } = entry;
    if (activeHost === 'popup') {
      renderPopupActive(state);
    } else {
      updateDockHeader(state);
      renderDockActive(state, entry);
    }
  }

  function buildPlaceholder(state, errorMessage) {
    const wrap = document.createElement('div');
    wrap.className = 'canvas-placeholder';
    const heading = document.createElement('div');
    heading.className = 'canvas-placeholder-heading';
    heading.textContent = `kind = ${state.kind}`;
    wrap.appendChild(heading);
    const note = document.createElement('p');
    note.className = 'canvas-placeholder-note';
    if (errorMessage) {
      note.textContent = `Renderer error: ${errorMessage}`;
    } else if (state.kind === 'genome') {
      note.textContent =
        'GenomeCanvas is loading. The Work Panel shell is ready and will fill in as soon as the renderer mounts.';
    } else {
      note.textContent = `No renderer registered for kind="${state.kind}".`;
    }
    wrap.appendChild(note);
    const pre = document.createElement('pre');
    pre.className = 'canvas-placeholder-payload';
    pre.textContent = JSON.stringify(state.payload || {}, null, 2);
    wrap.appendChild(pre);
    return wrap;
  }

  function handle(payload) {
    if (!payload || typeof payload.event !== 'string') return false;
    if (!payload.event.startsWith('canvas:')) return false;

    if (payload.event === 'canvas:open') {
      const initial = payload.initial_state || {};
      const id = payload.canvas_id || initial.canvas_id;
      if (!id) {
        console.warn('[canvas] canvas:open missing canvas_id', payload);
        return true;
      }
      // Some open events arrive with a "pending" canvas_id from start_metadata
      // before the authoritative id from execute(). Reconcile by id below.
      if (id === 'pending' && activeCanvasId && canvases.has(activeCanvasId)) {
        // Will be reconciled by the matching tool_finish event_payload.
        return true;
      }
      canvases.set(id, {
        state: { ...initial, canvas_id: id, kind: payload.kind || initial.kind },
      });
      const mode = payload.mode || initial.mode || 'docked';
      show(id, mode === 'overlay' || mode === 'fullscreen' ? 'popup' : 'work-panel');
      return true;
    }

    if (payload.event === 'canvas:patch') {
      const id = payload.canvas_id;
      if (!id || !canvases.has(id)) return true;
      const entry = canvases.get(id);
      const patch = payload.patch || {};
      // Mirror the server-side merge rules.
      if (typeof patch.title === 'string') entry.state.title = patch.title;
      if (typeof patch.mode === 'string') entry.state.mode = patch.mode;
      if (typeof patch.visible === 'boolean') entry.state.visible = patch.visible;
      if (typeof patch.focused === 'boolean') entry.state.focused = patch.focused;
      if (patch.payload && typeof patch.payload === 'object') {
        const existing = entry.state.payload && typeof entry.state.payload === 'object'
          ? entry.state.payload
          : {};
        entry.state.payload = { ...existing, ...patch.payload };
      }
      if (payload.updated_at) entry.state.updated_at = payload.updated_at;
      if (id === activeCanvasId) render(id);
      renderTray();
      return true;
    }

    if (payload.event === 'canvas:focus') {
      const id = payload.canvas_id;
      if (!id || !canvases.has(id)) return true;
      show(id);
      return true;
    }

    if (payload.event === 'canvas:close') {
      const id = payload.canvas_id;
      if (canvases.has(id)) {
        const state = payload.state || canvases.get(id).state;
        canvases.set(id, { state: { ...state, visible: false, focused: false } });
      }
      if (id === activeCanvasId) closeActiveCanvas();
      else renderTray();
      return true;
    }

    return true;
  }

  function getCurrent() {
    if (!activeCanvasId) return null;
    return canvases.get(activeCanvasId)?.state || null;
  }

  function setSession(sessionId) {
    if (!sessionId || sessionId === currentSessionId) return;
    resetSession(sessionId);
  }

  function registerRenderer(kind, fn) {
    if (typeof kind !== 'string' || typeof fn !== 'function') return;
    renderers.set(kind, fn);
    if (activeCanvasId) {
      const entry = canvases.get(activeCanvasId);
      if (entry && entry.state.kind === kind) render(activeCanvasId);
    }
  }

  /**
   * Push a frontend-driven patch back to the server (browser → server inverse
   * channel). Used by kind-specific renderers when the user pans/zooms/selects.
   */
  async function pushPatch(canvasId, patch, opts = {}) {
    const sessionId = opts.sessionId || (window.state && window.state.currentSessionId);
    if (!sessionId || !canvasId) return null;
    try {
      const resp = await fetch(
        `/api/sessions/${encodeURIComponent(sessionId)}/canvas/${encodeURIComponent(canvasId)}/state`,
        {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ patch }),
        }
      );
      if (!resp.ok) return null;
      const data = await resp.json();
      if (data && data.state) {
        canvases.set(canvasId, { state: data.state });
        if (canvasId === activeCanvasId) render(canvasId);
        return data.state;
      }
    } catch (e) {
      console.warn('[canvas] pushPatch failed', e);
    }
    return null;
  }

  function mountDock() {
    if (!activeCanvasId) {
      updateDockHeader(null);
      renderDockEmpty();
      return;
    }
    const entry = canvases.get(activeCanvasId);
    if (!entry) {
      updateDockHeader(null);
      renderDockEmpty();
      return;
    }
    if (activeHost === 'popup') {
      updateDockHeader(entry.state);
      renderDockEmpty('Canvas is open in popup. Use the Work Panel button to return it to the persistent view.');
      return;
    }
    updateDockHeader(entry.state);
    renderDockActive(entry.state, entry);
  }

  function refresh() {
    if (!activeCanvasId) return;
    render(activeCanvasId);
  }

  document.addEventListener('click', (e) => {
    const action = e.target?.dataset?.action;
    if (action === 'open-popup' && activeCanvasId) {
      show(activeCanvasId, 'popup');
    }
  });

  window.canvasController = {
    handle,
    getCurrent,
    registerRenderer,
    pushPatch,
    show,
    hide,
    setSession,
    mountDock,
    refresh,
  };
})();
