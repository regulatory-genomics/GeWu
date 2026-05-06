// State
const state = {
    sidebar: { width: 220, collapsed: false },
    workPanel: { width: 420 },
    projects: [],
    currentProjectId: null,
    expandedProjects: {},
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
const addProjectBtn = document.getElementById('add-project-btn');
const sidebarHandle = document.getElementById('sidebar-handle');
const workPanelHandle = document.getElementById('work-panel-handle');
const agentSelector = document.getElementById('agent-selector');
const traceContainer = document.getElementById('trace-container');
const workTabs = Array.from(document.querySelectorAll('.work-tab'));
const projectPickerModal = document.getElementById('project-picker-modal');
const projectPickerClose = document.getElementById('project-picker-close');
const projectPickerCancel = document.getElementById('project-picker-cancel');
const projectPickerCreate = document.getElementById('project-picker-create');
const projectPickerCurrent = document.getElementById('project-picker-current');
const projectNameInput = document.getElementById('project-name-input');
const projectPathInput = document.getElementById('project-path-input');
const projectDirectoryList = document.getElementById('project-directory-list');

let isLoading = false;
let activeChatAbortController = null;
let activeToolCallEls = [];
const activeToolCallsByName = new Map();
const activeToolCallsById = new Map();
let currentMarkdownBuffer = '';
let currentAssistantToolsEl = null;
let recoveryPollTimer = null;
let sessionEventSource = null;
const autoResumedGroups = new Set();
const pendingBackgroundCompletions = new Map();
const seenBackgroundCompletionGroups = new Set();
let suppressChatAutoScroll = false;
let selectedProjectDirectory = '';

// Configure marked
marked.setOptions({
    breaks: true,
    gfm: true
});

// Cookie-based auth: no URL token, no `window.token`. Cookies are sent
// automatically by fetch and EventSource for same-origin requests.

// Load persisted state
function loadState() {
    try {
        const saved = localStorage.getItem('gewu_state');
        if (saved) {
            const parsed = JSON.parse(saved);
            if (parsed.sidebar) state.sidebar = parsed.sidebar;
            if (parsed.workPanel && Number.isFinite(parsed.workPanel.width)) {
                state.workPanel.width = Math.min(Math.max(parsed.workPanel.width, 320), 900);
            }
            if (Array.isArray(parsed.projects)) state.projects = parsed.projects;
            if (parsed.currentProjectId) state.currentProjectId = parsed.currentProjectId;
            if (parsed.expandedProjects) state.expandedProjects = parsed.expandedProjects;
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
            workPanel: state.workPanel,
            projects: state.projects,
            currentProjectId: state.currentProjectId,
            expandedProjects: state.expandedProjects,
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
    document.documentElement.style.setProperty('--work-panel-width', state.workPanel.width + 'px');
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
    if (workPanelHandle) {
        workPanelHandle.style.right = state.workPanel.width + 'px';
    }
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

let isResizingWorkPanel = false;
let workPanelStartX = 0;
let workPanelStartWidth = 0;

function startWorkPanelResize(e) {
    e.preventDefault();
    isResizingWorkPanel = true;
    workPanelStartX = e.clientX;
    workPanelStartWidth = state.workPanel.width;
    document.body.style.userSelect = 'none';
    document.body.style.overflow = 'hidden';
    workPanelHandle.classList.add('dragging');
    document.addEventListener('mousemove', onWorkPanelResize);
    document.addEventListener('mouseup', stopWorkPanelResize);
}

function onWorkPanelResize(e) {
    if (!isResizingWorkPanel) return;
    const delta = workPanelStartX - e.clientX;
    const newWidth = Math.min(Math.max(workPanelStartWidth + delta, 320), 900);
    state.workPanel.width = newWidth;
    document.documentElement.style.setProperty('--work-panel-width', newWidth + 'px');
    updateHandles();
    if (window.canvasController && typeof window.canvasController.refresh === 'function') {
        window.canvasController.refresh();
    }
}

function stopWorkPanelResize() {
    isResizingWorkPanel = false;
    document.body.style.userSelect = '';
    document.body.style.overflow = '';
    workPanelHandle.classList.remove('dragging');
    document.removeEventListener('mousemove', onWorkPanelResize);
    document.removeEventListener('mouseup', stopWorkPanelResize);
    saveState();
    if (window.canvasController && typeof window.canvasController.refresh === 'function') {
        window.canvasController.refresh();
    }
}

if (workPanelHandle) {
    workPanelHandle.addEventListener('mousedown', startWorkPanelResize);
}

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
        const resp = await fetch('/api/info');
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
        const resp = await fetch('/api/agents');
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
    return String(text)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
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
        const resp = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/work-graph`);
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
    const canvasDock = document.getElementById('canvas-dock');
    const isCanvasTab = state.workTab === 'canvas';
    traceContainer.hidden = isCanvasTab;
    if (canvasDock) canvasDock.hidden = !isCanvasTab;
    traceContainer.innerHTML = '';
    if (isCanvasTab) {
        if (window.canvasController && typeof window.canvasController.mountDock === 'function') {
            window.canvasController.mountDock();
        }
        return;
    }
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
        const resp = await fetch(`/api/runs/${encodeURIComponent(runId)}`);
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

function isChatNearBottom(threshold = 80) {
    return chatContainer.scrollHeight - chatContainer.scrollTop - chatContainer.clientHeight <= threshold;
}

function scrollChatToBottom() {
    chatContainer.scrollTop = chatContainer.scrollHeight;
}

function shouldAutoScrollChat(options = {}) {
    return !suppressChatAutoScroll && (options.forceScroll || isChatNearBottom());
}

function preserveChatFollow(shouldFollow, renderFn) {
    const follow = shouldFollow ?? isChatNearBottom();
    const previousSuppress = suppressChatAutoScroll;
    suppressChatAutoScroll = true;
    let result;
    try {
        result = renderFn();
    } finally {
        suppressChatAutoScroll = previousSuppress;
    }
    if (follow) scrollChatToBottom();
    return result;
}

function addMessage(role, content, options = {}) {
    const div = document.createElement('div');
    div.className = `message ${role}`;
    const renderedContent = (role === 'user' || role === 'assistant') ? renderMarkdown(content) : escapeHtml(content);
    div.innerHTML = `<div class="role">${role}</div><div class="content">${renderedContent}</div>`;
    chatContainer.appendChild(div);
    if (shouldAutoScrollChat(options)) scrollChatToBottom();
}

// Session Management
function flattenProjectSessions(projects = state.projects) {
    return projects.flatMap(project => project.sessions || []);
}

function syncFlatSessionsFromProjects() {
    state.sessions = flattenProjectSessions();
}

function findProjectForSession(sessionId) {
    if (!sessionId) return null;
    for (const project of state.projects) {
        for (const session of project.sessions || []) {
            if (session.id === sessionId) return project;
            if ((session.children || []).some(child => child.id === sessionId)) return project;
        }
    }
    return null;
}

function findRootSession(sessionId) {
    for (const session of state.sessions) {
        if (session.id === sessionId) return session;
    }
    return null;
}

function selectProject(projectId) {
    if (!projectId) return;
    state.currentProjectId = projectId;
    state.expandedProjects[projectId] = true;
    saveState();
}

function projectLabel(project) {
    return project.name || project.project_id || 'Project';
}

function shortWorkdir(workdir = '') {
    if (!workdir) return '';
    const parts = workdir.split('/').filter(Boolean);
    return parts.length ? parts[parts.length - 1] : workdir;
}

function projectNameFromPath(path = '') {
    return path.split('/').filter(Boolean).pop() || 'Project';
}

function upsertSessionInProject(session) {
    const projectId = session.project_id || state.currentProjectId || state.projects[0]?.project_id;
    if (!projectId) {
        if (!state.sessions.some(item => item.id === session.id)) state.sessions.unshift(session);
        return;
    }
    let project = state.projects.find(item => item.project_id === projectId);
    if (!project) {
        project = { project_id: projectId, name: 'Project', workdir: '', sessions: [] };
        state.projects.unshift(project);
    }
    const sessions = project.sessions || [];
    const index = sessions.findIndex(item => item.id === session.id);
    if (index === -1) sessions.unshift(session);
    else sessions[index] = { ...sessions[index], ...session };
    project.sessions = sessions;
    syncFlatSessionsFromProjects();
}

function removeSessionFromState(sessionId) {
    state.sessions = state.sessions.filter(session => session.id !== sessionId);
    for (const project of state.projects) {
        project.sessions = (project.sessions || []).filter(session => session.id !== sessionId);
    }
}

function renderSessions() {
    sessionList.innerHTML = '';
    state.projects.forEach(project => {
        const isCurrentProject = project.project_id === state.currentProjectId;
        const expanded = state.expandedProjects[project.project_id] !== false;
        const section = document.createElement('div');
        section.className = 'project-section' + (isCurrentProject ? ' active' : '');
        const header = document.createElement('button');
        header.type = 'button';
        header.className = 'project-header';
        header.innerHTML = `
            <span class="project-caret">${expanded ? '▾' : '▸'}</span>
            <span class="project-title" title="${escapeHtml(project.workdir || '')}">${escapeHtml(projectLabel(project))}</span>
            <span class="project-workdir">${escapeHtml(shortWorkdir(project.workdir))}</span>
        `;
        header.addEventListener('click', () => {
            selectProject(project.project_id);
            state.expandedProjects[project.project_id] = !expanded;
            renderSessions();
            saveState();
        });
        section.appendChild(header);

        const body = document.createElement('div');
        body.className = 'project-body';
        body.hidden = !expanded;
        (project.sessions || []).forEach(session => {
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
        body.appendChild(item);
        const childrenEl = document.createElement('div');
        childrenEl.className = 'session-children';
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
            childrenEl.appendChild(childItem);
        }
        if (childrenEl.childElementCount > 0) body.appendChild(childrenEl);
        });
        section.appendChild(body);
        sessionList.appendChild(section);
    });
}

async function refreshSessionTree() {
    try {
        const resp = await fetch('/api/projects/tree');
        if (!resp.ok) return;
        const data = await resp.json();
        const projects = data.projects || [];
        const byId = new Map(state.sessions.map(session => [session.id, session]));
        const nextProjects = projects.map(project => ({
            ...project,
            sessions: (project.sessions || []).map(node => ({
                ...(byId.get(node.session.id) || {}),
                ...node.session,
                children: node.children || []
            }))
        }));
        state.projects = nextProjects;
        syncFlatSessionsFromProjects();
        if (!state.currentProjectId && state.projects.length > 0) {
            state.currentProjectId = state.projects[0].project_id;
        }
        if (state.currentSessionId && !state.currentSession) {
            state.currentSession = state.sessions.find(s => s.id === state.currentSessionId) || null;
        }
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
    const project = session.project_id ? state.projects.find(p => p.project_id === session.project_id) : findProjectForSession(session.id);
    if (project) selectProject(project.project_id);
    agentSelector.value = session.agent_type;
    clearChat();
    renderSessions();
    setStatus('Loading...', true);
    let messages = [];

    // Load messages for this session
    try {
        const resp = await fetch(`/api/sessions/${session.id}/messages`);
        if (resp.ok) {
            const data = await resp.json();
            messages = data.messages || [];
            if (messages.length > 0) {
                renderMessageHistory(messages);
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
    queueUnresumedPersistedBackgroundCompletions(messages);
    startRecoveryPollingIfActive();
    startSessionEvents();
    await drainPendingBackgroundCompletions();
}

async function reloadCurrentSessionMessages() {
    if (!state.currentSession) return;
    const shouldFollow = isChatNearBottom();
    try {
        const resp = await fetch(`/api/sessions/${state.currentSession.id}/messages`);
        if (!resp.ok) return;
        const data = await resp.json();
        const messages = data.messages || [];
        preserveChatFollow(shouldFollow, () => {
            clearChat();
            if (messages.length > 0) {
                renderMessageHistory(messages, { autoScroll: false });
            } else {
                addMessage('assistant', 'Session ready. How can I help you?', { forceScroll: false });
            }
        });
        queueUnresumedPersistedBackgroundCompletions(messages);
    } catch (e) {
        console.warn('Failed to reload current session messages', e);
    }
}

function parseBackgroundCompletionMessage(msg) {
    if (msg?.role !== 'system' || typeof msg.content !== 'string') return null;
    if (!msg.content.startsWith('[background_tasks.completed]')) return null;
    const match = msg.content.match(/^\[background_tasks\.completed\]\s+group_id=([^\s]+)/);
    if (!match) return null;
    const summary = msg.content.split('\n').find(line => line.startsWith('Background tasks completed:')) || 'Background tasks completed';
    return {
        group_id: match[1],
        parent_session_id: state.currentSession?.id,
        summary,
    };
}

function queueUnresumedPersistedBackgroundCompletions(messages) {
    if (!state.currentSession) return;
    let completionIndex = -1;
    for (let i = messages.length - 1; i >= 0; i -= 1) {
        if (parseBackgroundCompletionMessage(messages[i])) {
            completionIndex = i;
            break;
        }
    }
    if (completionIndex < 0) return;

    const hasLaterConversation = messages
        .slice(completionIndex + 1)
        .some(msg => msg?.role === 'user' || msg?.role === 'assistant');
    if (hasLaterConversation) return;

    const lastCompletion = parseBackgroundCompletionMessage(messages[completionIndex]);
    if (!lastCompletion?.group_id) return;
    if (seenBackgroundCompletionGroups.has(lastCompletion.group_id)) return;
    if (autoResumedGroups.has(lastCompletion.group_id)) return;
    seenBackgroundCompletionGroups.add(lastCompletion.group_id);
    rememberPendingBackgroundCompletion(lastCompletion);
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

function stopSessionEvents() {
    if (sessionEventSource) {
        sessionEventSource.close();
        sessionEventSource = null;
    }
}

function startSessionEvents() {
    stopSessionEvents();
    if (!state.currentSession) return;
    const sessionId = state.currentSession.id;
    sessionEventSource = new EventSource(`/api/events?session_id=${encodeURIComponent(sessionId)}`);
    sessionEventSource.addEventListener('background_group.completed', async (event) => {
        if (!state.currentSession || state.currentSession.id !== sessionId) return;
        let payload = {};
        try { payload = JSON.parse(event.data || '{}'); } catch (e) { /* ignore */ }
        await refreshSessionTree();
        await loadWorkGraph(sessionId);
        await reloadCurrentSessionMessages();
        setStatus(payload.summary || 'Background tasks completed');
        await autoResumeAfterBackgroundCompletion(payload);
    });
}

function rememberPendingBackgroundCompletion(payload) {
    const sessionId = payload?.parent_session_id || state.currentSession?.id;
    if (!sessionId) return;
    const key = payload?.group_id || `completion:${Date.now()}`;
    const pending = pendingBackgroundCompletions.get(sessionId) || new Map();
    pending.set(key, payload || {});
    pendingBackgroundCompletions.set(sessionId, pending);
}

async function drainPendingBackgroundCompletions() {
    if (!state.currentSession || isLoading) return;
    const pending = pendingBackgroundCompletions.get(state.currentSession.id);
    if (!pending || pending.size === 0) return;

    const [, payload] = pending.entries().next().value;
    const groupId = payload?.group_id;
    if (groupId) pending.delete(groupId);
    else pending.clear();
    if (pending.size === 0) pendingBackgroundCompletions.delete(state.currentSession.id);

    await autoResumeAfterBackgroundCompletion(payload);
}

async function autoResumeAfterBackgroundCompletion(payload) {
    if (!state.currentSession) return;
    if (isLoading) {
        rememberPendingBackgroundCompletion(payload);
        return;
    }
    const groupId = payload?.group_id;
    if (groupId) {
        if (autoResumedGroups.has(groupId)) return;
        autoResumedGroups.add(groupId);
    }
    const message = 'Background tasks have completed. Review the injected completion summary in your session context and continue your work. Do not call get_task_status or list_child_tasks.';
    await sendMessageText(message, { internal: true });
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

function renderMessageHistory(messages, options = {}) {
    const shouldFollow = options.autoScroll ?? isChatNearBottom();
    preserveChatFollow(shouldFollow, () => renderMessageHistoryContent(messages));
}

function renderMessageHistoryContent(messages) {
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

function renderMessage(msg, options = {}) {
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
    if (shouldAutoScrollChat(options)) scrollChatToBottom();
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
            const resp = await fetch(`/api/runs/${encodeURIComponent(runId)}`);
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
        const childResp = await fetch(`/api/runs/${encodeURIComponent(run.id)}/children`);
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
        const resp = await fetch(`/api/runs/${encodeURIComponent(run.id)}/events`);
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
        project_id: state.currentProjectId,
        agent_type: agentType || 'specialist',
        created_at: Math.floor(Date.now() / 1000)
    };
    upsertSessionInProject(session);
    await switchSession(session, true);
}

async function deleteSession(sessionId) {
    if (!confirm('Delete this session?')) return;
    try {
        const resp = await fetch(`/api/sessions/${sessionId}`, {
            method: 'DELETE'
        });
        if (!resp.ok) throw new Error('Failed to delete');
        if (state.currentSession?.id === sessionId) {
            state.currentSession = null;
            clearChat();
            removeSessionFromState(sessionId);
            if (state.sessions.length > 0) {
                switchSession(state.sessions[0]);
            }
        } else {
            removeSessionFromState(sessionId);
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
        const resp = await fetch('/api/sessions', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ agent_type: agentType, project_id: state.currentProjectId })
        });
        const data = await resp.json();
        const session = data.session;
        state.currentSession = session;
        state.currentSessionId = session.id;
        state.currentProjectId = session.project_id || state.currentProjectId;
        agentSelector.value = session.agent_type;
        upsertSessionInProject(session);
        clearChat();
        renderSessions();
        await refreshSessionTree();
        setStatus('Ready');
        saveState();
        return true;
    } catch (e) {
        showError('Failed to create session: ' + e.message);
        setStatus('Error');
        return false;
    }
}

async function createProject() {
    if (!selectedProjectDirectory) {
        await openProjectPicker();
        return false;
    }
    const workdir = selectedProjectDirectory;
    const defaultName = projectNameFromPath(workdir);
    const name = projectNameInput?.value?.trim() || defaultName;
    try {
        setStatus('Creating project...', true);
        const resp = await fetch('/api/projects', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name, workdir })
        });
        const data = await readJsonResponse(resp);
        if (!resp.ok) throw new Error(data?.error || 'Failed to create project');
        const project = data.project;
        state.currentProjectId = project.project_id;
        state.expandedProjects[project.project_id] = true;
        closeProjectPicker();
        await refreshSessionTree();
        setStatus('Ready');
        return true;
    } catch (e) {
        showError('Failed to create project: ' + e.message);
        setStatus('Error');
        return false;
    }
}

async function readJsonResponse(resp) {
    const text = await resp.text();
    if (!text) return {};
    try {
        return JSON.parse(text);
    } catch (_err) {
        return { error: text };
    }
}

async function openProjectPicker(path = '') {
    selectedProjectDirectory = '';
    if (projectNameInput) projectNameInput.value = '';
    if (projectPathInput) projectPathInput.value = '';
    if (projectPickerModal) projectPickerModal.hidden = false;
    await browseProjectDirectories(path);
}

function closeProjectPicker() {
    selectedProjectDirectory = '';
    if (projectPickerModal) projectPickerModal.hidden = true;
}

async function browseProjectDirectories(path = '') {
    if (!projectDirectoryList) return;
    const query = path ? `?path=${encodeURIComponent(path)}` : '';
    projectDirectoryList.innerHTML = '<div class="directory-loading">Loading directories…</div>';
    try {
        const resp = await fetch(`/api/projects/directories${query}`);
        const data = await readJsonResponse(resp);
        if (!resp.ok) throw new Error(data?.error || 'Failed to browse directories');
        renderDirectoryChoices(data.directories || []);
    } catch (e) {
        projectDirectoryList.innerHTML = `<div class="directory-error">${escapeHtml(e.message)}</div>`;
    }
}

function renderDirectoryChoices(directories) {
    if (!projectDirectoryList) return;
    projectDirectoryList.innerHTML = '';
    if (directories.length === 0) {
        projectDirectoryList.innerHTML = '<div class="directory-empty">No directories found.</div>';
        return;
    }
    for (const directory of directories) {
        const row = document.createElement('button');
        row.type = 'button';
        row.className = 'directory-choice' + (directory.is_root ? ' root' : '');
        row.innerHTML = `
            <span class="directory-icon">${directory.name === '..' ? '↩' : '📁'}</span>
            <span class="directory-main">
                <span class="directory-name">${escapeHtml(directory.name)}</span>
                <span class="directory-path">${escapeHtml(directory.path)}</span>
            </span>
            <span class="directory-open">Open</span>
        `;
        row.addEventListener('click', () => {
            selectedProjectDirectory = directory.path;
            if (projectPathInput) projectPathInput.value = directory.path;
            if (projectNameInput && !projectNameInput.value.trim() && directory.name !== '..') {
                projectNameInput.value = projectNameFromPath(directory.path);
            }
            if (projectPickerCurrent) projectPickerCurrent.textContent = directory.path;
            browseProjectDirectories(directory.path);
        });
        projectDirectoryList.appendChild(row);
    }
}

function firstAvailableSession() {
    return state.sessions.find(s => s.id === state.currentSessionId) || state.sessions[0] || null;
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
        const resp = await fetch(`/api/sessions/${state.currentSession.id}/switch-agent`, {
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
        upsertSessionInProject(data.session);
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
    const shouldFollow = isChatNearBottom();
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
    if (shouldFollow) scrollChatToBottom();
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
        project_id: state.currentProjectId,
        agent_type: metadata.specialist_id || 'subagent',
        title: metadata.description || metadata.prompt || childSessionId.substring(0, 8),
        execution_status: metadata.mode === 'async' ? 'running' : 'running',
        result_status: 'pending',
        created_at: Math.floor(Date.now() / 1000)
    };
    let parent = findRootSession(parentId);
    if (!parent && state.currentSession?.id === parentId) {
        parent = state.currentSession;
        upsertSessionInProject(parent);
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
    const shouldFollow = isChatNearBottom();
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
    if (shouldFollow) scrollChatToBottom();
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
    const shouldFollow = isChatNearBottom();
    if (role === 'assistant') {
        currentMarkdownBuffer += text;
        const html = DOMPurify.sanitize(marked.parse(currentMarkdownBuffer));
        div.querySelector('.content').innerHTML = html;
    } else {
        div.querySelector('.reasoning-content').textContent += text;
    }
    if (shouldFollow) scrollChatToBottom();
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
async function sendMessageText(message, options = {}) {
    if (!state.currentSession || !message) return;
    isLoading = true;
    activeChatAbortController = new AbortController();
    setSendButtonMode('stop');
    setStatus(options.internal ? 'Continuing...' : 'Thinking...', true);
    if (!options.internal) addMessage('user', message, { forceScroll: true });
    resetStreamingTurn();
    try {
        const resp = await fetch('/api/chat', {
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
        if (statusText.textContent !== 'Stopped') setStatus('Ready');
        startRecoveryPollingIfActive();
        await drainPendingBackgroundCompletions();
    }
}

async function sendMessage() {
    if (isLoading) {
        activeChatAbortController?.abort();
        return;
    }
    if (!state.currentSession) return;
    const message = messageInput.value.trim();
    if (!message) return;
    messageInput.value = '';
    await sendMessageText(message);
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

addProjectBtn?.addEventListener('click', () => openProjectPicker());
projectPickerClose?.addEventListener('click', closeProjectPicker);
projectPickerCancel?.addEventListener('click', closeProjectPicker);
projectPickerCreate?.addEventListener('click', createProject);
projectPickerModal?.addEventListener('click', (event) => {
    if (event.target === projectPickerModal) closeProjectPicker();
});

agentSelector.addEventListener('change', switchAgent);

// Window resize handler
window.addEventListener('resize', updateHandles);


async function validateSession() {
    try {
        const modeResp = await fetch('/api/auth/mode');
        const modeData = await modeResp.json();
        if (!modeData.password_required) return true;
        const resp = await fetch('/api/validate');
        if (!resp.ok) {
            window.location.href = '/login';
            return false;
        }
        return true;
    } catch (e) {
        window.location.href = '/login';
        return false;
    }
}

// Initialize
loadState();
applyState();

validateSession().then(valid => {
    if (!valid) return;
    loadServerInfo();
    loadAgents().then(() => {
        renderSessions();
        refreshSessionTree().then(() => {
            if (!state.currentSession && state.sessions.length > 0) {
                state.currentSession = firstAvailableSession();
            }
            if (state.currentSession) {
                switchSession(state.currentSession, true);
            } else {
                addMessage('assistant', 'Select a session or create a new one to start.');
            }
        });
    });
});
