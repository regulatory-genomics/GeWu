// State
const state = {
    sidebar: { width: 220, collapsed: false },
    sessions: [],
    currentSession: null,
    currentSessionId: null,
    workGraph: null,
    workTab: 'timeline'
};
// Expose to canvas.js so the inverse-channel POST can find the session id.
window.state = state;

// DOM Elements
const sidebar = document.getElementById('sidebar');
const chatContainer = document.getElementById('chat-container');
const messageInput = document.getElementById('message-input');
const sendBtn = document.getElementById('send-btn');
const sessionList = document.getElementById('session-list');
const errorContainer = document.getElementById('error-container');
const modelNameEl = document.getElementById('model-name');
const endpointNameEl = document.getElementById('endpoint-name');
const statusIndicator = document.getElementById('status-indicator');
const statusText = document.getElementById('status-text');
const toggleSidebarBtn = document.getElementById('toggle-sidebar');
const newSessionBtn = document.getElementById('new-session-btn');
const sidebarHandle = document.getElementById('sidebar-handle');
const agentSelector = document.getElementById('agent-selector');
const traceContainer = document.getElementById('trace-container');
const workTabs = Array.from(document.querySelectorAll('.work-tab'));

let token = '';
let isLoading = false;
let activeChatAbortController = null;
let activeToolCallEls = [];
const activeToolCallsByName = new Map();
const activeToolCallsById = new Map();
let currentMarkdownBuffer = '';
let currentAssistantToolsEl = null;
let recoveryPollTimer = null;

// Configure marked
marked.setOptions({
    breaks: true,
    gfm: true
});

// URL params
const urlParams = new URLSearchParams(window.location.search);
token = urlParams.get('token') || '';
window.token = token;

// Load persisted state
function loadState() {
    try {
        const saved = localStorage.getItem('gewu_state');
        if (saved) {
            const parsed = JSON.parse(saved);
            if (parsed.sidebar) state.sidebar = parsed.sidebar;
            if (parsed.sessions) state.sessions = parsed.sessions;
            if (parsed.currentSessionId) state.currentSessionId = parsed.currentSessionId;
        }
    } catch (e) {
        console.warn('Failed to load persisted UI state', e);
    }
}

// Save state
function saveState() {
    try {
        localStorage.setItem('gewu_state', JSON.stringify({
            sidebar: state.sidebar,
            sessions: state.sessions,
            currentSessionId: state.currentSession?.id || state.currentSessionId || null
        }));
    } catch (e) {
        console.warn('Failed to save persisted UI state', e);
    }
}

// Apply state to UI
function applyState() {
    document.documentElement.style.setProperty('--sidebar-width', state.sidebar.width + 'px');
    if (state.sidebar.collapsed) {
        sidebar.classList.add('collapsed');
    }
    updateHandles();
}

// Update resize handle positions
function updateHandles() {
    const sidebarWidth = state.sidebar.collapsed ? 48 : state.sidebar.width;
    sidebarHandle.style.left = sidebarWidth + 'px';
    sidebarHandle.style.display = state.sidebar.collapsed ? 'none' : 'block';
}

// Resize Handle Logic
let isResizing = false;
let startX = 0;
let startWidth = 0;

function startResize(e) {
    e.preventDefault();
    isResizing = true;
    startX = e.clientX;
    startWidth = state.sidebar.collapsed ? 48 : state.sidebar.width;

    document.body.style.userSelect = 'none';
    document.body.style.overflow = 'hidden';
    sidebarHandle.classList.add('dragging');

    document.addEventListener('mousemove', onResize);
    document.addEventListener('mouseup', stopResize);
}

function onResize(e) {
    if (!isResizing) return;

    const delta = e.clientX - startX;
    let newWidth = startWidth + delta;

    // Collapse threshold
    if (newWidth < 100) {
        state.sidebar.collapsed = true;
        newWidth = 48;
    } else {
        state.sidebar.collapsed = false;
        newWidth = Math.min(Math.max(newWidth, 180), 350);
    }

    state.sidebar.width = newWidth;
    document.documentElement.style.setProperty('--sidebar-width', newWidth + 'px');
    sidebar.classList.toggle('collapsed', state.sidebar.collapsed);
    updateHandles();
}

function stopResize() {
    isResizing = false;
    document.body.style.userSelect = '';
    document.body.style.overflow = '';
    sidebarHandle.classList.remove('dragging');
    document.removeEventListener('mousemove', onResize);
    document.removeEventListener('mouseup', stopResize);
    saveState();
}

// Event Listeners for Resize Handle
sidebarHandle.addEventListener('mousedown', startResize);

// Toggle Sidebar
toggleSidebarBtn.addEventListener('click', () => {
    state.sidebar.collapsed = !state.sidebar.collapsed;
    if (state.sidebar.collapsed) {
        sidebar.classList.add('collapsed');
        sidebarHandle.style.display = 'none';
    } else {
        sidebar.classList.remove('collapsed');
        sidebarHandle.style.display = 'block';
    }
    updateHandles();
    saveState();
});

// API Functions
async function loadServerInfo() {
    try {
        const resp = await fetch(`/api/info?token=${encodeURIComponent(token)}`);
        const data = await resp.json();
        modelNameEl.textContent = data.model || 'unknown';
        endpointNameEl.textContent = data.endpoint || 'unknown';
    } catch (e) {
        modelNameEl.textContent = 'error';
        endpointNameEl.textContent = 'error';
    }
}

async function loadAgents() {
    try {
        const resp = await fetch(`/api/agents?token=${encodeURIComponent(token)}`);
        if (!resp.ok) throw new Error('Failed to load agents');
        const data = await resp.json();
        const agents = (data.agents || []).map(agent => agent.name);
        agentSelector.innerHTML = '';
        agents.forEach(name => {
            const option = document.createElement('option');
            option.value = name;
            option.textContent = name;
            agentSelector.appendChild(option);
        });
        if (!agents.length) {
            const option = document.createElement('option');
            option.value = 'general';
            option.textContent = 'general';
            agentSelector.appendChild(option);
        }
    } catch (e) {
        agentSelector.innerHTML = `
            <option value="general">general</option>
            <option value="build">build</option>
            <option value="explore">explore</option>
        `;
    }
}

function setStatus(text, loading = false) {
    statusText.textContent = text;
    statusIndicator.className = loading ? 'loading' : '';
}

function setSendButtonMode(mode) {
    const stopping = mode === 'stop';
    sendBtn.textContent = stopping ? 'Stop' : 'Send';
    sendBtn.classList.toggle('stop', stopping);
    sendBtn.disabled = false;
}

function escapeHtml(text) {
    const div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
}

