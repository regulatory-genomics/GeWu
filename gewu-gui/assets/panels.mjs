import {bindFrame} from './plugin-bridge.mjs';
import {button, changes, editor, node, preview} from './editor.mjs';

export const dismissedKey = (scope) => `gewu.dismissed.v1:${JSON.stringify([scope.workspace, scope.session_id ?? scope.id])}`;
export const stateKey = (scope, instance, view) => `gewu.viewstate.v1:${JSON.stringify([scope.workspace, instance.pluginId, instance.bindingResource?.uri ?? '', view.id])}`;

/// The right-dock host: a Plugins manager plus one tab per open application
/// view. Server instances are the source of truth, so a reload attaches to a
/// live instance and never starts a stopped one.
export function panelHost(root, rpc, {onShow, onHide} = {}) {
  let scope, active = 'plugins', seq = 0, syncing = null, manager, managerSignature = '';
  let dismissed = new Set();
  const panels = new Map();
  const tabs = node('nav');
  tabs.setAttribute('aria-label', 'Application views');
  const body = node('div');
  body.className = 'plugin-body';
  root.replaceChildren(tabs, body);

  const fail = (target, error) => target.append(node('p', String(error.message ?? error)));
  const requestFor = (tab = scope?.id) => (action) => rpc(tab, action);
  const read = (key, fallback) => {
    try { return JSON.parse(localStorage.getItem(key)) ?? fallback; } catch { return fallback; }
  };
  const write = (key, value) => { try { localStorage.setItem(key, JSON.stringify(value)); } catch {} };

  function dismiss(instanceId, next) {
    if (!instanceId) return;
    if (next) dismissed.delete(instanceId); else dismissed.add(instanceId);
    if (scope) write(dismissedKey(scope), [...dismissed]);
  }

  function render() {
    tabs.replaceChildren();
    const tab = button('Plugins', () => { active = 'plugins'; render(); refresh(); });
    tab.setAttribute('aria-pressed', String(active === 'plugins'));
    tabs.append(tab);
    const changed = button('Changes', () => showChanges());
    changed.setAttribute('aria-pressed', String(active === 'changes'));
    tabs.append(changed);
    for (const [id, panel] of panels) {
      const item = button(panel.title, () => { active = id; render(); });
      item.setAttribute('aria-pressed', String(active === id));
      tabs.append(item);
      if (id === active) tabs.append(button('Close', () => close(id)));
      panel.content.hidden = id !== active;
    }
    if (manager) manager.hidden = active !== 'plugins';
  }

  function close(id) {
    const panel = panels.get(id);
    if (!panel) return;
    panels.delete(id);
    panel.dispose?.();
    panel.content.remove();
    dismiss(panel.instanceId, false);
    active = [...panels.keys()].at(-1) ?? 'plugins';
    render();
    if (active === 'plugins') refresh();
  }

  function add(id, title, instanceId, viewType) {
    const content = node('div');
    content.className = 'plugin-panel';
    content.hidden = true;
    const panel = {title, instanceId, viewType, content, dispose: null};
    panels.set(id, panel);
    body.append(content);
    return panel;
  }

  async function openView(instance, view, resource, {reveal = true} = {}) {
    const id = `${instance.id}/${view.id}`;
    if (panels.has(id)) { if (reveal) { active = id; render(); } return; }
    const request = requestFor();
    const panel = add(id, view.title, instance.id, view.id);
    let disposed = false, release = () => {};
    panel.dispose = () => { disposed = true; release(); };
    if (reveal) { active = id; render(); onShow?.(); }
    const state = read(stateKey(scope, instance, view), {});
    try {
      const binding = await request({action: 'attach', instanceId: instance.id, viewType: view.id, resource});
      const {viewId} = binding;
      const detach = () => request({action: 'detach', viewId}).catch(() => {});
      if (disposed) { detach(); return; }
      release = detach;
      if (view.kind === 'webview') {
        const frame = node('iframe');
        frame.title = view.title;
        panel.content.append(frame);
        const local = bindFrame(frame, binding.url, async () => {
          const context = await request({action: 'view_ready', viewId});
          if (!disposed) {
            panel.viewId = viewId;
            if (active === id) await request({action: 'focus_view', viewId});
          }
          return {...context, state};
        }, {
          getContext: (_, ctx) => ctx,
          callTool: ({tool, args}) => request({action: 'call', instanceId: instance.id, viewId, tool, args}),
          resolveResource: ({resource}) => request({action: 'resource', instanceId: instance.id, viewId, resource}),
          openResource: ({resource}) => { openResultResource(instance, resource); return {opened: true}; },
          publishContext: ({summary}) => request({action: 'publish_context', viewId, summary}),
          saveState: async ({state: next}) => { const saved = await request({action: 'save_state', viewId, state: next}); write(stateKey(scope, instance, view), saved); return saved; },
          requestService: ({serviceId, path, method}) => request({action: 'service', viewId, serviceId, path, method}),
          getCall: ({callId}) => request({action: 'call_status', callId}),
          cancelCall: ({callId}) => request({action: 'cancel', callId}),
        }, (error) => fail(panel.content, error));
        release = () => { local(); detach(); };
      } else if (view.renderer === 'diff') await changes(panel.content, request);
      else if (!resource) throw Error('Select a content resource before opening this view.');
      else if (view.renderer === 'text') await editor(panel.content, request, resource);
      else preview(panel.content, (await request({action: 'resource', instanceId: instance.id, resource})).url, resource);
    } catch (error) { if (!disposed) fail(panel.content, error); }
  }

  function openResultResource(instance, resource) {
    const declared = instance.views.find((v) => v.resourceTypes.includes(resource.type) && (!v.mimeTypes || v.mimeTypes.includes(resource.mimeType)));
    if (declared) { openView(instance, declared, resource); return; }
    const request = requestFor();
    const id = `preview-${++seq}`;
    const panel = add(id, resource.uri, null, null);
    active = id;
    render();
    onShow?.();
    request({action: 'resource', instanceId: instance.id, resource}).then(({url}) => preview(panel.content, url, resource)).catch((error) => fail(panel.content, error));
  }

  function editPath(path) {
    const id = `edit-${++seq}`;
    const resource = {uri: `workspace:///${path.split('/').map(encodeURIComponent).join('/')}`, type: 'text'};
    const panel = add(id, path, null, null);
    active = id;
    render();
    onShow?.();
    editor(panel.content, requestFor(), resource).catch((error) => fail(panel.content, error));
  }

  async function agentResult(resultId) {
    const request = requestFor(), key = scope?.id;
    const id = `result-${++seq}`;
    const panel = add(id, 'Application result', null, null);
    active = id;
    render();
    onShow?.();
    try {
      const {pluginId, bindingResource, result} = await request({action: 'agent_result', resultId});
      if (scope?.id !== key) return;
      panel.content.append(node('pre', JSON.stringify(result, null, 2)));
      for (const resource of result.resources ?? []) panel.content.append(button(resource.uri, async () => {
        try {
          const instance = await request({action: 'open', pluginId, bindingResource: bindingResource ?? undefined});
          if (scope?.id === key) openResultResource(instance, resource);
        } catch (error) { fail(panel.content, error); }
      }));
    } catch (error) { fail(panel.content, error); }
  }

  function showChanges() {
    let panel = panels.get('changes');
    if (!panel) {
      panel = add('changes', 'File changes', null, null);
      panel.dispose = null;
      panel.content.append(button('Refresh changes', () => loadChanges(panel)));
    }
    active = 'changes';
    render();
    loadChanges(panel);
  }

  async function loadChanges(panel) {
    panel.content.querySelector('section')?.remove();
    const target = node('section');
    panel.content.append(target);
    try { await changes(target, requestFor()); } catch (error) { fail(target, error); }
  }

  function primaryView(instance) {
    return instance.views?.find((v) => v.kind === 'webview' && v.placement === 'main')
      ?? instance.views?.find((v) => v.kind === 'webview');
  }

  /// Reconcile open tabs with the live instances. This only attaches and
  /// detaches; it never opens an instance.
  function sync() {
    if (!scope?.session_id || syncing) return syncing;
    const request = requestFor();
    const key = scope.id;
    syncing = (async () => {
      try {
        const {instances} = await request({action: 'list'});
        if (scope?.id !== key) return;
        const live = new Set(instances.map((i) => i.id));
        for (const [id, panel] of [...panels]) {
          if (panel.instanceId && !live.has(panel.instanceId)) {
            panels.delete(id);
            panel.dispose?.();
            panel.content.remove();
            if (active === id) active = 'plugins';
          }
        }
        for (const instance of instances) {
          const view = primaryView(instance);
          if (!view || dismissed.has(instance.id)) continue;
          if (panels.has(`${instance.id}/${view.id}`)) continue;
          await openView(instance, view, undefined, {reveal: true});
        }
        render();
        // Re-rendering the manager under the pointer steals clicks; only do it
        // when the set of open instances actually changed.
        const signature = instances.map((i) => i.id).join(',');
        if (active === 'plugins' && signature !== managerSignature) refresh();
      } catch {}
      finally { syncing = null; }
    })();
    return syncing;
  }

  async function openPlugin(plugin) {
    const request = requestFor();
    const resource = plugin.scope === 'resource'
      ? promptResource(`Workspace folder of the ${plugin.title ?? plugin.id} document`, plugin.views?.[0]?.resourceTypes?.[0] ?? 'document')
      : undefined;
    if (plugin.scope === 'resource' && !resource) return;
    const instance = await request({action: 'open', pluginId: plugin.id, bindingResource: resource});
    dismiss(instance.id, true);
    const view = primaryView(instance);
    if (view) await openView(instance, view);
    else { active = 'plugins'; render(); refresh(); }
  }

  async function refresh() {
    if (active !== 'plugins' || !scope) return;
    const request = requestFor();
    const key = scope.id;
    try {
      const [{plugins, instances}, calls] = await Promise.all([request({action: 'list'}), request({action: 'calls'})]);
      if (active !== 'plugins' || scope?.id !== key) return;
      const view = node('div');
      view.append(node('h3', 'Plugins'));
      for (const p of plugins) {
        const row = node('div');
        row.className = 'plugin-row';
        const name = node('span', `${p.title ?? p.id}`);
        if (p.version) name.append(node('code', ` v${p.version}`));
        row.append(name);
        row.append(node('span', !p.configured ? ' · installed, not configured' : !p.enabled ? ' · disabled' : ' · enabled'));
        if (p.configured) {
          row.append(button(p.enabled ? 'Disable' : 'Enable', async () => {
            try { await request({action: 'set_enabled', pluginId: p.id, enabled: !p.enabled}); await refresh(); }
            catch (error) { fail(view, error); }
          }));
          if (p.enabled) row.append(button('Open…', async () => {
            try { await openPlugin(p); } catch (error) { fail(view, error); }
          }));
        } else {
          if (p.package) row.append(node('code', p.package));
          row.append(node('span', ' add it to config.json to use it'));
        }
        view.append(row);
      }
      if (instances.length) view.append(node('h3', 'Open'));
      for (const instance of instances) {
        const row = node('div');
        row.className = 'plugin-row';
        row.append(node('span', instance.title ?? instance.pluginId));
        if (instance.bindingResource) row.append(node('code', instance.bindingResource.uri));
        for (const v of instance.views ?? []) {
          row.append(button(v.title, () => {
            const resource = v.kind !== 'webview' && v.renderer !== 'diff'
              ? promptResource(`Workspace content path for ${v.title}`, v.resourceTypes?.[0])
              : undefined;
            if (v.kind === 'webview' || v.renderer === 'diff' || resource) {
              openView(instance, v, resource).catch((error) => fail(view, error));
            }
          }));
        }
        row.append(button('Stop', async () => {
          try { await request({action: 'close', instanceId: instance.id}); await sync(); }
          catch (error) { fail(view, error); }
        }));
        view.append(row);
      }
      view.append(node('h3', 'Calls'));
      for (const call of calls) {
        const row = node('div');
        row.className = 'plugin-row';
        row.append(node('span', `${call.tool} · ${call.status}${call.progress ? ` · ${call.progress}` : ''}`));
        if (call.status === 'running') row.append(button('Cancel', async () => {
          try { await request({action: 'cancel', callId: call.id}); await refresh(); } catch (error) { fail(view, error); }
        }));
        else row.append(button('Result', async () => {
          try {
            const {result} = await request({action: 'call_status', callId: call.id});
            agentResultPanel(`Result · ${call.tool}`, result, instances.find((i) => i.id === call.instanceId));
          } catch (error) { fail(view, error); }
        }));
        const instance = instances.find((i) => i.id === call.instanceId);
        if (instance) for (const resource of call.resources ?? []) row.append(button(resource.uri, () => openResultResource(instance, resource)));
        view.append(row);
      }
      // Advanced: raw tool invocation for debugging and conformance runs.
      const tools = node('details');
      tools.append(node('summary', 'Tool calls (advanced)'));
      for (const instance of instances) for (const tool of instance.tools.filter((t) => t.target === 'backend')) {
        tools.append(button(tool.name, async () => {
          try {
            const text = window.prompt(`${tool.description}\nArguments (JSON object)`, '{}');
            if (text === null) return;
            await request({action: 'call', instanceId: instance.id, tool: tool.name, args: JSON.parse(text)});
            await refresh();
          } catch (error) { fail(view, error); }
        }));
      }
      view.append(tools);
      managerSignature = instances.map((i) => i.id).join(',');
      manager.replaceChildren(view);
    } catch (error) { manager.replaceChildren(node('p', error.message)); }
  }

  function agentResultPanel(title, result, instance) {
    const panel = add(`result-${++seq}`, title, null, null);
    panel.content.append(node('pre', JSON.stringify(result, null, 2)));
    if (instance) for (const resource of result.resources ?? []) panel.content.append(button(resource.uri, () => openResultResource(instance, resource)));
    active = [...panels.keys()].at(-1);
    render();
  }

  function promptResource(message, type = 'document') {
    const path = window.prompt(message)?.trim();
    return path ? {uri: `workspace:///${path.split('/').map(encodeURIComponent).join('/')}`, type} : undefined;
  }

  async function switchTab(next) {
    if (!next) return;
    if (scope && scope.id === next.id && scope.workspace === next.workspace && scope.session_id === next.session_id) return;
    scope = next;
    dismissed = new Set(read(dismissedKey(next), []));
    for (const panel of panels.values()) panel.dispose?.();
    panels.clear();
    body.replaceChildren();
    manager = node('div');
    manager.className = 'plugin-manager';
    body.append(manager);
    active = 'plugins';
    render();
    refresh();
  }

  return {switchTab, sync, editPath, agentResult, setVisible: (on) => { if (on) sync(); }};
}
