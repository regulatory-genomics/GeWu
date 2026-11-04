// GeWu web GUI client.
//
// The server renders all transcript HTML; this script only manages tabs,
// applies snapshots/deltas, and forwards user actions. Model-authored text is
// inserted with textContent, never innerHTML.

(() => {
  "use strict";

  const TOKEN_KEY = "gewu-gui-token";
  const MAX_RECONNECT_MS = 8000;

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
    upstream: document.getElementById("upstream"),
  };

  const state = {
    ws: null,
    connected: false,
    ready: false,
    authRequired: false,
    token: loadToken(),
    active: 0,
    transcriptTab: null,
    busy: false,
    attempt: 0,
    reconnectTimer: null,
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

  // -- connection ---------------------------------------------------------

  async function boot() {
    try {
      const response = await fetch("/api/config", { cache: "no-store" });
      const config = await response.json();
      state.authRequired = Boolean(config.auth_required);
      el.upstream.textContent = config.server || "";
      el.upstream.title = config.server ? `Upstream ${config.server}` : "";
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
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = null;

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

  function scheduleReconnect() {
    if (state.reconnectTimer) return;
    const delay = Math.min(MAX_RECONNECT_MS, 500 * 2 ** state.attempt);
    state.attempt += 1;
    el.offlineText.textContent = `Disconnected — reconnecting in ${Math.ceil(delay / 1000)}s…`;
    state.reconnectTimer = setTimeout(() => {
      state.reconnectTimer = null;
      connect();
    }, delay);
  }

  function setOffline(offline) {
    el.offline.classList.toggle("hidden", !offline);
    if (!offline) {
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
        setBusy(message.tabs.some((tab) => tab.id === message.active && tab.busy));
        break;
      case "tabs":
        state.active = message.active;
        renderTabs(message.tabs, message.active);
        setBusy(message.tabs.some((tab) => tab.id === message.active && tab.busy));
        break;
      case "transcript":
        setTranscript(message.html, false);
        break;
      case "text_delta":
        appendDelta(message.block, message.delta);
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

  function setTranscript(html, forceScroll) {
    const switched = state.transcriptTab !== state.active;
    const shouldScroll = forceScroll || switched || nearBottom();
    state.transcriptTab = state.active;
    el.transcript.innerHTML = html;
    if (shouldScroll) scrollToBottom();
  }

  function appendDelta(block, delta) {
    const node = el.transcript.querySelector(
      `.streaming-text[data-block="${block}"]`,
    );
    if (!node) {
      requestRefresh();
      return;
    }
    const shouldScroll = nearBottom();
    node.textContent += delta;
    if (shouldScroll) scrollToBottom();
  }

  function setStatus(html) {
    el.status.innerHTML = html;
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
    el.send.disabled = !state.connected || !state.ready || state.busy;
    el.interrupt.classList.toggle("hidden", !state.busy);
    el.interrupt.disabled = !state.connected;
    el.prompt.classList.toggle("locked", state.busy);
  }

  function autosize() {
    el.prompt.style.height = "auto";
    el.prompt.style.height = `${Math.min(el.prompt.scrollHeight, 220)}px`;
  }

  function submitPrompt() {
    const text = el.prompt.value.trim();
    if (!text) return;
    if (state.busy) {
      toast("Wait for the current reply to finish, or press Esc to interrupt");
      return;
    }
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
    if (event.key === "Escape" && state.busy && el.login.classList.contains("hidden")) {
      send({ op: "interrupt", tab: state.active });
    }
  });

  el.newTab.addEventListener("click", () => send({ op: "open_tab" }));

  el.interrupt.addEventListener("click", () => {
    send({ op: "interrupt", tab: state.active });
  });

  el.retry.addEventListener("click", () => {
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = null;
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
    const button = event.target.closest("[data-action]");
    if (!button) return;
    const tab = Number(button.dataset.tab);
    const block = Number(button.dataset.block);
    if (!Number.isInteger(tab) || !Number.isInteger(block)) return;
    if (button.dataset.action === "toggle_tool") {
      send({ op: "toggle_tool", tab, block });
    } else if (button.dataset.action === "toggle_group") {
      send({ op: "toggle_group", tab, block });
    }
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

  boot();
})();