function renderMarkdown(text) {
    const html = marked.parse(text);
    return DOMPurify.sanitize(html);
}

function setWorkTab(tab) {
    state.workTab = tab;
    for (const button of workTabs) {
        button.classList.toggle('active', button.dataset.workTab === tab);
    }
    renderWorkPanel();
}

for (const button of workTabs) {
    button.addEventListener('click', () => setWorkTab(button.dataset.workTab));
}

async function loadWorkGraph(sessionId = state.currentSession?.id) {
    if (!sessionId) {
        state.workGraph = null;
        renderWorkPanel();
        return;
    }
    try {
        const resp = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/work-graph?token=${encodeURIComponent(token)}`);
        if (!resp.ok) throw new Error('Failed to load work graph');
        const data = await resp.json();
        state.workGraph = data.graph || null;
    } catch (e) {
        console.warn('Failed to load work graph', e);
        state.workGraph = null;
    }
    renderWorkPanel();
}

function renderWorkPanel() {
    if (!traceContainer) return;
    traceContainer.innerHTML = '';
    const graph = state.workGraph;
    if (!graph) {
        traceContainer.innerHTML = '<div class="trace-empty">No work graph is available for this session yet.</div>';
        return;
    }
    traceContainer.appendChild(createWorkOverview(graph));
    if (state.workTab === 'todo') {
        renderWorkTodos(graph);
    } else if (state.workTab === 'tasks') {
        renderWorkTasks(graph);
    } else if (state.workTab === 'trace') {
        renderWorkTrace(graph);
    } else {
        renderWorkTimeline(graph);
    }
}

function createWorkOverview(graph) {
    const nodes = graph.nodes || [];
    const counts = workGraphCounts(graph);
    const live = graphHasActiveWork(graph);
    const root = nodes.find(node => node.id === graph.root_node_id) || nodes[0] || {};
    const overview = document.createElement('div');
    overview.className = 'work-overview';
    overview.innerHTML = `
        <div class="work-overview-title">
            <span title="${escapeHtml(root.session_id || '')}">${escapeHtml(root.title || 'Session work')}</span>
            ${live ? '<span class="work-live-pill">live</span>' : ''}
        </div>
        <div class="work-stat-grid">
            <div class="work-stat"><div class="work-stat-value">${counts.running}</div><div class="work-stat-label">running</div></div>
            <div class="work-stat"><div class="work-stat-value">${counts.todos}</div><div class="work-stat-label">todos</div></div>
            <div class="work-stat"><div class="work-stat-value">${counts.tasks}</div><div class="work-stat-label">tasks</div></div>
            <div class="work-stat"><div class="work-stat-value">${counts.events}</div><div class="work-stat-label">events</div></div>
        </div>
    `;
    return overview;
}

function workGraphCounts(graph) {
    const nodes = graph.nodes || [];
    const activeKinds = new Set(['task', 'subagent', 'tool', 'assistant_turn', 'workflow', 'workflow_node', 'trace_run']);
    return {
        running: nodes.filter(node => activeKinds.has(node.kind) && node.status === 'running').length,
        todos: currentTodoNodes(graph).length,
        tasks: nodes.filter(node => node.kind === 'task' || node.kind === 'subagent').length,
        events: (graph.events || []).length,
    };
}

function renderWorkEmptyState(title, body, hint = '') {
    const empty = document.createElement('div');
    empty.className = 'work-empty-state';
    empty.innerHTML = `
        <div class="work-empty-title">${escapeHtml(title)}</div>
        <div>${escapeHtml(body)}</div>
        ${hint ? `<div class="work-empty-hint">${escapeHtml(hint)}</div>` : ''}
    `;
    traceContainer.appendChild(empty);
}

function renderWorkTimeline(graph) {
    const events = [...(graph.events || [])].sort((a, b) => (a.created_at || 0) - (b.created_at || 0));
    if (!events.length) {
        renderWorkNodesFallback(graph);
        return;
    }
    const section = document.createElement('div');
    section.className = 'work-section';
    const nodesById = new Map((graph.nodes || []).map(node => [node.id, node]));
    for (const event of events.slice(-40)) {
        const node = nodesById.get(event.node_id) || {};
        const card = document.createElement('div');
        card.className = 'work-card timeline-event';
        card.innerHTML = `
            <div class="work-card-header">
                <span class="work-kind">${escapeHtml(event.kind || node.kind || 'event')}</span>
                <span class="work-title" title="${escapeHtml(event.title || node.title || '')}">${escapeHtml(event.title || node.title || 'Work event')}</span>
            </div>
            <div class="work-meta">
                <span>${escapeHtml(node.kind || 'node')}</span>
                <span>${escapeHtml(formatTimestamp(event.created_at))}</span>
                ${event.summary ? `<span>${escapeHtml(String(event.summary).slice(0, 96))}</span>` : ''}
            </div>
        `;
        section.appendChild(card);
    }
    traceContainer.appendChild(section);
}

function renderWorkNodesFallback(graph) {
    const nodes = (graph.nodes || []).filter(node => node.id !== graph.root_node_id);
    if (!nodes.length) {
        renderWorkEmptyState(
            'No visible work events yet',
            'This session currently has only the user prompt and an empty assistant turn. No todo, subagent, tool, or active reasoning event has been recorded yet.',
            'If the agent is still thinking, wait a moment; once a stream delta, todo, or tool call arrives this panel will update automatically.'
        );
        return;
    }
    const section = document.createElement('div');
    section.className = 'work-section';
    for (const node of nodes) section.appendChild(createWorkNodeCard(node));
    traceContainer.appendChild(section);
}

function renderWorkTasks(graph) {
    const tasks = (graph.nodes || []).filter(node => node.kind === 'task' || node.kind === 'subagent');
    if (!tasks.length) {
        renderWorkEmptyState('No delegated tasks yet', 'This turn has not spawned a subagent or child task.', 'Tasks appear here when the assistant uses the Task tool or specialist runtime.');
        return;
    }
    const section = document.createElement('div');
    section.className = 'work-section';
    for (const task of tasks) section.appendChild(createWorkNodeCard(task));
    traceContainer.appendChild(section);
}

function renderWorkTodos(graph) {
    const todos = currentTodoNodes(graph);
    if (!todos.length) {
        renderWorkEmptyState('No todo checklist yet', 'The assistant has not called todowrite for this session.', 'Todo is the planning checklist; Tasks are delegated execution. They are intentionally shown separately.');
        return;
    }
    const section = document.createElement('div');
    section.className = 'work-section';
    for (const todo of todos) section.appendChild(createWorkNodeCard(todo));
    traceContainer.appendChild(section);
}

function currentTodoNodes(graph) {
    const latest = new Map();
    for (const node of graph.nodes || []) {
        if (node.kind !== 'todo') continue;
        const todoId = normalizedTodoKey(node);
        const previous = latest.get(todoId);
        const previousTime = previous?.started_at || 0;
        const currentTime = node.started_at || 0;
        if (!previous || currentTime >= previousTime) latest.set(todoId, node);
    }
    return Array.from(latest.values()).sort((a, b) => {
        const ap = Number(a.metadata?.priority ?? 9999);
        const bp = Number(b.metadata?.priority ?? 9999);
        if (ap !== bp) return ap - bp;
        return String(a.title || '').localeCompare(String(b.title || ''));
    });
}

function normalizedTodoKey(node) {
    const title = String(node.title || '').trim().replace(/\s+/g, ' ');
    if (title) return title;
    return node.metadata?.todo_id || node.id;
}

function renderWorkTrace(graph) {
    const runs = (graph.nodes || []).filter(node => node.trace_run_id && ['tool', 'workflow', 'workflow_node', 'subagent', 'assistant_turn', 'trace_run'].includes(node.kind));
    if (!runs.length) {
        renderWorkEmptyState('No trace runs yet', 'No tool, workflow, or specialist trace run is available for this session.', 'Trace is the debug view; use Timeline/Todo/Tasks for normal progress.');
        return;
    }
    const section = document.createElement('div');
    section.className = 'work-section';
    for (const run of runs) section.appendChild(createWorkNodeCard(run));
    traceContainer.appendChild(section);
}

function createWorkNodeCard(node) {
    const card = document.createElement('div');
    card.className = `work-card ${node.status || 'pending'}`;
    card.innerHTML = `
        <div class="work-card-header">
            <span class="work-kind">${escapeHtml(node.kind || 'node')}</span>
            <span class="work-title" title="${escapeHtml(node.title || '')}">${escapeHtml(node.title || node.id)}</span>
            <span class="work-status ${escapeHtml(node.status || 'pending')}">${escapeHtml(node.status || 'pending')}</span>
        </div>
        <div class="work-meta">
            ${node.started_at ? `<span>${escapeHtml(formatTimestamp(node.started_at))}</span>` : ''}
            ${node.finished_at ? `<span>${escapeHtml(formatDuration(node.started_at, node.finished_at))}</span>` : ''}
            ${node.summary ? `<span>${escapeHtml(String(node.summary).slice(0, 96))}</span>` : ''}
        </div>
        <div class="work-actions"></div>
    `;
    const actions = card.querySelector('.work-actions');
    if (node.session_id && node.kind !== 'session') {
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'work-action';
        button.textContent = 'Open session';
        button.addEventListener('click', () => openSyntheticSession(node.session_id, node.title || 'subagent'));
        actions.appendChild(button);
    }
    if (node.trace_run_id) {
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'work-action';
        button.textContent = 'Trace';
        button.addEventListener('click', () => openWorkTraceNode(node.trace_run_id, card));
        actions.appendChild(button);
    }
    if (!actions.children.length) actions.remove();
    return card;
}

async function openWorkTraceNode(runId, card) {
    let panel = card.querySelector(':scope > .run-event-panel');
    if (panel) {
        panel.remove();
        return;
    }
    panel = document.createElement('div');
    panel.className = 'run-event-panel';
    panel.innerHTML = '<div class="run-event-title">Trace</div><div class="trace-graph">Loading trace…</div>';
    card.appendChild(panel);
    const graph = panel.querySelector('.trace-graph');
    graph.innerHTML = '';
    try {
        const resp = await fetch(`/api/runs/${encodeURIComponent(runId)}?token=${encodeURIComponent(token)}`);
        if (resp.ok) {
            const data = await resp.json();
            if (data.run) graph.appendChild(await renderRunNode(data.run));
        }
    } catch (e) {
        console.warn('Failed to open work trace node', e);
    }
    if (!graph.children.length) graph.textContent = 'Trace run is not available.';
}

function formatTimestamp(value) {
    if (!value) return '—';
    return new Date(Number(value) * 1000).toLocaleTimeString();
}

function formatDuration(startedAt, finishedAt) {
    if (!startedAt || !finishedAt) return 'running';
    const seconds = Math.max(0, Number(finishedAt) - Number(startedAt));
    if (seconds < 60) return `${seconds}s`;
    return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

function showError(msg) {
    errorContainer.innerHTML = `
        <div class="error">
            <span>${escapeHtml(msg)}</span>
            <button class="error-close" onclick="closeError()">×</button>
        </div>
    `;
    errorContainer.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
}

function closeError() {
    errorContainer.innerHTML = '';
}

function clearChat() {
    chatContainer.innerHTML = '';
}

function addMessage(role, content) {
    const div = document.createElement('div');
    div.className = `message ${role}`;
    const renderedContent = (role === 'user' || role === 'assistant') ? renderMarkdown(content) : escapeHtml(content);
    div.innerHTML = `<div class="role">${role}</div><div class="content">${renderedContent}</div>`;
    chatContainer.appendChild(div);
    chatContainer.scrollTop = chatContainer.scrollHeight;
}

// Session Management
function renderSessions() {
    sessionList.innerHTML = '';
    state.sessions.forEach(session => {
        const item = document.createElement('div');
        item.className = 'session-item' + (session.id === state.currentSession?.id ? ' active' : '');
        item.innerHTML = `
            <span class="session-icon">💬</span>
            <span class="session-name" title="${escapeHtml(session.agent_type)}">${escapeHtml(session.id.substring(0, 8))}</span>
            <button class="session-delete-btn" title="Delete session">✕</button>
        `;
        item.addEventListener('click', (e) => {
            if (e.target.classList.contains('session-delete-btn')) {
                e.stopPropagation();
                deleteSession(session.id);
            } else {
                switchSession(session);
            }
        });
        sessionList.appendChild(item);
        for (const child of session.children || []) {
            const childItem = document.createElement('div');
            childItem.className = 'session-item session-child' + (child.id === state.currentSession?.id ? ' active' : '');
            const status = child.execution_status || child.result_status || 'pending';
            const label = child.title || child.agent_type || child.id.substring(0, 8);
            childItem.innerHTML = `
                <span class="session-status-dot ${escapeHtml(status)}"></span>
                <span class="session-name" title="${escapeHtml(label)}">${escapeHtml(child.agent_type || 'subagent')} · ${escapeHtml(label.substring(0, 28))}</span>
            `;
            childItem.addEventListener('click', () => switchSession(child));
            sessionList.appendChild(childItem);
        }
    });
}

async function refreshSessionTree() {
    try {
        const resp = await fetch(`/api/sessions/tree?token=${encodeURIComponent(token)}`);
        if (!resp.ok) return;
        const data = await resp.json();
        const tree = data.sessions || [];
        const byId = new Map(state.sessions.map(session => [session.id, session]));
        const nextSessions = [];
        for (const node of tree) {
            const session = { ...(byId.get(node.session.id) || {}), ...node.session, children: node.children || [] };
            nextSessions.push(session);
        }
        for (const session of state.sessions) {
            if (!nextSessions.some(item => item.id === session.id)) nextSessions.push(session);
        }
        state.sessions = nextSessions;
        renderSessions();
        saveState();
    } catch (e) {
        console.warn('Failed to refresh session tree', e);
    }
}

async function switchSession(session, forceReload = false) {
    if (!forceReload && state.currentSession?.id === session.id) return;
    state.currentSession = session;
    state.currentSessionId = session.id;
    agentSelector.value = session.agent_type;
    clearChat();
    renderSessions();
    setStatus('Loading...', true);

    // Load messages for this session
    try {
        const resp = await fetch(`/api/sessions/${session.id}/messages?token=${encodeURIComponent(token)}`);
        if (resp.ok) {
            const data = await resp.json();
            if (data.messages && data.messages.length > 0) {
                renderMessageHistory(data.messages);
            } else {
                addMessage('assistant', 'Session ready. How can I help you?');
            }
        } else {
            addMessage('assistant', 'Session ready. How can I help you?');
        }
    } catch (e) {
        addMessage('assistant', 'Session ready. How can I help you?');
    }

    renderSessions();
    setStatus('Ready');
    await refreshSessionTree();
    await loadWorkGraph(session.id);
    startRecoveryPollingIfActive();
}

async function reloadCurrentSessionMessages() {
    if (!state.currentSession) return;
    try {
        const resp = await fetch(`/api/sessions/${state.currentSession.id}/messages?token=${encodeURIComponent(token)}`);
        if (!resp.ok) return;
        const data = await resp.json();
        clearChat();
        if (data.messages && data.messages.length > 0) {
            renderMessageHistory(data.messages);
        } else {
            addMessage('assistant', 'Session ready. How can I help you?');
        }
    } catch (e) {
        console.warn('Failed to reload current session messages', e);
    }
}

function graphHasActiveWork(graph = state.workGraph) {
    const activeKinds = new Set(['task', 'subagent', 'tool', 'assistant_turn', 'workflow', 'workflow_node', 'trace_run']);
    return Boolean((graph?.nodes || []).some(node => activeKinds.has(node.kind) && node.status === 'running'));
}

function stopRecoveryPolling() {
    if (recoveryPollTimer) {
        clearInterval(recoveryPollTimer);
        recoveryPollTimer = null;
    }
}

function startRecoveryPollingIfActive() {
    stopRecoveryPolling();
    if (!state.currentSession || !graphHasActiveWork()) return;
    setStatus('Recovering live progress...', true);
    recoveryPollTimer = setInterval(async () => {
        if (!state.currentSession || isLoading) return;
        const beforeActive = graphHasActiveWork();
        await refreshSessionTree();
        await loadWorkGraph(state.currentSession.id);
        await reloadCurrentSessionMessages();
        if (!graphHasActiveWork()) {
            stopRecoveryPolling();
            await switchSession(state.currentSession, true);
            setStatus('Ready');
        } else if (beforeActive) {
            setStatus('Work still running...', true);
        }
    }, 2500);
}

function renderMessageHistory(messages) {
    const toolCallHosts = new Map();
    let lastAssistantDiv = null;
    let lastAssistantToolsEl = null;
    if (window.canvasController) {
        try { window.canvasController.hide(); } catch (e) { /* non-fatal */ }
    }
    for (const msg of messages) {
        if (msg.role === 'system') continue;

        if (msg.role === 'assistant') {
            if (isEmptyAssistantTurn(msg)) {
                renderEmptyAssistantTurn(msg);
                lastAssistantDiv = null;
                lastAssistantToolsEl = null;
                continue;
            }
            if (msg.reasoning) {
                const reasoningDiv = createReasoningDiv();
                reasoningDiv.open = true;
                reasoningDiv.querySelector('.reasoning-content').textContent = msg.reasoning;
            }
            const host = renderMessage(msg);
            lastAssistantDiv = host;
            lastAssistantToolsEl = ensureToolsContainer(host);
            for (const call of msg.tool_calls || []) {
                if (call.id) {
                    toolCallHosts.set(call.id, {
                        toolName: call.function?.name || 'tool',
                        input: call.function?.arguments || '',
                        toolsEl: lastAssistantToolsEl,
                        messageDiv: host
                    });
                }
            }
        } else if (msg.role === 'tool') {
            const host = toolCallHosts.get(msg.tool_call_id);
            if (host) {
                const details = createPersistedToolCall(host.toolName || msg.tool_name || 'tool', host.input, msg.content || '');
                host.toolsEl.appendChild(details);
                addTraceRunFromToolOutput(host.messageDiv, msg.content || '');
                replayCanvasEventFromToolOutput(msg.content || '');
            } else if (lastAssistantToolsEl) {
                const details = createPersistedToolCall(msg.tool_name || 'tool', '', msg.content || '');
                lastAssistantToolsEl.appendChild(details);
                addTraceRunFromToolOutput(lastAssistantDiv, msg.content || '');
                replayCanvasEventFromToolOutput(msg.content || '');
            } else {
                renderMessage(msg);
                replayCanvasEventFromToolOutput(msg.content || '');
            }
        } else {
            renderMessage(msg);
            if (msg.role === 'user') {
                lastAssistantDiv = null;
                lastAssistantToolsEl = null;
            }
        }
    }
}

function replayCanvasEventFromToolOutput(output) {
    if (!window.canvasController) return;
    try {
        const parsed = parseToolOutputJson(output);
        if (parsed && typeof parsed.event === 'string' && parsed.event.startsWith('canvas:')) {
            window.canvasController.handle(parsed.event_payload || parsed);
        }
    } catch (e) { /* persisted tool output may be non-JSON */ }
}

function isEmptyAssistantTurn(msg) {
    return msg.role === 'assistant'
        && !msg.active_turn
        && !msg.reasoning
        && !String(msg.content || '').trim()
        && !(msg.tool_calls || []).length;
}

function renderEmptyAssistantTurn(msg) {
    const div = renderMessage({
        ...msg,
        content: 'Assistant turn exists, but no content, reasoning, tool call, or trace was persisted yet.'
    });
    div.classList.add('empty-turn');
    return div;
}

function renderMessage(msg) {
    const div = document.createElement('div');
    div.className = `message ${msg.role}`;
    if (msg.active_turn) div.classList.add('active-turn');

    let content = '';
    if (msg.role === 'system') {
        content = escapeHtml(msg.content);
    } else if (msg.role === 'user') {
        if (Array.isArray(msg.content)) {
            content = msg.content.map(part => {
                if (part.type === 'text') return renderMarkdown(part.text);
                return escapeHtml(JSON.stringify(part));
            }).join('');
        } else {
            content = renderMarkdown(msg.content || '');
        }
    } else if (msg.role === 'assistant') {
        const assistantContent = msg.content || '';
        content = renderMarkdown(assistantContent);
    } else if (msg.role === 'tool') {
        content = `<span class="tool-label">[${msg.tool_name || 'tool'}]</span> ${escapeHtml(msg.content)}`;
    }

    div.innerHTML = `<div class="role">${msg.role}</div><div class="content">${content}</div>`;
    if (msg.role === 'assistant') {
        div.querySelector('.content').dataset.markdown = msg.content || '';
    }
    chatContainer.appendChild(div);
    chatContainer.scrollTop = chatContainer.scrollHeight;
    return div;
}

function ensureToolsContainer(messageDiv) {
    let toolsEl = messageDiv.querySelector('.assistant-tools');
    if (!toolsEl) {
        toolsEl = document.createElement('div');
        toolsEl.className = 'assistant-tools';
        messageDiv.appendChild(toolsEl);
    }
    return toolsEl;
}

function extractRootRunId(output) {
    const value = parseToolOutputJson(output);
    return value?._trace?.root_run_id || value?._trace?.current_run_id || null;
}

function parseToolOutputJson(output) {
    const text = output || '';
    try {
        return JSON.parse(text || 'null');
    } catch (e) {
        console.warn('Failed to parse tool output as JSON', e);
    }

    const start = text.search(/[\[{]/);
    if (start < 0) return null;

    const opener = text[start];
    const closer = opener === '{' ? '}' : ']';
    let depth = 0;
    let inString = false;
    let escaped = false;

    for (let i = start; i < text.length; i++) {
        const ch = text[i];
        if (inString) {
            if (escaped) {
                escaped = false;
            } else if (ch === '\\') {
                escaped = true;
            } else if (ch === '"') {
                inString = false;
            }
            continue;
        }

        if (ch === '"') {
            inString = true;
        } else if (ch === opener) {
            depth += 1;
        } else if (ch === closer) {
            depth -= 1;
            if (depth === 0) {
                try {
                    return JSON.parse(text.slice(start, i + 1));
                } catch (e) {
                    return null;
                }
            }
        }
    }

    return null;
}

function getTraceRunIds(messageDiv) {
    try {
        return JSON.parse(messageDiv.dataset.traceRunIds || '[]');
    } catch (e) {
        return [];
    }
}

function addTraceRunFromToolOutput(messageDiv, output) {
    if (!messageDiv) return;
    const runId = extractRootRunId(output);
    if (!runId) return;
    const runIds = getTraceRunIds(messageDiv);
    if (!runIds.includes(runId)) {
        runIds.push(runId);
        messageDiv.dataset.traceRunIds = JSON.stringify(runIds);
    }
    ensureTraceAction(messageDiv);
}

function ensureTraceAction(messageDiv) {
    if (messageDiv.querySelector(':scope > .trace-actions')) return;
    const actions = document.createElement('div');
    actions.className = 'trace-actions';
    actions.innerHTML = '<button type="button" class="trace-toggle-btn">⟡ Trace graph</button>';
    actions.querySelector('button').addEventListener('click', async () => {
        await toggleMessageTrace(messageDiv);
    });
    messageDiv.appendChild(actions);
}

async function toggleMessageTrace(messageDiv) {
    const existing = messageDiv.querySelector(':scope > .message-trace-panel');
    if (existing) {
        existing.remove();
        return;
    }

    const panel = document.createElement('div');
    panel.className = 'message-trace-panel';
    panel.innerHTML = '<div class="trace-graph-title">Execution graph</div><div class="trace-graph">Loading trace…</div>';
    messageDiv.appendChild(panel);

    const graph = panel.querySelector('.trace-graph');
    graph.innerHTML = '';
    const runIds = getTraceRunIds(messageDiv);
    for (const runId of runIds) {
        try {
            const resp = await fetch(`/api/runs/${encodeURIComponent(runId)}?token=${encodeURIComponent(token)}`);
            if (resp.ok) {
                const data = await resp.json();
                if (data.run) graph.appendChild(await renderRunNode(data.run));
            }
        } catch (e) {
            console.warn('Failed to load trace run', e);
        }
    }
    if (!graph.children.length) {
        graph.textContent = 'No trace run is available for this turn.';
    }
}

function createPersistedToolCall(toolName, input, output) {
    const details = document.createElement('details');
    details.className = 'tool-call';
    details.innerHTML = `
        <summary>
            <span class="tool-icon">▶</span>
            <span class="tool-name">${escapeHtml(toolName)}</span>
            <span class="tool-status done">done ✓</span>
        </summary>
        <div class="tool-body">
            <div class="tool-field">
                <div class="tool-field-label">Input</div>
                <div class="tool-field-content">${escapeHtml(input || '')}</div>
            </div>
            <div class="tool-field">
                <div class="tool-field-label">Output</div>
                <div class="tool-field-content tool-output">${escapeHtml(output || '')}</div>
            </div>
        </div>
    `;
    return details;
}

async function renderRunNode(run) {
    const node = document.createElement('details');
    node.className = 'run-node';
    node.open = run.status === 'failed' || run.kind === 'workflow' || run.kind === 'workflow_node';
    const duration = formatRunDuration(run);
    const childLabel = run.kind === 'workflow' || run.kind === 'workflow_node' ? 'graph root' : 'node';
    const sessionLink = run.session_id && run.kind === 'specialist'
        ? `<button type="button" class="run-open-session" data-action="session" data-session-id="${escapeHtml(run.session_id)}">Open session</button>`
        : '';
    node.innerHTML = `
        <summary>
            <span class="run-kind">${escapeHtml(run.kind)}</span>
            <span class="run-main">
                <span class="run-name">${escapeHtml(run.name)}</span>
                <span class="run-meta">
                    <span>${escapeHtml(childLabel)}</span>
                    <span>${escapeHtml(duration)}</span>
                </span>
            </span>
            <span class="run-actions">
                ${sessionLink}
                <button type="button" class="run-open-session" data-action="details" data-session-id="${escapeHtml(run.session_id || '')}">Details</button>
                <span class="run-status ${escapeHtml(run.status)}">${escapeHtml(run.status)}</span>
            </span>
        </summary>
        <details class="run-details-toggle">
            <summary>Raw input/output</summary>
            <div class="run-io-grid">
                <div>
                    <div class="run-io-label">Input</div>
                    <div class="run-json">${escapeHtml(formatJson(run.input_json))}</div>
                </div>
                <div>
                    <div class="run-io-label">Output</div>
                    <div class="run-json">${escapeHtml(formatJson(run.output_json || run.error || null))}</div>
                </div>
            </div>
        </details>
    `;
    for (const openButton of node.querySelectorAll('.run-open-session')) {
        openButton.addEventListener('click', async (e) => {
            e.preventDefault();
            e.stopPropagation();
            if (openButton.dataset.action === 'session') {
                await openSyntheticSession(openButton.dataset.sessionId, run.name);
            } else {
                await openRunDetails(run, node);
            }
        });
    }
    try {
        const childResp = await fetch(`/api/runs/${encodeURIComponent(run.id)}/children?token=${encodeURIComponent(token)}`);
        if (childResp.ok) {
            const data = await childResp.json();
            for (const child of data.children || []) {
                node.appendChild(await renderRunNode(child));
            }
        }
    } catch (e) {
        console.warn('Failed to load child runs', e);
    }
    return node;
}

async function openRunDetails(run, node) {
    const existing = node.querySelector(':scope > .run-event-panel');
    if (existing) {
        existing.remove();
        return;
    }

    node.open = true;
    let events = [];
    try {
        const resp = await fetch(`/api/runs/${encodeURIComponent(run.id)}/events?token=${encodeURIComponent(token)}`);
        if (resp.ok) {
            const data = await resp.json();
            events = data.events || [];
        }
    } catch (e) {
        console.warn('Failed to load run events', e);
    }

    const panel = document.createElement('div');
    panel.className = 'run-event-panel';
    panel.innerHTML = `
        <div class="run-event-title">Specialist details</div>
        <div class="run-json">${escapeHtml(JSON.stringify({
            run_id: run.id,
            session_id: run.session_id,
            input_json: run.input_json,
            output_json: run.output_json,
            error: run.error,
            events
        }, null, 2))}</div>
    `;
    node.appendChild(panel);
}

function formatJson(value) {
    if (value === undefined || value === null || value === '') return '—';
    if (typeof value === 'string') {
        try {
            return JSON.stringify(JSON.parse(value), null, 2);
        } catch (e) {
            return value;
        }
    }
    return JSON.stringify(value, null, 2);
}

function formatRunDuration(run) {
    if (!run.started_at || !run.finished_at) return run.started_at ? 'running' : 'not started';
    const seconds = Math.max(0, Number(run.finished_at) - Number(run.started_at));
    if (seconds < 1) return '<1s';
    if (seconds < 60) return `${seconds}s`;
    const minutes = Math.floor(seconds / 60);
    const rest = seconds % 60;
    return `${minutes}m ${rest}s`;
}

async function openSyntheticSession(sessionId, agentType) {
    const session = {
        id: sessionId,
        agent_type: agentType || 'specialist',
        created_at: Math.floor(Date.now() / 1000)
    };
    if (!state.sessions.some(s => s.id === sessionId)) {
        state.sessions.unshift(session);
    }
    await switchSession(session, true);
}

async function deleteSession(sessionId) {
    if (!confirm('Delete this session?')) return;
    try {
        const resp = await fetch(`/api/sessions/${sessionId}?token=${encodeURIComponent(token)}`, {
            method: 'DELETE'
        });
        if (!resp.ok) throw new Error('Failed to delete');
        if (state.currentSession?.id === sessionId) {
            state.currentSession = null;
            clearChat();
            const idx = state.sessions.findIndex(s => s.id === sessionId);
            if (idx !== -1) state.sessions.splice(idx, 1);
            if (state.sessions.length > 0) {
                switchSession(state.sessions[0]);
            }
        } else {
            const idx = state.sessions.findIndex(s => s.id === sessionId);
            if (idx !== -1) state.sessions.splice(idx, 1);
        }
        renderSessions();
        saveState();
    } catch (e) {
        showError('Failed to delete session: ' + e.message);
    }
}

async function createSession(agentType = 'general') {
    try {
        setStatus('Creating...', true);
        const resp = await fetch(`/api/sessions?token=${encodeURIComponent(token)}`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ agent_type: agentType })
        });
        const data = await resp.json();
        const session = data.session;
        state.currentSession = session;
        state.currentSessionId = session.id;
        agentSelector.value = session.agent_type;
        state.sessions.unshift(session);
        if (state.sessions.length > 10) state.sessions.pop();
        clearChat();
        renderSessions();
        setStatus('Ready');
        saveState();
        return true;
    } catch (e) {
        showError('Failed to create session: ' + e.message);
        setStatus('Error');
        return false;
    }
}

async function switchAgent() {
    if (!state.currentSession) {
        showError('No active session');
        return;
    }
    const newAgentType = agentSelector.value;
    if (newAgentType === state.currentSession.agent_type) {
        return;
    }
    try {
        setStatus('Switching agent...', true);
        const resp = await fetch(`/api/sessions/${state.currentSession.id}/switch-agent?token=${encodeURIComponent(token)}`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ agent_type: newAgentType })
        });
        if (!resp.ok) {
            throw new Error('Failed to switch agent');
        }
        const data = await resp.json();
        state.currentSession = data.session;
        state.currentSessionId = data.session.id;
        const idx = state.sessions.findIndex(s => s.id === state.currentSession.id);
        if (idx !== -1) {
            state.sessions[idx] = data.session;
        }
        renderSessions();
        saveState();
        addMessage('assistant', `Agent switched to ${newAgentType}. Memory preserved from previous agent.`);
        setStatus('Ready');
    } catch (e) {
        showError('Failed to switch agent: ' + e.message);
        setStatus('Error');
    }
}

// Tool Call Display (inline in chat)
function rememberActiveToolCall(toolCallId, toolName, details) {
    activeToolCallEls.push(details);
    if (toolCallId) {
        activeToolCallsById.set(toolCallId, details);
    }
    const key = toolName || 'tool';
    const queue = activeToolCallsByName.get(key) || [];
    queue.push(details);
    activeToolCallsByName.set(key, queue);
}

function takeActiveToolCall(toolCallId, toolName) {
    if (toolCallId && activeToolCallsById.has(toolCallId)) {
        const details = activeToolCallsById.get(toolCallId);
        activeToolCallsById.delete(toolCallId);
        activeToolCallEls = activeToolCallEls.filter(el => el !== details);
        const key = details.dataset.toolName || toolName || 'tool';
        const queue = activeToolCallsByName.get(key) || [];
        const idx = queue.indexOf(details);
        if (idx !== -1) queue.splice(idx, 1);
        if (queue.length === 0) activeToolCallsByName.delete(key);
        return details;
    }
    const key = toolName || 'tool';
    const queue = activeToolCallsByName.get(key);
    if (queue && queue.length > 0) {
        const details = queue.shift();
        if (queue.length === 0) activeToolCallsByName.delete(key);
        activeToolCallEls = activeToolCallEls.filter(el => el !== details);
        if (details.dataset.toolCallId) activeToolCallsById.delete(details.dataset.toolCallId);
        return details;
    }
    const details = activeToolCallEls.shift() || null;
    if (details?.dataset.toolCallId) activeToolCallsById.delete(details.dataset.toolCallId);
    return details;
}

function resetStreamingTurn() {
    currentAssistantDiv = null;
    currentReasoningDiv = null;
    currentAssistantToolsEl = null;
    currentMarkdownBuffer = '';
    activeToolCallEls = [];
    activeToolCallsByName.clear();
    activeToolCallsById.clear();
}

function markActiveToolCallsCanceled() {
    for (const details of activeToolCallEls) {
        const status = details.querySelector('.tool-status');
        const icon = details.querySelector('.tool-icon');
        if (status) {
            status.className = 'tool-status cancelled';
            status.textContent = 'stopped';
        }
        if (icon) icon.textContent = '▶';
        details.open = false;
    }
    activeToolCallEls = [];
    activeToolCallsByName.clear();
    activeToolCallsById.clear();
}

function createToolCall(toolCallId, toolName, input, metadata = null) {
    ensureAssistantTurn();
    const details = document.createElement('details');
    details.className = 'tool-call';
    if (toolCallId) details.dataset.toolCallId = toolCallId;
    details.dataset.toolName = toolName || 'tool';
    details.innerHTML = `
        <summary>
            <span class="tool-icon">▼</span>
            <span class="tool-name">${escapeHtml(toolName)}</span>
            <span class="tool-status running">running...</span>
        </summary>
        <div class="tool-body">
            <div class="tool-field">
                <div class="tool-field-label">Input</div>
                <div class="tool-field-content">${escapeHtml(input)}</div>
            </div>
            <div class="tool-field">
                <div class="tool-field-label">Output</div>
                <div class="tool-field-content tool-output"></div>
            </div>
        </div>
    `;
    currentAssistantToolsEl.appendChild(details);
    if (metadata && typeof metadata === 'object') {
        attachToolStartMetadata(details, metadata);
    }
    details.open = true;
    rememberActiveToolCall(toolCallId, toolName, details);
    chatContainer.scrollTop = chatContainer.scrollHeight;
    return details;
}

function attachToolStartMetadata(details, metadata) {
    // Canvas events ride on tool_start.metadata. If we recognise one, hand
    // off to the canvas controller and skip the trace/session UI for this
    // tool call (the canvas controller manages its own surface).
    if (metadata && typeof metadata.event === 'string' && metadata.event.startsWith('canvas:')) {
        if (window.canvasController) {
            try { window.canvasController.handle(metadata); } catch (e) { console.warn('canvas handle failed', e); }
        }
        return;
    }
    const childSessionId = metadata.child_session_id || metadata.session_id || metadata.task_id;
    const rootRunId = metadata.root_run_id || metadata.current_run_id;
    if (childSessionId) {
        upsertChildSessionFromMetadata(metadata, childSessionId);
        const action = document.createElement('div');
        action.className = 'trace-actions';
        action.innerHTML = '<button type="button" class="trace-toggle-btn">Open session</button>';
        action.querySelector('button').addEventListener('click', async (event) => {
            event.preventDefault();
            event.stopPropagation();
            await openSyntheticSession(childSessionId, metadata.specialist_id || metadata.description || 'subagent');
        });
        details.querySelector('.tool-body').prepend(action);
        refreshSessionTree();
    }
    if (rootRunId) {
        const messageDiv = details.closest('.message');
        if (messageDiv) {
            const runIds = getTraceRunIds(messageDiv);
            if (!runIds.includes(rootRunId)) {
                runIds.push(rootRunId);
                messageDiv.dataset.traceRunIds = JSON.stringify(runIds);
            }
            ensureTraceAction(messageDiv);
        }
    }
}

function upsertChildSessionFromMetadata(metadata, childSessionId) {
    const parentId = metadata.parent_session_id || state.currentSession?.id;
    const child = {
        id: childSessionId,
        agent_type: metadata.specialist_id || 'subagent',
        title: metadata.description || metadata.prompt || childSessionId.substring(0, 8),
        execution_status: metadata.mode === 'async' ? 'running' : 'running',
        result_status: 'pending',
        created_at: Math.floor(Date.now() / 1000)
    };
    let parent = state.sessions.find(session => session.id === parentId);
    if (!parent && state.currentSession?.id === parentId) {
        parent = state.currentSession;
        state.sessions.unshift(parent);
    }
    if (!parent) return;
    const children = parent.children || [];
    const existingIndex = children.findIndex(item => item.id === childSessionId);
    if (existingIndex === -1) {
        children.push(child);
    } else {
        children[existingIndex] = { ...children[existingIndex], ...child };
    }
    parent.children = children;
    renderSessions();
    saveState();
}

function finishToolCall(details, output) {
    const summary = details.querySelector('summary');
    const status = summary.querySelector('.tool-status');
    status.className = 'tool-status done';
    status.textContent = 'done ✓';
    summary.querySelector('.tool-icon').textContent = '▶';
    details.querySelector('.tool-output').textContent = output;
    details.open = false;
    addTraceRunFromToolOutput(details.closest('.message'), output);
    // Route canvas events that ride on tool_finish.output.
    try {
        const parsed = parseToolOutputJson(output);
        if (parsed && typeof parsed.event === 'string' && parsed.event.startsWith('canvas:') && window.canvasController) {
            // Use the richer event_payload when present (carries patch/state).
            window.canvasController.handle(parsed.event_payload || parsed);
        }
    } catch (e) { /* tool output not JSON; ignore */ }
    refreshSessionTree();
    loadWorkGraph();
    chatContainer.scrollTop = chatContainer.scrollHeight;
}

// Streaming Message Elements
let currentAssistantDiv = null;
let currentReasoningDiv = null;

function ensureAssistantTurn() {
    if (!currentAssistantDiv) {
        currentAssistantDiv = createAssistantDiv();
    }
    if (!currentAssistantToolsEl) {
        currentAssistantToolsEl = document.createElement('div');
        currentAssistantToolsEl.className = 'assistant-tools';
        currentAssistantDiv.appendChild(currentAssistantToolsEl);
    }
}

function createAssistantDiv() {
    const div = document.createElement('div');
    div.className = 'message assistant';
    div.innerHTML = `<div class="role">Assistant</div><div class="content"></div>`;
    chatContainer.appendChild(div);
    currentMarkdownBuffer = '';
    currentAssistantToolsEl = null;
    return div;
}

function createReasoningDiv() {
    const details = document.createElement('details');
    details.className = 'reasoning-message';
    details.innerHTML = `
        <summary><span class="icon">▶</span>Reasoning</summary>
        <div class="reasoning-content"></div>
    `;
    chatContainer.appendChild(details);
    return details;
}

function appendToDiv(div, role, text) {
    if (role === 'assistant') {
        currentMarkdownBuffer += text;
        const html = DOMPurify.sanitize(marked.parse(currentMarkdownBuffer));
        div.querySelector('.content').innerHTML = html;
    } else {
        div.querySelector('.reasoning-content').textContent += text;
    }
    chatContainer.scrollTop = chatContainer.scrollHeight;
}

// SSE Handler
function handleSSEEvent(event) {
    switch (event.type) {
        case 'reasoning_delta':
            if (!currentReasoningDiv) {
                currentReasoningDiv = createReasoningDiv();
            }
            appendToDiv(currentReasoningDiv, 'reasoning', event.data.delta);
            break;
        case 'content_delta':
            if (currentAssistantToolsEl && currentAssistantToolsEl.children.length > 0) {
                currentAssistantDiv = null;
                currentAssistantToolsEl = null;
                currentMarkdownBuffer = '';
            }
            if (!currentAssistantDiv) {
                currentAssistantDiv = createAssistantDiv();
            }
            appendToDiv(currentAssistantDiv, 'assistant', event.data.delta);
            break;
        case 'tool_start':
            createToolCall(event.data.tool_call_id, event.data.tool, event.data.input, event.data.metadata);
            break;
        case 'tool_finish':
            {
                const toolCallEl = takeActiveToolCall(event.data.tool_call_id, event.data.tool);
                if (toolCallEl) {
                    finishToolCall(toolCallEl, event.data.output);
                }
            }
            break;
        case 'finish':
            if (currentReasoningDiv) {
                currentReasoningDiv.open = false;
            }
            resetStreamingTurn();
            loadWorkGraph();
            break;
        case 'error':
            showError(event.data.error);
            break;
    }
}

// Send Message
async function sendMessage() {
    if (isLoading) {
        activeChatAbortController?.abort();
        return;
    }
    if (!state.currentSession) return;

    const message = messageInput.value.trim();
    if (!message) return;

    isLoading = true;
    activeChatAbortController = new AbortController();
    setSendButtonMode('stop');
    messageInput.value = '';
    setStatus('Thinking...', true);

    addMessage('user', message);

    resetStreamingTurn();

    try {
        const resp = await fetch(`/api/chat?token=${encodeURIComponent(token)}`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            signal: activeChatAbortController.signal,
            body: JSON.stringify({
                session_id: state.currentSession.id,
                agent_type: state.currentSession.agent_type || agentSelector.value || 'general',
                message
            })
        });

        if (!resp.ok) {
            const errorText = await resp.text();
            throw new Error(errorText || `Chat request failed (${resp.status})`);
        }

        const reader = resp.body.getReader();
        const decoder = new TextDecoder();
        let buffer = '';

        while (true) {
            const { done, value } = await reader.read();
            if (done) break;

            buffer += decoder.decode(value, { stream: true });
            const lines = buffer.split('\n');
            buffer = lines.pop();

            for (const line of lines) {
                if (line.startsWith('data: ')) {
                    try {
                        const event = JSON.parse(line.slice(6));
                        handleSSEEvent(event);
                    } catch (e) {
                        console.error('Failed to parse SSE event:', e, 'line:', line);
                    }
                }
            }
        }
    } catch (e) {
        if (e.name === 'AbortError') {
            markActiveToolCallsCanceled();
            setStatus('Stopped');
            return;
        }
        console.error('Fetch error:', e);
        showError('Connection error: ' + e.message);
    } finally {
        isLoading = false;
        activeChatAbortController = null;
        setSendButtonMode('send');
        if (statusText.textContent !== 'Stopped') {
            setStatus('Ready');
        }
        startRecoveryPollingIfActive();
    }
}

// Event Listeners
sendBtn.addEventListener('click', sendMessage);
messageInput.addEventListener('keydown', (e) => {
    if (e.isComposing || e.keyCode === 229) {
        return;
    }
    if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        sendMessage();
    }
});

newSessionBtn.addEventListener('click', async () => {
    if (await createSession()) {
        addMessage('assistant', 'Session ready. How can I help you?');
    }
});

agentSelector.addEventListener('change', switchAgent);

// Window resize handler
window.addEventListener('resize', updateHandles);

function showTokenError(message) {
    chatContainer.innerHTML = `
        <div style="display: flex; align-items: center; justify-content: center; height: 100%; flex-direction: column; text-align: center; padding: 20px;">
            <div style="color: var(--accent-red); margin-bottom: 16px;">
                <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                    <circle cx="12" cy="12" r="10"></circle>
                    <line x1="12" y1="8" x2="12" y2="12"></line>
                    <line x1="12" y1="16" x2="12.01" y2="16"></line>
                </svg>
            </div>
            <h2 style="color: var(--accent-red); margin: 0 0 8px 0;">Access Denied</h2>
            <p style="color: var(--text-secondary); margin: 0 0 16px 0;">${escapeHtml(message)}</p>
            <p style="font-size: 12px; color: var(--text-muted); margin: 0;">Please provide a valid token to access this page</p>
        </div>
    `;
}

async function validateToken() {
    if (!token) {
        showTokenError("No token provided in URL");
        return false;
    }
    try {
        const resp = await fetch(`/api/validate?token=${encodeURIComponent(token)}`);
        if (!resp.ok) {
            showTokenError("Invalid token");
            return false;
        }
        return true;
    } catch (e) {
        showTokenError("Token validation failed");
        return false;
    }
}

// Initialize
loadState();
applyState();

validateToken().then(valid => {
    if (!valid) return;
    loadServerInfo();
    loadAgents().then(() => {
        if (!state.currentSession && state.sessions.length > 0) {
            const restored = state.sessions.find(s => s.id === state.currentSessionId);
            state.currentSession = restored || state.sessions[0];
        }
        renderSessions();
        refreshSessionTree();
        if (state.currentSession) {
            switchSession(state.currentSession, true);
        } else {
            addMessage('assistant', 'Select a session or create a new one to start.');
        }
    });
});
