import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('./app.js', import.meta.url), 'utf8');
const indexHtml = readFileSync(new URL('./index.html', import.meta.url), 'utf8');
const appCss = readFileSync(new URL('./app.css', import.meta.url), 'utf8');

function makeElement() {
    const children = [];
    const queryCache = new Map();
    let className = '';
    return {
        get className() { return className; },
        set className(value) { className = value || ''; },
        classList: { add() {}, remove() {}, toggle() {}, contains() { return false; } },
        children,
        dataset: {},
        style: { setProperty() {} },
        addEventListener() {},
        appendChild(child) { children.push(child); return child; },
        prepend(child) { children.unshift(child); return child; },
        remove() {},
        querySelector(selector) {
            if (!queryCache.has(selector)) queryCache.set(selector, makeElement());
            return queryCache.get(selector);
        },
        querySelectorAll() { return []; },
        closest() { return null; },
        scrollIntoView() {},
        get childElementCount() { return children.length; },
        get lastElementChild() { return children[children.length - 1] || null; },
        set innerHTML(value) { this._innerHTML = value; },
        get innerHTML() { return this._innerHTML || ''; },
        set textContent(value) { this._textContent = value; },
        get textContent() { return this._textContent || ''; },
        value: 'general',
        scrollTop: 0,
        scrollHeight: 0,
        clientHeight: 0,
    };
}

function makeJsonResponse(body, ok = true) {
    return {
        ok,
        json: async () => body,
        text: async () => JSON.stringify(body),
    };
}

async function flushInitialization() {
    for (let i = 0; i < 6; i += 1) {
        await new Promise(resolve => setImmediate(resolve));
    }
}

function createHarness(fetchResponseOrHandler) {
    const elements = new Map();
    const fetchCalls = [];
    const getElement = (id) => {
        if (!elements.has(id)) elements.set(id, makeElement());
        return elements.get(id);
    };
    const errorContainer = getElement('error-container');
    const statusText = getElement('status-text');
    const fetchHandler = typeof fetchResponseOrHandler === 'function'
        ? fetchResponseOrHandler
        : () => fetchResponseOrHandler;

    const window = { addEventListener() {}, canvasController: { setSession() {}, hide() {} }, location: { href: '' } };
    const context = {
        console: { warn() {}, error() {}, log() {} },
        fetch: async (url, options) => {
            fetchCalls.push(String(url));
            return fetchHandler(url, options);
        },
        localStorage: { getItem() { return null; }, setItem() {} },
        marked: { setOptions() {}, parse(value) { return value; } },
        DOMPurify: { sanitize(value) { return value; } },
        window,
        location: window.location,
        EventSource: class { addEventListener() {} close() {} },
        AbortController: class {},
        setTimeout() { return 0; },
        clearTimeout() {},
        setInterval() { return 0; },
        clearInterval() {},
        document: {
            documentElement: { style: { setProperty() {} } },
            body: makeElement(),
            getElementById: getElement,
            querySelectorAll() { return []; },
            createElement() { return makeElement(); },
            addEventListener() {},
        },
    };
    context.window.state = undefined;
    vm.createContext(context);
    vm.runInContext(source, context);
    return { context, errorContainer, statusText, fetchCalls, elements };
}

test('createSession surfaces backend JSON errors from failed responses', async () => {
    const response = {
        ok: false,
        text: async () => JSON.stringify({ error: 'No factory can resolve capability: memory_sql_query' }),
    };
    const { context, errorContainer, statusText } = createHarness(response);

    const result = await vm.runInContext('createSession("general")', context);

    assert.equal(result, false);
    assert.match(errorContainer.innerHTML, /No factory can resolve capability: memory_sql_query/);
    assert.doesNotMatch(errorContainer.innerHTML, /undefined \(reading 'id'\)/);
    assert.equal(statusText.textContent, 'Error');
});

test('index does not render a global Skills panel', () => {
    assert.doesNotMatch(indexHtml, /class="skills-panel"/);
    assert.doesNotMatch(indexHtml, /id="skills-list"/);
    assert.doesNotMatch(indexHtml, />Skills</);
});

test('app initialization does not fetch global skills', async () => {
    const { fetchCalls } = createHarness((url) => {
        if (url === '/api/auth/mode') return makeJsonResponse({ password_required: false });
        if (url === '/api/info') return makeJsonResponse({ model: 'test', endpoint: 'test' });
        if (url === '/api/agents') return makeJsonResponse({ agents: [{ name: 'general' }] });
        if (url === '/api/projects/tree') return makeJsonResponse({ projects: [] });
        if (url === '/api/skills') return makeJsonResponse({ skills: [{ name: 'hidden', description: 'hidden' }] });
        return makeJsonResponse({});
    });

    await flushInitialization();

    assert.ok(fetchCalls.includes('/api/agents'));
    assert.ok(!fetchCalls.includes('/api/skills'), `unexpected /api/skills fetches: ${fetchCalls.join(', ')}`);
});

test('app css does not include Skills panel styles', () => {
    assert.doesNotMatch(appCss, /\.skills-panel/);
    assert.doesNotMatch(appCss, /\.skill-row/);
    assert.doesNotMatch(appCss, /\.skill-badge/);
});

test('load_skill tool calls still render in transcript details', () => {
    const { context } = createHarness(makeJsonResponse({ password_required: true }));

    const html = vm.runInContext(
        'createPersistedToolCall("load_skill", "{\\\"name\\\":\\\"gene-locus-resolver\\\"}", "<skill_content name=\\\"gene-locus-resolver\\\">loaded</skill_content>").innerHTML',
        context,
    );

    assert.match(html, /load_skill/);
    assert.match(html, /done ✓/);
    assert.match(html, /gene-locus-resolver/);
});
