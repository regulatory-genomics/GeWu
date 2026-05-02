// gewu-web / static / canvas.js
//
// Frontend canvas controller. Listens for canvas events that ride on the
// existing GeWu tool-event SSE stream (see tachyon-canvas/docs/sse-bridge.md)
// and renders a wide overlay surface.
//
// MVP renders a kind-tagged placeholder body. The real GenomeCanvas component
// from tachyon-canvas mounts here in Phase 3 by replacing the body builder
// for kind === 'genome'.
//
// Public API:
//   window.canvasController.handle(eventPayload)
//   window.canvasController.getCurrent() -> {canvas_id, kind, ...} | null
//   window.canvasController.registerRenderer(kind, fn(state, host) -> void)
//
// The page MUST contain a <div id="canvas-overlay" hidden> element.

(function () {
  const overlay = document.getElementById('canvas-overlay');
  if (!overlay) {
    console.warn('[canvas] missing #canvas-overlay; canvas events will be ignored');
    return;
  }

  // canvas_id -> {state, hostEl}
  const canvases = new Map();
  // kind -> render fn
  const renderers = new Map();
  let activeCanvasId = null;

  function ensureShell() {
    if (overlay.dataset.shell === 'ready') return;
    overlay.innerHTML = `
      <div class="canvas-overlay-backdrop" data-action="close-overlay"></div>
      <div class="canvas-overlay-frame" role="dialog" aria-modal="true">
        <header class="canvas-overlay-header">
          <span class="canvas-overlay-kind"></span>
          <span class="canvas-overlay-title"></span>
          <span class="canvas-overlay-id"></span>
          <span class="canvas-overlay-flex"></span>
          <button type="button" class="canvas-overlay-close" data-action="close-overlay" title="Close">×</button>
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
      if (action === 'close-overlay') hide();
    });
  }

  function show(canvasId) {
    ensureShell();
    activeCanvasId = canvasId;
    overlay.hidden = false;
    overlay.classList.add('visible');
    document.body.classList.add('canvas-overlay-open');
    render(canvasId);
  }

  function hide() {
    overlay.hidden = true;
    overlay.classList.remove('visible');
    document.body.classList.remove('canvas-overlay-open');
    activeCanvasId = null;
  }

  function render(canvasId) {
    const entry = canvases.get(canvasId);
    if (!entry) return;
    const { state } = entry;
    const kindEl = overlay.querySelector('.canvas-overlay-kind');
    const titleEl = overlay.querySelector('.canvas-overlay-title');
    const idEl = overlay.querySelector('.canvas-overlay-id');
    const bodyEl = overlay.querySelector('.canvas-overlay-body');
    const statusEl = overlay.querySelector('.canvas-overlay-status');
    if (kindEl) kindEl.textContent = state.kind;
    if (titleEl) titleEl.textContent = state.title || '(untitled)';
    if (idEl) idEl.textContent = state.canvas_id;
    if (statusEl) statusEl.textContent = `updated ${state.updated_at}`;
    if (!bodyEl) return;
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
        'GenomeCanvas frontend (tachyon-canvas) has not been mounted yet. ' +
        'This placeholder will be replaced once Phase 3 ships.';
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
      show(id);
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
      canvases.delete(id);
      if (id === activeCanvasId) hide();
      return true;
    }

    return true;
  }

  function getCurrent() {
    if (!activeCanvasId) return null;
    return canvases.get(activeCanvasId)?.state || null;
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
    const token = opts.token || (window.token || '');
    if (!sessionId || !canvasId) return null;
    try {
      const resp = await fetch(
        `/api/sessions/${encodeURIComponent(sessionId)}/canvas/${encodeURIComponent(canvasId)}/state?token=${encodeURIComponent(token)}`,
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

  window.canvasController = {
    handle,
    getCurrent,
    registerRenderer,
    pushPatch,
    show,
    hide,
  };
})();
