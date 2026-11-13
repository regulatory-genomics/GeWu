// GeWu web GUI client.
//
// The server renders all transcript HTML; this script only manages tabs,
// applies snapshots/deltas, and forwards user actions. Model-authored text is
// inserted with textContent, never innerHTML.

(() => {
  "use strict";

  const TOKEN_KEY = "gewu-gui-token";
  const SIDEBAR_KEY = "gewu-gui-sidebar";
  const MAX_RECONNECT_MS = 60000;

  const el = {
    app: document.getElementById("app"),
    offline: document.getElementById("offline"),
    offlineText: document.getElementById("offline-text"),
    retry: document.getElementById("retry"),
    tabs: document.getElementById("tabs"),
    newTab: document.getElementById("new-tab"),
    interrupt: document.getElementById("interrupt"),
    transcript: document.getElementById("transcript"),
    status: document.getElementById("status"),
    composer: document.getElementById("composer"),
    prompt: document.getElementById("prompt"),
    send: document.getElementById("send"),
    toasts: document.getElementById("toasts"),
    login: document.getElementById("login"),
    loginForm: document.getElementById("login-form"),
    loginError: document.getElementById("login-error"),
    token: document.getElementById("token"),
    sidebar: document.getElementById("sidebar"),
    sidebarToggle: document.getElementById("sidebar-toggle"),
    filePreview: document.getElementById("file-preview"),
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

  // Open by default, except on narrow screens where it covers the transcript.
  function loadSidebarOpen() {
    try {
      const saved = localStorage.getItem(SIDEBAR_KEY);
      if (saved === "open") return true;
      if (saved === "closed") return false;
    } catch {
      // Fall through to the default.
    }
    return !window.matchMedia("(max-width: 800px)").matches;
  }

  function setSidebarOpen(open, remember) {
    el.app.classList.toggle("sidebar-collapsed", !open);
    el.sidebarToggle.setAttribute("aria-expanded", String(open));
    if (!remember) return;
    try {
      localStorage.setItem(SIDEBAR_KEY, open ? "open" : "closed");
    } catch {
      // The state still applies to this page.
    }
  }

  function toggleSidebar() {
    setSidebarOpen(el.app.classList.contains("sidebar-collapsed"), true);
  }

  // -- connection ---------------------------------------------------------

  async function boot() {
    try {
      const response = await fetch("/api/config", { cache: "no-store" });
      const config = await response.json();
      state.authRequired = Boolean(config.auth_required);
    } catch (error) {
      toast(`Could not reach gewu-gui: ${error.message}`);
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
      state.attempt = 0;
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
        toast("Received an invalid message from gewu-gui");
        return;
      }
      handle(message);
    });

    ws.addEventListener("close", () => {
      if (state.ws !== ws) return;
      state.ws = null;
      state.connected = false;
      state.ready = false;
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
      toast("Not connected to gewu-gui");
      return false;
    }
    state.ws.send(JSON.stringify(message));
    return true;
  }

  // -- server messages ----------------------------------------------------

  function handle(message) {
    switch (message.type) {
      case "snapshot":
        state.active = message.active;
        state.ready = true;
        state.transcriptTab = message.active;
        renderTabs(message.tabs, message.active);
        setTranscript(message.transcript, true);
        setStatus(message.status);
        el.sidebar.innerHTML = message.sidebar || "";
        setBusy(message.tabs.some((tab) => tab.id === message.active && tab.busy));
        break;
      case "tabs": {
        state.active = message.active;
        const alive = new Set(message.tabs.map((tab) => tab.id));
        for (const id of state.scrollTops.keys()) {
          if (!alive.has(id)) state.scrollTops.delete(id);
        }
        renderTabs(message.tabs, message.active);
        setBusy(message.tabs.some((tab) => tab.id === message.active && tab.busy));
        break;
      }
      case "transcript":
        setTranscript(message.html, false);
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
      case "status":
        setStatus(message.html);
        break;
      case "error":
        if (isUnauthorized(message.message)) {
          handleUnauthorized();
        } else {
          toast(message.message);
        }
        break;
      case "notice":
        toast(message.message);
        break;
      case "file_tree":
        const tree = document.getElementById("file-tree");
        if (tree) tree.outerHTML = message.html;
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
      case "close_preview":
        el.filePreview.classList.add("hidden");
        el.filePreview.textContent = "";
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

  // -- tabs ---------------------------------------------------------------

  function renderTabs(tabs, active) {
    const fragment = document.createDocumentFragment();
    for (const tab of tabs) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "tab";
      button.dataset.tab = String(tab.id);
      if (tab.id === active) button.classList.add("active");
      if (tab.busy) button.classList.add("busy");
      button.title = tab.session_id
        ? `Session ${tab.session_id}`
        : "New session";

      const label = document.createElement("span");
      label.className = "tab-label";
      label.textContent = tab.label;
      button.appendChild(label);

      if (tab.busy) {
        const spinner = document.createElement("span");
        spinner.className = "tab-spinner";
        spinner.setAttribute("aria-label", "working");
        button.appendChild(spinner);
      }

      const close = document.createElement("span");
      close.className = "tab-close";
      close.dataset.close = String(tab.id);
      close.setAttribute("role", "button");
      close.setAttribute("aria-label", `Close ${tab.label}`);
      close.title = "Close and delete this session";
      close.textContent = "×";
      button.appendChild(close);

      fragment.appendChild(button);
    }
    el.tabs.replaceChildren(fragment);
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
    state.stuck = nearBottom();
    if (state.transcriptTab != null) {
      state.scrollTops.set(state.transcriptTab, el.transcript.scrollTop);
    }
  });

  function setTranscript(html, forceScroll) {
    const switched = state.transcriptTab !== state.active;
    if (!switched && state.transcriptTab != null) {
      state.scrollTops.set(state.transcriptTab, el.transcript.scrollTop);
    }
    state.transcriptTab = state.active;
    el.transcript.innerHTML = html;
    if (switched && !forceScroll && state.scrollTops.has(state.active)) {
      // Returning to a visited tab: put the viewport back where it was.
      el.transcript.scrollTop = state.scrollTops.get(state.active);
      state.stuck = nearBottom();
      return;
    }
    if (forceScroll || switched) state.stuck = true;
    if (state.stuck) scrollToBottom();
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
      toast("Still loading the session; try again in a moment");
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
    if (event.key === "Escape" && !el.filePreview.classList.contains("hidden")) {
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

  el.sidebarToggle.addEventListener("click", toggleSidebar);
  el.status.addEventListener("mousedown", (event) => {
    if (event.target.closest("[data-mode]")) event.preventDefault();
  });
  el.status.addEventListener("click", (event) => {
    const button = event.target.closest("[data-mode]");
    if (button && !state.busy) send({ op: "switch_agent", tab: state.active, agent: button.dataset.mode });
    if (!event.target.closest("[data-model-picker]")) el.prompt.focus();
  });
  el.status.addEventListener("change", (event) => {
    const picker = event.target.closest("[data-model-picker]");
    if (picker && !state.busy) send({ op: "switch_model", tab: Number(picker.dataset.tab), model: picker.value });
  });

  el.newTab.addEventListener("click", () => send({ op: "open_tab" }));

  el.interrupt.addEventListener("click", () => {
    send({ op: "interrupt", tab: state.active });
  });

  el.retry.addEventListener("click", () => {
    clearReconnect();
    connect();
  });

  el.tabs.addEventListener("click", (event) => {
    const close = event.target.closest("[data-close]");
    if (close) {
      event.stopPropagation();
      send({ op: "close_tab", tab: Number(close.dataset.close) });
      return;
    }
    const tab = event.target.closest("[data-tab]");
    if (tab) {
      const id = Number(tab.dataset.tab);
      if (id !== state.active) send({ op: "switch_tab", tab: id });
    }
  });

  el.transcript.addEventListener("click", (event) => {
    const revert = event.target.closest("[data-revert-block]");
    if (revert) {
      closeMessageMenus();
      if (!state.busy && window.confirm("Revert chat from this message? File changes will NOT be undone.")) {
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
    }
  });

  function closeMessageMenus(restoreFocus = false) {
    for (const trigger of el.transcript.querySelectorAll('.message-menu-trigger[aria-expanded="true"]')) {
      trigger.setAttribute("aria-expanded", "false");
      const menu = document.getElementById(trigger.getAttribute("aria-controls"));
      if (menu) menu.hidden = true;
      if (restoreFocus) trigger.focus();
    }
  }

  document.addEventListener("click", (event) => {
    if (!event.target.closest(".block.user")) closeMessageMenus();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && el.transcript.querySelector('.message-menu-trigger[aria-expanded="true"]')) {
      closeMessageMenus(true);
      event.preventDefault();
      event.stopImmediatePropagation();
    }
  }, true);

  el.sidebar.addEventListener("click", (event) => {
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
      toast(`Skipped ${skipped} file(s) over the 50 MB upload limit`);
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
      toast("Still loading the session; try again in a moment");
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

  setSidebarOpen(loadSidebarOpen(), false);
  boot();
})();
