// GeWu web GUI client.
//
// The server renders all transcript HTML; this script only manages sessions,
// applies snapshots/deltas, and forwards user actions. Model-authored text is
// inserted with textContent, never innerHTML.

import {panelHost} from './panels.mjs';

(() => {
  "use strict";

  const TOKEN_KEY = "gewu-gui-token";
  const SIDEBAR_KEY = "gewu-gui-sidebar";
  const SESSIONS_KEY = "gewu-gui-sessions";
  const WORKSPACES_KEY = "gewu-gui-collapsed-workspaces";
  const MAX_RECONNECT_MS = 60000;

  const el = {
    app: document.getElementById("app"),
    offline: document.getElementById("offline"),
    offlineText: document.getElementById("offline-text"),
    retry: document.getElementById("retry"),
    interrupt: document.getElementById("interrupt"),
    transcript: document.getElementById("transcript"),
    status: document.getElementById("status"),
    composer: document.getElementById("composer"),
    prompt: document.getElementById("prompt"),
    send: document.getElementById("send"),
    toasts: document.getElementById("toasts"),
    alerts: document.getElementById("alerts"),
    login: document.getElementById("login"),
    loginForm: document.getElementById("login-form"),
    loginError: document.getElementById("login-error"),
    token: document.getElementById("token"),
    sidebar: document.getElementById("sidebar"),
    sessionSidebar: document.getElementById("session-sidebar"),
    sessionsExpand: document.getElementById("sessions-expand"),
    filesExpand: document.getElementById("files-expand"),
    drawerBackdrop: document.getElementById("drawer-backdrop"),
    filePreview: document.getElementById("file-preview"),
    fileMenu: document.getElementById("file-menu"),
    workflowZoom: document.getElementById("workflow-zoom"),
  };

  const state = {
    ws: null,
    connected: false,
    ready: false,
    authRequired: false,
    token: loadToken(),
    active: 0,
    transcriptTab: null,
    stuck: true,
    scrollTops: new Map(),
    busy: false,
    attempt: 0,
    reconnectTimer: null,
    countdownTimer: null,
    reconnectAt: 0,
    refreshTimer: null,
    lastToast: { text: "", at: 0 },
    lastAlert: { text: "", at: 0 },
    loadingEarlier: false,
    follow: new Map(),
    workflowZoom: { runId: null, scale: 1, x: 0, y: 0 },
    fileMenu: { path: null, entry: null },
    previewWindow: null,
  };

  function loadToken() {
    try {
      return localStorage.getItem(TOKEN_KEY) || "";
    } catch {
      return "";
    }
  }

  function saveToken(token) {
    try {
      localStorage.setItem(TOKEN_KEY, token);
    } catch {
      // Storage can be unavailable in private modes; the token still works
      // for this page.
    }
  }

  function forgetToken() {
    try {
      localStorage.removeItem(TOKEN_KEY);
    } catch {
      // Ignore.
    }
  }

  // -- sidebar ------------------------------------------------------------

  // The files sidebar starts closed; the sessions sidebar starts open except
  // on narrow screens where it covers the transcript.
  function loadSidebarOpen() {
    try {
      const saved = localStorage.getItem(SIDEBAR_KEY);
      if (saved === "open") return true;
      if (saved === "closed") return false;
    } catch {
      // Fall through to the default.
    }
    return false;
  }

  function loadSessionsOpen() {
    try {
      const saved = localStorage.getItem(SESSIONS_KEY);
      if (saved === "open") return true;
      if (saved === "closed") return false;
    } catch {
      // Fall through to the default.
    }
    return !window.matchMedia("(max-width: 800px)").matches;
  }

  function narrowScreen() {
    return window.matchMedia("(max-width: 800px)").matches;
  }

  function setSidebarOpen(open, remember) {
    el.app.classList.toggle("sidebar-collapsed", !open);
    el.filesExpand.setAttribute("aria-expanded", String(open));
    if (!remember) return;
    try {
      localStorage.setItem(SIDEBAR_KEY, open ? "open" : "closed");
    } catch {
      // The state still applies to this page.
    }
  }

  function setSessionsOpen(open, remember) {
    el.app.classList.toggle("sessions-collapsed", !open);
    el.sessionsExpand.setAttribute("aria-expanded", String(open));
    if (!remember) return;
    try {
      localStorage.setItem(SESSIONS_KEY, open ? "open" : "closed");
    } catch {
      // The state still applies to this page.
    }
  }

  function loadCollapsedWorkspaces() {
    try {
      const saved = JSON.parse(localStorage.getItem(WORKSPACES_KEY) || "[]");
      return Array.isArray(saved)
        ? new Set(saved.filter((id) => typeof id === "string"))
        : new Set();
    } catch {
      return new Set();
    }
  }

  const collapsedWorkspaces = loadCollapsedWorkspaces();

  function saveCollapsedWorkspaces() {
    try {
      localStorage.setItem(WORKSPACES_KEY, JSON.stringify([...collapsedWorkspaces]));
    } catch {
      // The state still applies to this page.
    }
  }

  function applyWorkspaceCollapseState() {
    for (const group of el.sessionSidebar.querySelectorAll("[data-session-workspace-group]")) {
      const collapsed = collapsedWorkspaces.has(group.dataset.sessionWorkspaceGroup);
      group.classList.toggle("collapsed", collapsed);
      const toggle = group.querySelector("[data-session-workspace-toggle]");
      if (toggle) toggle.setAttribute("aria-expanded", String(!collapsed));
      const list = group.querySelector(".session-list");
      if (list) list.hidden = collapsed;
    }
  }

  function setSessionSidebarHtml(html) {
    el.sessionSidebar.innerHTML = html || "";
    applyWorkspaceCollapseState();
  }

  function toggleWorkspaceGroup(id) {
    if (collapsedWorkspaces.has(id)) collapsedWorkspaces.delete(id);
    else collapsedWorkspaces.add(id);
    saveCollapsedWorkspaces();
    applyWorkspaceCollapseState();
  }

  function expandWorkspace(id) {
    if (!collapsedWorkspaces.delete(id)) return;
    saveCollapsedWorkspaces();
    applyWorkspaceCollapseState();
  }

  function toggleSessions() {
    const open = el.app.classList.contains("sessions-collapsed");
    if (open && narrowScreen()) setSidebarOpen(false, false);
    setSessionsOpen(open, true);
  }

  function toggleSidebar() {
    const open = el.app.classList.contains("sidebar-collapsed");
    if (open && narrowScreen()) setSessionsOpen(false, false);
    setSidebarOpen(open, true);
  }

  el.drawerBackdrop.addEventListener("click", () => {
    setSessionsOpen(false, false);
    setSidebarOpen(false, false);
  });

  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || !narrowScreen()) return;
    if (!el.app.classList.contains("sessions-collapsed")) {
      setSessionsOpen(false, false);
      event.preventDefault();
      event.stopImmediatePropagation();
    } else if (!el.app.classList.contains("sidebar-collapsed")) {
      setSidebarOpen(false, false);
      event.preventDefault();
      event.stopImmediatePropagation();
    }
  });

  window.matchMedia("(max-width: 800px)").addEventListener("change", (event) => {
    if (!event.matches) return;
    setSessionsOpen(false, false);
    setSidebarOpen(false, false);
  });

  // -- connection ---------------------------------------------------------

  async function boot() {
    try {
      const response = await fetch("/api/config", { cache: "no-store" });
      const config = await response.json();
      state.authRequired = Boolean(config.auth_required);
    } catch (error) {
      showAlert(`Could not reach gewu-gui: ${error.message}`);
    }

    if (state.authRequired && !state.token) {
      showLogin();
      return;
    }
    connect();
  }

  function connect() {
    if (
      state.ws &&
      (state.ws.readyState === WebSocket.OPEN ||
        state.ws.readyState === WebSocket.CONNECTING)
    ) {
      return;
    }
    clearReconnect();

    const scheme = location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${scheme}//${location.host}/ws`;
    const ws = new WebSocket(url);
    state.ws = ws;

    ws.addEventListener("open", () => {
      if (state.ws !== ws) return;
      state.connected = true;
      state.ready = false;
      for (const entry of pluginPending.values()) entry.reject(Error('Disconnected'));
      pluginPending.clear();
      state.attempt = 0;
      state.follow.clear();
      state.loadingEarlier = false;
      setOffline(false);
      updateComposer();
      if (state.authRequired) {
        send({ op: "auth", token: state.token });
      }
    });

    ws.addEventListener("message", (event) => {
      if (state.ws !== ws) return;
      let message;
      try {
        message = JSON.parse(event.data);
      } catch (error) {
        showAlert("Received an invalid message from gewu-gui");
        return;
      }
      handle(message);
    });

    ws.addEventListener("close", () => {
      if (state.ws !== ws) return;
      state.ws = null;
      state.connected = false;
      state.ready = false;
      state.follow.clear();
      state.loadingEarlier = false;
      closePendingPreview();
      updateComposer();
      if (state.authRequired && !state.token) {
        setOffline(false);
        return;
      }
      setOffline(true);
      scheduleReconnect();
    });

    ws.addEventListener("error", () => {
      // The close handler owns reconnection.
    });
  }

  // Cancel a pending reconnect and stop the countdown.
  function clearReconnect() {
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = null;
    clearInterval(state.countdownTimer);
    state.countdownTimer = null;
  }

  // Render the remaining whole seconds from the fixed deadline, so the label
  // stays correct even when background tabs throttle timers.
  function renderCountdown() {
    const seconds = Math.max(0, Math.ceil((state.reconnectAt - Date.now()) / 1000));
    el.offlineText.textContent = `Disconnected — reconnecting in ${seconds}s…`;
  }

  function scheduleReconnect() {
    if (state.reconnectTimer) return;
    const delay = Math.min(MAX_RECONNECT_MS, 500 * 2 ** state.attempt);
    state.attempt += 1;
    state.reconnectAt = Date.now() + delay;
    renderCountdown();
    // Tick faster than once a second so whole-second values never skip.
    state.countdownTimer = setInterval(renderCountdown, 250);
    state.reconnectTimer = setTimeout(() => {
      clearReconnect();
      connect();
    }, delay);
  }

  function setOffline(offline) {
    el.offline.classList.toggle("hidden", !offline);
    if (!offline) {
      clearReconnect();
      el.offlineText.textContent = "Disconnected";
    }
  }

  function send(message) {
    if (!state.ws || state.ws.readyState !== WebSocket.OPEN) {
      showAlert("Not connected to gewu-gui");
      return false;
    }
    state.ws.send(JSON.stringify(message));
    return true;
  }

  const pluginPending = new Map();
  let pluginRequest = 0;
  let pluginTabs = [];
  const panels = panelHost(document.getElementById('plugin-host'), document.getElementById('plugin-toggle'), (tab, plugin) => {
    return new Promise((resolve,reject) => {
      if (!Number.isSafeInteger(tab) || !state.ready) {reject(Error('No connected workspace'));return;}
      const request=++pluginRequest;
      const timer=setTimeout(()=>{pluginPending.delete(request);reject(Error('Plugin request timed out'));},65000);
      pluginPending.set(request,{tab,resolve:value=>{clearTimeout(timer);resolve(value);},reject:error=>{clearTimeout(timer);reject(error);}});
      if (!send({op:'plugin',tab,request,plugin})) {
        pluginPending.get(request).reject(Error('Disconnected'));
        pluginPending.delete(request);
      }
    });
  });

  function syncPanels(message) {
    pluginTabs = message.tabs;
    panels.switchTab(pluginTabs.find(tab=>tab.id===message.active));
  }

  // -- server messages ----------------------------------------------------

  function handle(message) {
    switch (message.type) {
      case "snapshot":
        state.active = message.active;
        state.ready = true;
        state.transcriptTab = message.active;
        state.loadingEarlier = false;
        setTranscript(message.transcript, true);
        setStatus(message.status);
        el.sidebar.innerHTML = message.sidebar || "";
        setSessionSidebarHtml(message.session_sidebar);
        setBusy(message.tabs.some((tab) => tab.id === message.active && tab.busy));
        syncPanels(message);
        break;
      case "tabs": {
        if (state.active !== message.active) {
          const active = message.tabs.find((tab) => tab.id === message.active);
          if (active) expandWorkspace(active.workspace);
        }
        state.active = message.active;
        const alive = new Set(message.tabs.map((tab) => tab.id));
        for (const id of state.scrollTops.keys()) {
          if (!alive.has(id)) state.scrollTops.delete(id);
        }
        for (const id of state.follow.keys()) {
          if (!alive.has(id)) state.follow.delete(id);
        }
        setBusy(message.tabs.some((tab) => tab.id === message.active && tab.busy));
        syncPanels(message);
        break;
      }
      case 'plugin': {
        const entry=pluginPending.get(message.request);
        if (!entry) break;
        pluginPending.delete(message.request);
        const tab=pluginTabs.find(tab=>tab.id===entry.tab);
        if (tab && !tab.session_id && message.session_id) {
          const bound={...tab,session_id:message.session_id};
          pluginTabs=pluginTabs.map(item=>item.id===bound.id ? bound : item);
          if (state.active===entry.tab) panels.switchTab(bound);
        }
        if (message.error) entry.reject(Error(message.error)); else entry.resolve(message.result);
        break;
      }
      case "session_sidebar":
        setSessionSidebarHtml(message.html);
        break;
      case "transcript":
        state.loadingEarlier = false;
        setTranscript(message.html, false);
        break;
      case "transcript_prepend":
        prependTranscript(message.html);
        break;
      case "tool_card":
        replaceToolCard(message.first, message.html);
        break;
      case "tool_card_start":
        startToolCard(message.first, message.html, message.assistant);
        break;
      case "block_append":
        appendBlock(message.index, message.html);
        break;
      case "streaming_body":
        updateStreamingBody(message.index, message.html);
        break;
      case "block_replace":
        replaceBlock(message.index, message.html);
        break;
      case "status":
        setStatus(message.html);
        break;
      case "error":
        state.loadingEarlier = false;
        closePendingPreview();
        if (isUnauthorized(message.message)) {
          handleUnauthorized();
        } else {
          showAlert(message.message, "error", message.key || null);
        }
        break;
      case "retry_wait":
        showRetryWait(message.key, message.prefix, message.attempt, message.delay_secs);
        break;
      case "notice":
        toast(message.message);
        break;
      case "file_tree":
        const tree = document.getElementById("file-tree");
        if (tree) tree.outerHTML = message.html;
        break;
      case "workspace_browser": {
        document.getElementById("workspace-browser")?.remove();
        if (message.html) el.app.insertAdjacentHTML("beforeend", message.html);
        break;
      }
      case "workspace_picker":
        document.getElementById("session-workspace-picker")?.remove();
        if (message.html) el.app.insertAdjacentHTML("beforeend", message.html);
        break;
      case "close_workspace_browser":
        document.getElementById("workspace-browser")?.remove();
        document.getElementById("session-workspace-picker")?.remove();
        break;
      case "tools": {
        const panel = document.getElementById("sidebar-tools");
        if (panel) panel.innerHTML = message.html;
        break;
      }
      case "file_preview":
        el.filePreview.innerHTML = message.html;
        el.filePreview.classList.remove("hidden");
        break;
      case "download_ready": {
        if (typeof message.url !== "string" || !message.url.startsWith("/download/")) break;
        void startDownload(message.url);
        break;
      }
      case "preview_ready": {
        if (typeof message.url !== "string" || !message.url.startsWith("/preview/")) break;
        const pending = state.previewWindow;
        state.previewWindow = null;
        if (pending && !pending.closed) {
          pending.location.replace(message.url);
          break;
        }
        const opened = window.open(message.url, "_blank");
        if (!opened) {
          showAlert("Allow popups for this site to preview HTML files", "error");
          break;
        }
        // The new window starts as a same-origin blank page; drop the opener
        // before it navigates to the sandboxed preview document.
        opened.opener = null;
        break;
      }
      case "close_preview":
        el.filePreview.classList.add("hidden");
        el.filePreview.textContent = "";
        break;
      case "workflow_zoom": {
        const keep =
          !el.workflowZoom.classList.contains("hidden") &&
          state.workflowZoom.runId === message.run_id;
        if (!keep) state.workflowZoom = { runId: message.run_id, scale: 1, x: 0, y: 0 };
        zoomDrag = null;
        el.workflowZoom.innerHTML = message.html;
        el.workflowZoom.classList.remove("hidden");
        if (keep) applyWorkflowZoom();
        else fitWorkflowZoom();
        break;
      }
      case "close_workflow_zoom":
        hideWorkflowZoom();
        break;
      default:
        break;
    }
  }

  function isUnauthorized(message) {
    return String(message || "").trim().toLowerCase() === "unauthorized";
  }

  function handleUnauthorized() {
    state.token = "";
    forgetToken();
    setOffline(false);
    if (state.ws) state.ws.close();
    showLogin("That token was not accepted.");
  }

  // -- workflow graph zoom panel ------------------------------------------

  const ZOOM_MIN = 0.1;
  const ZOOM_MAX = 4;
  const ZOOM_STEP = 1.25;
  let zoomDrag = null;

  function workflowZoomParts() {
    return {
      viewport: el.workflowZoom.querySelector("[data-zoom-viewport]"),
      stage: el.workflowZoom.querySelector("[data-zoom-stage]"),
      level: el.workflowZoom.querySelector("[data-zoom-level]"),
    };
  }

  function workflowZoomSize(stage) {
    const previous = stage.style.transform;
    stage.style.transform = "none";
    const size = stage.getBoundingClientRect();
    stage.style.transform = previous;
    return size;
  }

  function applyWorkflowZoom() {
    const { stage, level } = workflowZoomParts();
    if (!stage) return;
    const { scale, x, y } = state.workflowZoom;
    stage.style.transform = `translate(${x}px, ${y}px) scale(${scale})`;
    if (level) level.textContent = `${Math.round(scale * 100)}%`;
  }

  function fitWorkflowZoom() {
    const { viewport, stage } = workflowZoomParts();
    if (!viewport || !stage) return;
    const view = viewport.getBoundingClientRect();
    if (!view.width || !view.height) return;
    const natural = workflowZoomSize(stage);
    if (!natural.width || !natural.height) return;
    const padding = 24;
    const fit = Math.min(
      1,
      (view.width - padding) / natural.width,
      (view.height - padding) / natural.height,
    );
    const scale = Math.max(ZOOM_MIN, Math.round(fit * 100) / 100);
    state.workflowZoom.scale = scale;
    state.workflowZoom.x = (view.width - natural.width * scale) / 2;
    state.workflowZoom.y = (view.height - natural.height * scale) / 2;
    applyWorkflowZoom();
  }

  function resetWorkflowZoom() {
    const { viewport, stage } = workflowZoomParts();
    if (!viewport || !stage) return;
    const view = viewport.getBoundingClientRect();
    const natural = workflowZoomSize(stage);
    state.workflowZoom.scale = 1;
    state.workflowZoom.x = Math.max(12, (view.width - natural.width) / 2);
    state.workflowZoom.y = Math.max(12, (view.height - natural.height) / 2);
    applyWorkflowZoom();
  }

  function zoomWorkflowBy(factor, originX, originY) {
    const { viewport, stage } = workflowZoomParts();
    if (!viewport || !stage) return;
    const view = viewport.getBoundingClientRect();
    const px = originX ?? view.width / 2;
    const py = originY ?? view.height / 2;
    const before = state.workflowZoom.scale;
    const after = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, before * factor));
    if (after === before) return;
    const k = after / before;
    state.workflowZoom.x = px - k * (px - state.workflowZoom.x);
    state.workflowZoom.y = py - k * (py - state.workflowZoom.y);
    state.workflowZoom.scale = after;
    applyWorkflowZoom();
  }

  function hideWorkflowZoom() {
    el.workflowZoom.classList.add("hidden");
    el.workflowZoom.textContent = "";
    state.workflowZoom = { runId: null, scale: 1, x: 0, y: 0 };
  }

  function setBusy(busy) {
    state.busy = busy;
    updateComposer();
  }

  // -- transcript ---------------------------------------------------------

  function nearBottom() {
    const node = el.transcript;
    return node.scrollHeight - node.scrollTop - node.clientHeight < 96;
  }

  function scrollToBottom() {
    el.transcript.scrollTop = el.transcript.scrollHeight;
  }

  // Stick-to-bottom: follow new content until the user scrolls away, and
  // resume when they return. Only real scroll events move the flag, so
  // content that grows without a scroll (tool output, late images) never
  // unsticks; the next update re-anchors to the bottom instead. The live
  // offset is also recorded per tab so switching back restores it.
  el.transcript.addEventListener("scroll", () => {
    const wasStuck = state.stuck;
    state.stuck = nearBottom();
    if (state.transcriptTab != null) {
      state.scrollTops.set(state.transcriptTab, el.transcript.scrollTop);
    }
    if (wasStuck !== state.stuck) syncFollow();
    maybeLoadEarlier();
  });

  function setTranscript(html, forceScroll) {
    const switched = state.transcriptTab !== state.active;
    const previousTop = el.transcript.scrollTop;
    if (!switched && state.transcriptTab != null) {
      state.scrollTops.set(state.transcriptTab, el.transcript.scrollTop);
    }
    state.transcriptTab = state.active;
    el.transcript.innerHTML = html;
    if (switched && !forceScroll && state.scrollTops.has(state.active)) {
      // Returning to a visited tab: put the viewport back where it was.
      el.transcript.scrollTop = state.scrollTops.get(state.active);
      state.stuck = nearBottom();
    } else {
      if (forceScroll || switched) state.stuck = true;
      if (!forceScroll && !switched) {
        // Same-tab refresh: keep the reader's approximate position.
        el.transcript.scrollTop = previousTop;
        state.stuck = nearBottom();
      }
      if (state.stuck) scrollToBottom();
    }
    syncFollow();
    maybeLoadEarlier();
  }

  // Auto-load the previous page when the top sentinel comes into view. The
  // server caps each request, so repeated calls while the transcript is
  // shorter than the viewport fill the page a step at a time.
  function maybeLoadEarlier() {
    if (!state.ready || state.loadingEarlier) return;
    if (state.transcriptTab !== state.active) return;
    if (el.transcript.scrollTop > 80) return;
    if (!el.transcript.querySelector("[data-load-earlier]")) return;
    state.loadingEarlier = true;
    if (!send({ op: "load_earlier", tab: state.active })) {
      state.loadingEarlier = false;
    }
  }

  // Tell the server whether new output should evict old rendered blocks.
  function syncFollow() {
    if (!state.connected || !state.ready) return;
    if (state.transcriptTab !== state.active) return;
    const following = state.stuck;
    if (state.follow.get(state.active) === following) return;
    state.follow.set(state.active, following);
    if (!send({ op: "set_follow", tab: state.active, follow: following })) {
      state.follow.delete(state.active);
    }
  }

  // Insert a rendered older page above the transcript without moving the
  // content the reader is looking at.
  function prependTranscript(html) {
    state.loadingEarlier = false;
    if (state.transcriptTab !== state.active) {
      requestRefresh();
      return;
    }
    const sentinel = el.transcript.querySelector(".load-earlier");
    if (!sentinel) {
      requestRefresh();
      return;
    }
    const beforeHeight = el.transcript.scrollHeight;
    const beforeTop = el.transcript.scrollTop;
    sentinel.remove();
    const template = document.createElement("template");
    template.innerHTML = html;
    el.transcript.insertBefore(template.content, el.transcript.firstChild);
    el.transcript.scrollTop = beforeTop + (el.transcript.scrollHeight - beforeHeight);
    if (state.transcriptTab != null) {
      state.scrollTops.set(state.transcriptTab, el.transcript.scrollTop);
    }
    state.stuck = nearBottom();
    state.follow.delete(state.active);
    syncFollow();
    maybeLoadEarlier();
  }

  // Replace one streaming block's rendered Markdown body in place: keeping
  // the `.block` node avoids re-triggering its entry animation or moving the
  // scroll position.
  function updateStreamingBody(index, html) {
    const block = el.transcript.querySelector(
      `.block[data-block="${Number(index)}"]`,
    );
    if (!block) {
      requestRefresh();
      return;
    }
    const body = fromHtml(html);
    if (!body) {
      requestRefresh();
      return;
    }
    block.replaceChildren(body);
    if (state.stuck) scrollToBottom();
  }

  // Replace one rendered block in place (e.g. a steered message settling
  // out of its queued state): swapping the node keeps the surrounding
  // transcript and scroll position intact.
  function replaceBlock(index, html) {
    const block = el.transcript.querySelector(
      `.block[data-block="${Number(index)}"]`,
    );
    if (!block) {
      requestRefresh();
      return;
    }
    const node = fromHtml(html);
    if (!node) {
      requestRefresh();
      return;
    }
    block.replaceWith(node);
  }

  function setStatus(html) {
    el.status.innerHTML = html;
  }

  // Swap one card's contents in place: keeping the `.tool-card` node avoids
  // re-triggering its entry animation or moving the scroll position.
  function replaceToolCard(first, html) {
    const card = el.transcript.querySelector(
      `.tool-card[data-card="${Number(first)}"]`,
    );
    if (!card) {
      requestRefresh();
      return;
    }
    const template = document.createElement("template");
    template.innerHTML = html;
    const fresh = template.content.firstElementChild;
    if (!fresh) {
      requestRefresh();
      return;
    }
    card.replaceChildren(...fresh.childNodes);
    if (state.stuck) scrollToBottom();
  }

  // Append one rendered block (user message, streaming assistant block,
  // error note) at the end of the transcript.
  function appendBlock(index, html) {
    // A block that is already there means the DOM has drifted.
    if (el.transcript.querySelector(`.block[data-block="${Number(index)}"]`)) {
      requestRefresh();
      return;
    }
    const node = fromHtml(html);
    if (!node) {
      requestRefresh();
      return;
    }
    el.transcript.querySelector(".empty")?.remove();
    node.classList.add("enter");
    el.transcript.appendChild(node);
    if (state.stuck) scrollToBottom();
  }

  // Start of a new tool run: convert the preceding assistant block out of
  // its streaming form, then append the card at the end of the transcript.
  function startToolCard(first, html, assistant) {
    if (assistant) {
      const [index, bodyHtml] = assistant;
      const block = el.transcript.querySelector(
        `.block[data-block="${Number(index)}"]`,
      );
      const body = fromHtml(bodyHtml);
      if (!block || !body) {
        requestRefresh();
        return;
      }
      // Swap only the body: keeping the `.block` node avoids re-triggering
      // its entry animation.
      block.replaceChildren(body);
    }
    const card = fromHtml(html);
    if (!card) {
      requestRefresh();
      return;
    }
    el.transcript.querySelector(".empty")?.remove();
    card.classList.add("enter");
    el.transcript.appendChild(card);
    if (state.stuck) scrollToBottom();
  }

  // Parse trusted server-rendered HTML into the first element.
  function fromHtml(html) {
    const template = document.createElement("template");
    template.innerHTML = html;
    return template.content.firstElementChild;
  }

  // The streaming block can be missing if a delta raced a tab switch.
  function requestRefresh() {
    if (state.refreshTimer) return;
    state.refreshTimer = setTimeout(() => {
      state.refreshTimer = null;
      send({ op: "refresh" });
    }, 150);
  }

  // -- composer -----------------------------------------------------------

  function updateComposer() {
    el.send.disabled = !state.connected || !state.ready;
    el.interrupt.classList.toggle("hidden", !state.busy);
    el.interrupt.disabled = !state.connected;
    el.prompt.classList.toggle("locked", false);
  }

  function autosize() {
    el.prompt.style.height = "auto";
    el.prompt.style.height = `${Math.min(el.prompt.scrollHeight, 220)}px`;
  }

  function submitPrompt() {
    const text = el.prompt.value.trim();
    if (!text) return;
    if (!state.ready) {
      showAlert("Still loading the session; try again in a moment");
      return;
    }
    if (!send({ op: "ask", tab: state.active, prompt: text })) return;
    el.prompt.value = "";
    autosize();
    el.prompt.focus();
  }

  // -- events -------------------------------------------------------------

  el.composer.addEventListener("submit", (event) => {
    event.preventDefault();
    submitPrompt();
  });

  el.prompt.addEventListener("input", autosize);

  el.prompt.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      submitPrompt();
    } else if (event.key === "Escape" && state.busy) {
      event.preventDefault();
      send({ op: "interrupt", tab: state.active });
    }
  });

  document.addEventListener("keydown", (event) => {
    // Escape inside a question form must not interrupt the ask that is
    // waiting for it.
    if (event.key === "Escape" && event.target.closest(".question-form")) return;
    if (event.key === "Escape" && !el.workflowZoom.classList.contains("hidden")) {
      send({ op: "close_workflow_zoom" });
    } else if (event.key === "Escape" && !el.filePreview.classList.contains("hidden")) {
      send({ op: "close_preview" });
    } else if (event.key === "Escape" && state.busy && el.login.classList.contains("hidden")) {
      send({ op: "interrupt", tab: state.active });
    } else if (
      (event.ctrlKey || event.metaKey) &&
      !event.altKey &&
      !event.shiftKey &&
      event.key.toLowerCase() === "b" &&
      el.login.classList.contains("hidden")
    ) {
      event.preventDefault();
      toggleSidebar();
    }
  });

  el.sessionsExpand.addEventListener("click", toggleSessions);
  el.filesExpand.addEventListener("click", toggleSidebar);
  el.status.addEventListener("mousedown", (event) => {
    if (event.target.closest("[data-mode]")) event.preventDefault();
  });
  el.status.addEventListener("click", (event) => {
    const button = event.target.closest("[data-mode]");
    if (button && !state.busy) send({ op: "switch_agent", tab: state.active, agent: button.dataset.mode });
    if (!event.target.closest("[data-model-picker]") && !event.target.closest("[data-effort-picker]")) el.prompt.focus();
  });
  el.status.addEventListener("change", (event) => {
    const picker = event.target.closest("[data-model-picker]");
    if (picker && !state.busy) send({ op: "switch_model", tab: Number(picker.dataset.tab), model: picker.value });
    const effort = event.target.closest("[data-effort-picker]");
    if (effort && !state.busy) send({
      op: "switch_reasoning_effort",
      tab: Number(effort.dataset.tab),
      effort: effort.value === "default" ? null : effort.value,
    });
  });

  el.interrupt.addEventListener("click", () => {
    send({ op: "interrupt", tab: state.active });
  });

  el.retry.addEventListener("click", () => {
    clearReconnect();
    connect();
  });

  el.transcript.addEventListener("click", (event) => {
    const pluginResult=event.target.closest('[data-plugin-result]');
    if (pluginResult) { panels.agentResult(pluginResult.dataset.pluginResult); return; }
    const loadEarlier = event.target.closest("[data-load-earlier]");
    if (loadEarlier) {
      if (state.loadingEarlier) return;
      state.loadingEarlier = true;
      if (!send({ op: "load_earlier", tab: state.active })) {
        state.loadingEarlier = false;
      }
      return;
    }
    const revert = event.target.closest("[data-revert-block]");
    if (revert) {
      closeMessageMenus();
      if (!state.busy) {
        send({ op: "revert_chat", tab: state.active, block: Number(revert.dataset.revertBlock), undo: false });
      }
      return;
    }
    const trigger = event.target.closest(".message-menu-trigger");
    if (trigger) {
      const open = trigger.getAttribute("aria-expanded") !== "true";
      closeMessageMenus();
      if (open) {
        trigger.setAttribute("aria-expanded", "true");
        const menu = document.getElementById(trigger.getAttribute("aria-controls"));
        if (menu) {
          menu.hidden = false;
          const item = menu.querySelector("button");
          item.disabled = state.busy;
          item.focus();
        }
      }
      return;
    }
    if (!event.target.closest(".message-menu")) closeMessageMenus();
    // Tool images sit inside a click-to-toggle card; clicking the picture
    // itself should not collapse it (right-click still saves the image).
    if (event.target.closest(".tool-image")) return;
    const qtab = event.target.closest("[data-qtab]");
    if (qtab) {
      const form = qtab.closest(".question-form");
      if (form) {
        for (const button of form.querySelectorAll("[data-qtab]")) {
          const active = button === qtab;
          button.classList.toggle("active", active);
          button.setAttribute("aria-selected", String(active));
        }
        for (const panel of form.querySelectorAll("[data-qpanel]")) {
          const active = panel.dataset.qpanel === qtab.dataset.qtab;
          panel.classList.toggle("active", active);
          panel.hidden = !active;
        }
      }
      return;
    }
    const dismiss = event.target.closest("[data-question-dismiss]");
    if (dismiss) {
      const form = dismiss.closest(".question-form");
      if (form) {
        const controls = Array.from(form.querySelectorAll("button, input"));
        for (const control of controls) control.disabled = true;
        // A rejected dismissal (or a failed send) leaves the form in place;
        // re-enable it instead of stranding a disabled card.
        setTimeout(() => {
          if (!form.isConnected) return;
          for (const control of controls) control.disabled = false;
        }, 4000);
        send({
          op: "reject_question",
          tab: Number(form.dataset.tab),
          request_id: form.dataset.request,
        });
      }
      return;
    }
    const button = event.target.closest("[data-action]");
    if (!button) return;
    const tab = Number(button.dataset.tab);
    const block = Number(button.dataset.block);
    if (!Number.isInteger(tab) || !Number.isInteger(block)) return;
    if (button.dataset.action === "toggle_tool") {
      send({ op: "toggle_tool", tab, block });
    } else if (button.dataset.action === "toggle_group") {
      send({ op: "toggle_group", tab, block });
    } else if (button.dataset.action === "toggle_thought") {
      send({ op: "toggle_thought", tab, block });
    } else if (button.dataset.action === "zoom_workflow") {
      send({ op: "zoom_workflow", tab, block });
    }
  });

  // One answer list per question, in tab order: selected labels plus an
  // optional typed answer. A single-select question with a typed answer
  // uses only that answer.
  function questionAnswers(form) {
    return Array.from(form.querySelectorAll("[data-qpanel]")).map((panel) => {
      const selected = Array.from(
        panel.querySelectorAll("input[type=radio]:checked, input[type=checkbox]:checked"),
      ).map((input) => input.value);
      const custom = panel.querySelector("[data-custom]");
      const typed = custom ? custom.value.trim() : "";
      if (!typed) return selected;
      const multiple = !!panel.querySelector("input[type=checkbox]");
      if (!multiple) return [typed];
      return selected.includes(typed) ? selected : selected.concat(typed);
    });
  }

  el.transcript.addEventListener("submit", (event) => {
    const form = event.target.closest(".question-form");
    if (!form) return;
    event.preventDefault();
    const submit = form.querySelector(".question-submit");
    if (submit) {
      submit.disabled = true;
      setTimeout(() => {
        if (submit.isConnected) submit.disabled = false;
      }, 4000);
    }
    send({
      op: "answer_question",
      tab: Number(form.dataset.tab),
      request_id: form.dataset.request,
      answers: questionAnswers(form),
    });
  });

  // Typing a custom answer replaces a single-select radio choice, and
  // choosing a radio clears the typed answer, so the two never conflict.
  el.transcript.addEventListener("input", (event) => {
    const custom = event.target.closest("[data-custom]");
    if (!custom) return;
    const panel = custom.closest("[data-qpanel]");
    if (!panel) return;
    for (const radio of panel.querySelectorAll("input[type=radio]")) radio.checked = false;
  });

  el.transcript.addEventListener("change", (event) => {
    const radio = event.target.closest('input[type="radio"]');
    if (!radio) return;
    const panel = radio.closest("[data-qpanel]");
    const custom = panel && panel.querySelector("[data-custom]");
    if (custom) custom.value = "";
  });

  function closeMessageMenus(restoreFocus = false) {
    for (const trigger of el.transcript.querySelectorAll('.message-menu-trigger[aria-expanded="true"]')) {
      trigger.setAttribute("aria-expanded", "false");
      const menu = document.getElementById(trigger.getAttribute("aria-controls"));
      if (menu) menu.hidden = true;
      if (restoreFocus) trigger.focus();
    }
  }

  function closeWorkspaceMenus(restoreFocus = false) {
    for (const trigger of el.sessionSidebar.querySelectorAll('.session-workspace-menu-trigger[aria-expanded="true"]')) {
      trigger.setAttribute("aria-expanded", "false");
      const menu = document.getElementById(trigger.getAttribute("aria-controls"));
      if (menu) menu.hidden = true;
      if (restoreFocus) trigger.focus();
    }
  }

  function openFileMenu(entry, x, y) {
    state.fileMenu = { path: entry.dataset.path, entry };
    const preview = el.fileMenu.querySelector("[data-file-preview]");
    if (preview) preview.hidden = !isHtmlEntry(entry);
    el.fileMenu.hidden = false;
    el.fileMenu.style.left = "0px";
    el.fileMenu.style.top = "0px";
    const rect = el.fileMenu.getBoundingClientRect();
    const left = Math.max(8, Math.min(x, window.innerWidth - rect.width - 8));
    const top = Math.max(8, Math.min(y, window.innerHeight - rect.height - 8));
    el.fileMenu.style.left = `${left}px`;
    el.fileMenu.style.top = `${top}px`;
    const item = el.fileMenu.querySelector("button:not([hidden])");
    if (item) item.focus({ preventScroll: true });
  }

  function isHtmlEntry(entry) {
    return entry.classList.contains("tree-file") && /\.html?$/i.test(entry.dataset.path || "");
  }

  // The preview window is opened inside the click gesture because popup
  // blockers reject opens that follow an async round trip. Navigate it once
  // the ticket URL arrives; close it if the request fails or the socket drops.
  function closePendingPreview() {
    const win = state.previewWindow;
    state.previewWindow = null;
    if (win && !win.closed) win.close();
  }

  function closeFileMenu(restoreFocus = false) {
    if (el.fileMenu.hidden) return;
    el.fileMenu.hidden = true;
    const entry = state.fileMenu.entry;
    state.fileMenu = { path: null, entry: null };
    if (restoreFocus && entry) entry.focus({ preventScroll: true });
  }

  async function startDownload(url) {
    try {
      const head = await fetch(url, { method: "HEAD", credentials: "same-origin" });
      if (!head.ok) {
        const message = await head.text();
        showAlert(message || `Download failed (${head.status})`, "error");
        return;
      }
    } catch (error) {
      showAlert(`Download failed: ${error}`, "error");
      return;
    }
    const link = document.createElement("a");
    link.href = url;
    link.download = "";
    link.rel = "noopener";
    link.hidden = true;
    document.body.appendChild(link);
    link.click();
    link.remove();
  }

  document.addEventListener("click", (event) => {
    if (!event.target.closest(".block.user")) closeMessageMenus();
    if (!event.target.closest(".session-workspace-menu") && !event.target.closest("[data-session-workspace-menu]")) {
      closeWorkspaceMenus();
    }
    if (!event.target.closest("#file-menu")) closeFileMenu();
  });
  document.addEventListener("contextmenu", (event) => {
    if (event.target.closest("#file-menu")) return;
    if (event.target.closest("[data-path].tree-file, [data-path].tree-dir")) return;
    closeFileMenu();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !el.fileMenu.hidden) {
      closeFileMenu(true);
      event.preventDefault();
      event.stopImmediatePropagation();
      return;
    }
    if (event.key === "Escape" && el.transcript.querySelector('.message-menu-trigger[aria-expanded="true"]')) {
      closeMessageMenus(true);
      event.preventDefault();
      event.stopImmediatePropagation();
    }
    if (event.key === "Escape" && el.sessionSidebar.querySelector('.session-workspace-menu-trigger[aria-expanded="true"]')) {
      closeWorkspaceMenus(true);
      event.preventDefault();
      event.stopImmediatePropagation();
    }
  }, true);

  el.sidebar.addEventListener("click", (event) => {
    if (event.target.closest("[data-sidebar-collapse]")) {
      setSidebarOpen(false, true);
      return;
    }
    const sidebarTab = event.target.closest("[data-sidebar-tab]");
    if (sidebarTab) {
      setSidebarTab(sidebarTab.dataset.sidebarTab);
      return;
    }
    const dir = event.target.closest("[data-path].tree-dir");
    if (dir) { send({ op: "toggle_dir", path: dir.dataset.path }); return; }
    const file = event.target.closest("[data-path].tree-file");
    if (file) { send({ op: "read_file", path: file.dataset.path }); return; }
    if (event.target.closest("[data-tree-upload]")) {
      el.fileUpload.click();
      return;
    }
    if (event.target.closest("[data-tree-refresh]")) { send({ op: "refresh_tree" }); }
  });

  el.sidebar.addEventListener("contextmenu", (event) => {
    const entry = event.target.closest("[data-path].tree-file, [data-path].tree-dir");
    if (!entry) return;
    event.preventDefault();
    openFileMenu(entry, event.clientX, event.clientY);
  });

  el.sidebar.addEventListener("keydown", (event) => {
    if (event.key !== "ContextMenu" && !(event.shiftKey && event.key === "F10")) return;
    const entry = event.target.closest("[data-path].tree-file, [data-path].tree-dir");
    if (!entry) return;
    event.preventDefault();
    const rect = entry.getBoundingClientRect();
    openFileMenu(entry, rect.left + 16, rect.bottom);
  });

  el.fileMenu.addEventListener("click", (event) => {
    if (event.target.closest('[data-file-edit]')) {
      const path = state.fileMenu.path;
      closeFileMenu();
      if (path) panels.editPath(path);
      return;
    }
    if (event.target.closest("[data-file-preview]")) {
      const path = state.fileMenu.path;
      closeFileMenu();
      if (!path) return;
      const win = window.open("", "_blank");
      if (win) win.opener = null;
      state.previewWindow = win;
      send({ op: "preview_file", path });
      return;
    }
    if (!event.target.closest("[data-file-download]")) return;
    const path = state.fileMenu.path;
    closeFileMenu();
    if (path) send({ op: "download", path });
  });

  window.addEventListener("resize", () => closeFileMenu());
  el.sidebar.addEventListener("scroll", () => closeFileMenu(), true);

  el.sessionSidebar.addEventListener("click", (event) => {
    if (event.target.closest("[data-sessions-collapse]")) {
      setSessionsOpen(false, true);
      return;
    }
    if (event.target.closest("[data-session-new]")) {
      send({ op: "open_tab" });
      return;
    }
    const workspaceMenu = event.target.closest("[data-session-workspace-menu]");
    if (workspaceMenu) {
      const open = workspaceMenu.getAttribute("aria-expanded") !== "true";
      closeWorkspaceMenus();
      if (open) {
        workspaceMenu.setAttribute("aria-expanded", "true");
        const menu = document.getElementById(workspaceMenu.getAttribute("aria-controls"));
        if (menu) {
          menu.hidden = false;
          const item = menu.querySelector("button");
          if (item) item.focus();
        }
      }
      return;
    }
    const close = event.target.closest("[data-session-close]");
    if (close) {
      event.stopPropagation();
      send({ op: "close_tab", tab: Number(close.dataset.sessionClose) });
      return;
    }
    const closeWorkspace = event.target.closest("[data-session-workspace-close]");
    if (closeWorkspace) {
      event.stopPropagation();
      closeWorkspaceMenus();
      send({ op: "close_workspace", workspace: closeWorkspace.dataset.sessionWorkspaceClose });
      return;
    }
    const workspaceToggle = event.target.closest("[data-session-workspace-toggle]");
    if (workspaceToggle) {
      toggleWorkspaceGroup(workspaceToggle.dataset.sessionWorkspaceToggle);
      return;
    }
    const create = event.target.closest("[data-session-new-workspace]");
    if (create) {
      expandWorkspace(create.dataset.sessionNewWorkspace);
      send({
        op: "create_tab",
        workspace: create.dataset.sessionNewWorkspace,
        path: create.dataset.sessionNewPath,
      });
      if (narrowScreen()) setSessionsOpen(false, false);
      return;
    }
    const tab = event.target.closest("[data-session-tab]");
    if (tab) {
      const id = Number(tab.dataset.sessionTab);
      if (id !== state.active) send({ op: "switch_tab", tab: id });
      if (narrowScreen()) setSessionsOpen(false, false);
    }
  });

  el.app.addEventListener("click", (event) => {
    const closeNewSession = event.target.closest("[data-new-session-close]");
    if (closeNewSession) {
      send({ op: "close_workspace_browser" });
      return;
    }
    const createTab = event.target.closest("[data-new-session-create]");
    if (createTab) {
      expandWorkspace(createTab.dataset.newSessionWorkspace);
      send({
        op: "create_tab",
        workspace: createTab.dataset.newSessionWorkspace,
        path: createTab.dataset.newSessionPath,
      });
      return;
    }
    const browseNewSession = event.target.closest("[data-new-session-browse-path]");
    if (browseNewSession) {
      send({ op: "browse_new_session", path: browseNewSession.dataset.newSessionBrowsePath });
      return;
    }
    const close = event.target.closest("[data-workspace-browser-close]");
    if (close) {
      send({ op: "close_workspace_browser" });
      return;
    }
    const add = event.target.closest("[data-workspace-add-path]");
    if (add) {
      send({ op: "add_workspace", tab: state.active, path: add.dataset.workspaceAddPath });
      return;
    }
    const browse = event.target.closest("[data-workspace-browse-path]");
    if (browse) {
      send({ op: "browse_workspace", path: browse.dataset.workspaceBrowsePath });
    }
  });

  // -- file upload ---------------------------------------------------------

  // Uploads always land in the working-directory root; the server sanitizes
  // names and enforces the size cap.
  const MAX_UPLOAD_BYTES = 50 * 1024 * 1024;

  function bytesToBase64(buffer) {
    const bytes = new Uint8Array(buffer);
    let binary = "";
    const chunk = 0x8000;
    for (let offset = 0; offset < bytes.length; offset += chunk) {
      binary += String.fromCharCode(...bytes.subarray(offset, offset + chunk));
    }
    return btoa(binary);
  }

  async function uploadFiles(fileList) {
    const files = [...fileList].filter((file) => file.size <= MAX_UPLOAD_BYTES);
    const skipped = fileList.length - files.length;
    if (skipped > 0) {
      showAlert(`Skipped ${skipped} file(s) over the 50 MB upload limit`, "warning");
    }
    if (!files.length) return;
    const payloads = await Promise.all(
      files.map(async (file) => ({
        name: file.name,
        content_base64: bytesToBase64(await file.arrayBuffer()),
      })),
    );
    send({ op: "upload_files", files: payloads });
  }

  el.fileUpload = document.getElementById("file-upload");
  el.fileUpload.addEventListener("change", () => {
    if (el.fileUpload.files.length) uploadFiles(el.fileUpload.files);
    el.fileUpload.value = "";
  });

  const fileTreeHost = document.getElementById("app");
  fileTreeHost.addEventListener("dragover", (event) => {
    if (!event.dataTransfer?.types?.includes("Files")) return;
    event.preventDefault();
    fileTreeHost.classList.add("drag-over");
  });
  fileTreeHost.addEventListener("dragleave", (event) => {
    if (event.target === fileTreeHost) fileTreeHost.classList.remove("drag-over");
  });
  fileTreeHost.addEventListener("drop", (event) => {
    fileTreeHost.classList.remove("drag-over");
    if (!event.dataTransfer?.files?.length) return;
    event.preventDefault();
    if (!state.ready) {
      showAlert("Still loading the session; try again in a moment");
      return;
    }
    uploadFiles(event.dataTransfer.files);
  });

  function setSidebarTab(name) {
    const tabs = el.sidebar.querySelectorAll("[data-sidebar-tab]");
    const panels = el.sidebar.querySelectorAll(".sidebar-panel");
    for (const tab of tabs) {
      const active = tab.dataset.sidebarTab === name;
      tab.classList.toggle("active", active);
      tab.setAttribute("aria-selected", String(active));
    }
    for (const panel of panels) {
      const active = panel.id === `sidebar-${name}`;
      panel.classList.toggle("active", active);
      panel.hidden = !active;
    }
  }

  el.filePreview.addEventListener("click", (event) => {
    if (event.target.closest("[data-preview-close]")) send({ op: "close_preview" });
  });

  el.workflowZoom.addEventListener("click", (event) => {
    if (event.target.closest("[data-zoom-close]")) {
      send({ op: "close_workflow_zoom" });
      return;
    }
    const control = event.target.closest("[data-zoom]");
    if (control) {
      const action = control.dataset.zoom;
      if (action === "in") zoomWorkflowBy(ZOOM_STEP);
      else if (action === "out") zoomWorkflowBy(1 / ZOOM_STEP);
      else if (action === "fit") fitWorkflowZoom();
      else if (action === "reset") resetWorkflowZoom();
      return;
    }
    if (event.target === el.workflowZoom) send({ op: "close_workflow_zoom" });
  });

  el.workflowZoom.addEventListener(
    "wheel",
    (event) => {
      const viewport = event.target.closest("[data-zoom-viewport]");
      if (!viewport) return;
      event.preventDefault();
      const rect = viewport.getBoundingClientRect();
      const delta =
        event.deltaMode === 1
          ? event.deltaY * 16
          : event.deltaMode === 2
            ? event.deltaY * rect.height
            : event.deltaY;
      zoomWorkflowBy(
        Math.exp(-delta * 0.0015),
        event.clientX - rect.left,
        event.clientY - rect.top,
      );
    },
    { passive: false },
  );

  el.workflowZoom.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    const viewport = event.target.closest("[data-zoom-viewport]");
    if (!viewport) return;
    zoomDrag = {
      id: event.pointerId,
      x: event.clientX,
      y: event.clientY,
      originX: state.workflowZoom.x,
      originY: state.workflowZoom.y,
    };
    viewport.classList.add("dragging");
    viewport.setPointerCapture(event.pointerId);
  });

  el.workflowZoom.addEventListener("pointermove", (event) => {
    if (!zoomDrag || event.pointerId !== zoomDrag.id) return;
    state.workflowZoom.x = zoomDrag.originX + event.clientX - zoomDrag.x;
    state.workflowZoom.y = zoomDrag.originY + event.clientY - zoomDrag.y;
    applyWorkflowZoom();
  });

  function endZoomDrag(event) {
    if (!zoomDrag || event.pointerId !== zoomDrag.id) return;
    el.workflowZoom.querySelector("[data-zoom-viewport]")?.classList.remove("dragging");
    zoomDrag = null;
  }

  el.workflowZoom.addEventListener("pointerup", endZoomDrag);
  el.workflowZoom.addEventListener("pointercancel", endZoomDrag);

  // -- login --------------------------------------------------------------

  function showLogin(message) {
    el.login.classList.remove("hidden");
    el.loginError.textContent = message || "";
    el.loginError.classList.toggle("hidden", !message);
    el.loginForm.classList.toggle("shake", Boolean(message));
    el.token.focus();
    el.token.select();
  }

  el.loginForm.addEventListener("submit", (event) => {
    event.preventDefault();
    const token = el.token.value.trim();
    if (!token) return;
    state.token = token;
    saveToken(token);
    el.login.classList.add("hidden");
    el.loginError.classList.add("hidden");
    state.attempt = 0;
    connect();
  });

  // -- toasts -------------------------------------------------------------

  function toast(text) {
    const value = String(text || "").trim();
    if (!value) return;
    const now = Date.now();
    if (state.lastToast.text === value && now - state.lastToast.at < 8000) return;
    state.lastToast = { text: value, at: now };

    const node = document.createElement("div");
    node.className = "toast";
    node.textContent = value;
    el.toasts.appendChild(node);
    setTimeout(() => {
      node.classList.add("leaving");
      setTimeout(() => node.remove(), 250);
    }, 5000);
  }

  // -- floating alert box --------------------------------------------------
  //
  // Errors and warnings surface in a dedicated floating box instead of
  // vanishing toasts: errors stay until dismissed, warnings auto-expire.
  // Keyed alerts (e.g. retries of one failure) update a single entry in
  // place instead of stacking.

  function showAlert(text, level = "error", key = null) {
    const value = String(text || "").trim();
    if (!value) return;
    if (key) {
      const existing = el.alerts.querySelector(
        `.alert[data-alert-key="${CSS.escape(key)}"]`,
      );
      if (existing) {
        const label = existing.querySelector(".alert-text");
        if (label.textContent !== value) {
          label.textContent = value;
          // Re-trigger the pulse so an in-place update is noticeable.
          existing.classList.remove("updated");
          void existing.offsetWidth;
          existing.classList.add("updated");
        }
        return;
      }
    } else {
      const now = Date.now();
      if (state.lastAlert.text === value && now - state.lastAlert.at < 8000) return;
      state.lastAlert = { text: value, at: now };
    }

    const entry = document.createElement("div");
    entry.className = `alert ${level}`;
    if (key) entry.dataset.alertKey = key;
    const label = document.createElement("span");
    label.className = "alert-text";
    label.textContent = value;
    const dismiss = document.createElement("button");
    dismiss.type = "button";
    dismiss.className = "alert-dismiss";
    dismiss.setAttribute("aria-label", "Dismiss");
    dismiss.textContent = "×";
    dismiss.addEventListener("click", () => removeAlert(entry));
    entry.append(label, dismiss);
    el.alerts.appendChild(entry);
    refreshAlerts();

    if (level === "warning") {
      setTimeout(() => removeAlert(entry), 10000);
    }
  }

  function removeAlert(entry) {
    if (!entry.isConnected) return;
    entry.classList.add("leaving");
    setTimeout(() => {
      entry.remove();
      refreshAlerts();
    }, 250);
  }

  function refreshAlerts() {
    const entries = el.alerts.querySelectorAll(".alert");
    el.alerts.hidden = entries.length === 0;
    // With several problems at once, offer a one-click clear.
    let head = el.alerts.querySelector(".alerts-head");
    if (entries.length > 1 && !head) {
      head = document.createElement("div");
      head.className = "alerts-head";
      const title = document.createElement("span");
      title.className = "alerts-title";
      title.textContent = "Problems";
      const dismissAll = document.createElement("button");
      dismissAll.type = "button";
      dismissAll.className = "alerts-dismiss-all";
      dismissAll.textContent = "Dismiss all";
      dismissAll.addEventListener("click", () => {
        el.alerts.querySelectorAll(".alert").forEach((entry) => removeAlert(entry));
      });
      head.append(title, dismissAll);
      el.alerts.prepend(head);
    } else if (entries.length <= 1 && head) {
      head.remove();
    }
  }

  // Provider retry waits: a keyed entry with a live countdown that
  // dismisses itself when it reaches zero (the request then restarts).
  function showRetryWait(key, prefix, attempt, delaySecs) {
    if (delaySecs < 1) return; // nothing visible to count down
    let entry = el.alerts.querySelector(
      `.alert[data-alert-key="${CSS.escape(key)}"]`,
    );
    if (!entry) {
      entry = document.createElement("div");
      entry.className = "alert error";
      entry.dataset.alertKey = key;
      const label = document.createElement("span");
      label.className = "alert-text";
      const dismiss = document.createElement("button");
      dismiss.type = "button";
      dismiss.className = "alert-dismiss";
      dismiss.setAttribute("aria-label", "Dismiss");
      dismiss.textContent = "×";
      dismiss.addEventListener("click", () => removeAlert(entry));
      entry.append(label, dismiss);
      el.alerts.appendChild(entry);
    } else {
      // Re-trigger the pulse so the fresh attempt is noticeable.
      entry.classList.remove("updated");
      void entry.offsetWidth;
      entry.classList.add("updated");
    }
    entry.dataset.retryRemaining = String(delaySecs);
    entry.dataset.retryAttempt = String(attempt);
    entry.dataset.retryPrefix = prefix;
    renderRetryWait(entry);
    refreshAlerts();
    startRetryTicker();
  }

  function renderRetryWait(entry) {
    entry.querySelector(".alert-text").textContent =
      `${entry.dataset.retryPrefix} ${entry.dataset.retryRemaining}s` +
      ` (attempt ${entry.dataset.retryAttempt})`;
  }

  let retryTicker = null;
  function startRetryTicker() {
    if (retryTicker) return;
    retryTicker = setInterval(() => {
      for (const entry of el.alerts.querySelectorAll(".alert[data-retry-remaining]")) {
        const remaining = Number(entry.dataset.retryRemaining) - 1;
        if (remaining <= 0) {
          // The wait is over and the request restarts; drop the notice.
          removeAlert(entry);
        } else {
          entry.dataset.retryRemaining = String(remaining);
          renderRetryWait(entry);
        }
      }
      if (!el.alerts.querySelector(".alert[data-retry-remaining]")) {
        clearInterval(retryTicker);
        retryTicker = null;
      }
    }, 1000);
  }

  setSessionsOpen(loadSessionsOpen(), false);
  setSidebarOpen(loadSidebarOpen(), false);
  requestAnimationFrame(() => {
    requestAnimationFrame(() => el.app.classList.remove("preload"));
  });
  boot();
})();
