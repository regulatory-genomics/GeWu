const board = document.querySelector('#board');
const title = document.querySelector('#title');
const notice = document.querySelector('#notice');
const back = document.querySelector('#projects');
const statuses = ['Pending', 'In progress', 'Awaiting review', 'Completed', 'Blocked', 'Cancelled'];
let projects = [];

function el(tag, text, className) {
  const node = document.createElement(tag);
  node.textContent = text;
  if (className) node.className = className;
  return node;
}

function navigate(project) {
  const url = new URL(location.href);
  if (project === null) url.searchParams.delete('project');
  else url.searchParams.set('project', project);
  history.pushState(null, '', url);
  render();
  title.focus();
}

function render() {
  const selected = new URLSearchParams(location.search).get('project');
  const project = projects.find(({ id }) => id === selected);
  back.hidden = selected === null;
  title.textContent = selected === null ? 'Project board' : `${selected} — Task board`;
  const next = document.createDocumentFragment();
  if (selected === null) {
    board.className = 'projects';
    for (const item of projects) {
      const button = el('button', '', 'card project');
      button.append(el('strong', item.id), el('span', `${item.tasks.length} task(s) · ${item.complete ? 'Complete' : 'Open'}`));
      button.addEventListener('click', () => navigate(item.id));
      next.append(button);
    }
    if (!projects.length) next.append(el('p', 'No projects yet.'));
  } else if (!project) {
    board.className = '';
    next.append(el('p', 'Project not found.'));
  } else {
    board.className = 'tasks';
    for (const status of statuses) {
      const tasks = project.tasks.filter(task => task.status === status);
      const column = el('section', '', 'column');
      column.append(el('h2', `${status} (${tasks.length})`));
      for (const task of tasks) {
        const card = el('article', '', 'card');
        card.append(el('h3', task.name), el('p', task.id, 'muted'));
        if (task.deliverable) card.append(el('p', task.deliverable));
        if (task.dependencies.length) card.append(el('p', `Depends on: ${task.dependencies.join(', ')}`, 'muted'));
        column.append(card);
      }
      next.append(column);
    }
  }
  board.replaceChildren(next);
}

back.addEventListener('click', () => navigate(null));
window.addEventListener('popstate', render);

const login = document.querySelector('#login');
const input = document.querySelector('#token');
let token = sessionStorage.getItem('gewu-lab-token');
const chatForm = document.querySelector('#chat-form');
const chatText = document.querySelector('#chat-text');
const chatSend = document.querySelector('#chat-send');
const chatLog = document.querySelector('#chat-log');
const chatStatus = document.querySelector('#chat-status');
let chatSocket;
let pending = false;

function chatControls(enabled, status) {
  chatText.disabled = chatSend.disabled = !enabled;
  chatStatus.textContent = status;
}

chatForm.addEventListener('submit', event => {
  event.preventDefault();
  const text = chatText.value.trim();
  if (!text || pending || chatSocket?.readyState !== WebSocket.OPEN) return;
  if (new TextEncoder().encode(text).length > 4096) {
    chatStatus.textContent = 'Message exceeds 4096 bytes.';
    return;
  }
  chatSocket.send(JSON.stringify({ type: 'chat_send', text }));
  chatLog.append(el('p', `You: ${text}`));
  chatText.value = '';
  pending = true;
  chatControls(false, 'Waiting for Lab…');
});

function connect() {
  const socket = new WebSocket(`${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/ws`);
  let rejected = false;
  socket.addEventListener('open', () => socket.send(JSON.stringify({ token })));
  socket.addEventListener('message', ({ data }) => {
    const message = JSON.parse(data);
    if (message.type === 'unauthorized') {
      rejected = true;
      token = null;
      sessionStorage.removeItem('gewu-lab-token');
      projects = [];
      board.replaceChildren();
      notice.textContent = 'Enter the Lab access token.';
      login.hidden = false;
      input.focus();
      return;
    }
    if (message.type === 'chat_ready') {
      chatSocket = socket;
      chatControls(true, '');
      return;
    }
    if (message.type === 'chat_reply' || message.type === 'chat_error') {
      pending = false;
      chatLog.append(el('p', `Lab: ${message.text}`));
      chatControls(true, '');
      return;
    }
    if (!Array.isArray(message)) return;
    if (!chatSocket) chatControls(false, 'Lab chat is not configured.');
    projects = message;
    if (token) sessionStorage.setItem('gewu-lab-token', token);
    notice.textContent = '';
    render();
  });
  socket.addEventListener('close', () => {
    if (chatSocket === socket) {
      chatSocket = undefined;
      if (pending) chatLog.append(el('p', 'Disconnected before the reply. The message was not resent.'));
      pending = false;
      chatControls(false, 'Chat disconnected.');
    }
    if (!rejected) {
      notice.textContent = 'Disconnected. Reconnecting…';
      setTimeout(connect, 1500);
    }
  });
}

login.addEventListener('submit', event => {
  event.preventDefault();
  token = input.value;
  input.value = '';
  login.hidden = true;
  connect();
});

connect();
