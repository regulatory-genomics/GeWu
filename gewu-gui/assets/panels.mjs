import {bindFrame} from './plugin-bridge.mjs';
import {button, changes, editor, node, preview} from './editor.mjs';

export const dismissedKey = (scope) => `gewu.dismissed.v1:${JSON.stringify([scope.workspace, scope.session_id ?? scope.id])}`;
export const stateKey = (scope, instance, view) => `gewu.viewstate.v2:${JSON.stringify([scope.workspace, scope.session_id ?? scope.id, instance.pluginId, instance.bindingResource?.uri ?? '', view.id])}`;

export function runtimeOptionLabel(provider) {
  if (provider.kind === 'auto') return 'Auto · first compatible option';
  const kind = provider.kind ? provider.kind[0].toUpperCase() + provider.kind.slice(1) : 'Runtime';
  const name = provider.manager ?? provider.id ?? provider.kind;
  const detail = provider.detail ? ` · ${provider.detail}` : '';
  return `${kind} · ${name}${detail}`;
}

export function marketplaceStatus(release, installed = {}, catalog = []) {
  const state = installed[release.plugin];
  if (state?.activeGeneration === release.releaseId) {
    return state.runtime?.kind ? `Installed · ${state.runtime.kind}` : 'Package installed · runtime not verified';
  }
  const local = catalog.find((plugin) => plugin.id === release.plugin && plugin.version === release.version);
  return local ? `Installed locally · ${release.version}` : '';
}

/// The right-dock host: a Plugins manager plus one tab per open application
/// view. Server instances are the source of truth, so a reload attaches to a
/// live instance and never starts a stopped one.
export function panelHost(root, rpc, {onShow, onHide} = {}) {
  let scope, active = 'plugins', seq = 0, syncing = null, manager, changesView, marketplaceView, managerSignature = '';
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
    const market = button('Marketplace', () => showMarketplace());
    market.setAttribute('aria-pressed', String(active === 'marketplace'));
    tabs.append(market);
    for (const [id, panel] of panels) {
      const item = button(panel.title, () => { active = id; render(); });
      item.setAttribute('aria-pressed', String(active === id));
      tabs.append(item);
      if (id === active) {
        if (panel.reload) tabs.append(button('Reload view', panel.reload));
        tabs.append(button('Close', () => close(id)));
      }
      panel.content.hidden = id !== active;
    }
    if (manager) manager.hidden = active !== 'plugins';
    if (changesView) changesView.hidden = active !== 'changes';
    if (marketplaceView) marketplaceView.hidden = active !== 'marketplace';
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
    panel.dispose = () => {
      if (disposed) return;
      disposed = true;
      return release();
    };
    if (view.kind === 'webview') panel.reload = async () => {
      panel.reload = null;
      render();
      await panel.dispose();
      if (panels.get(id) !== panel) return;
      panels.delete(id);
      panel.content.remove();
      await openView(instance, view, resource, {reveal: active === id});
      render();
    };
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
          downloadResource: async ({resource}) => {
            const {url}=await request({action:'resource',instanceId:instance.id,viewId,resource});
            const result=await fetch(url);if(!result.ok)throw Error('Artifact download failed');
            const object=URL.createObjectURL(await result.blob()),link=document.createElement('a');link.href=object;link.download=resource.uri.split('/').at(-1)||'artifact';document.body.append(link);link.click();link.remove();setTimeout(()=>URL.revokeObjectURL(object),30000);return {downloaded:true};
          },
          publishContext: ({summary}) => request({action: 'publish_context', viewId, summary}),
          saveState: async ({state: next}) => { const saved = await request({action: 'save_state', viewId, state: next}); write(stateKey(scope, instance, view), saved); return saved; },
          requestService: ({serviceId, path, method}) => request({action: 'service', viewId, serviceId, path, method}),
          getCall: ({callId}) => request({action: 'call_status', callId}),
          cancelCall: ({callId}) => request({action: 'cancel', callId}),
          maximize: () => {const maximized=root.classList.toggle('plugin-workspace-maximized');return {maximized};},
          pickFiles: ({accept,multiple}) => new Promise((resolve,reject)=>{
            if(typeof accept!=='string'||accept.length>80||!/^[.a-zA-Z0-9,]+$/.test(accept)){reject(Error('Invalid file type filter'));return;}
            const input=document.createElement('input');input.type='file';input.accept=accept;input.multiple=!!multiple;input.hidden=true;root.append(input);
            input.oncancel=()=>{input.remove();resolve({files:[]});};
            input.onchange=async()=>{
              try{
                const chosen=[...input.files];if(chosen.length>8||chosen.some(f=>f.size>32*1024*1024))throw Error('Choose at most 8 files, each no larger than 32 MiB');
                const files=await Promise.all(chosen.map(file=>new Promise((done,fail)=>{const reader=new FileReader();reader.onerror=()=>fail(Error('Could not read file'));reader.onload=()=>done({name:file.name,base64:reader.result.split(',')[1]});reader.readAsDataURL(file);})));resolve({files});
              }catch(error){reject(error);}finally{input.remove();}
            };
            input.click();
          }),
        }, (error) => fail(panel.content, error));
        release = () => { local(); return detach(); };
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
    if (!changesView) return;
    active = 'changes';
    render();
    loadChanges();
  }

  function showMarketplace() {
    if (!marketplaceView) return;
    active = 'marketplace';
    render();
    loadMarketplace();
  }

  /// The signed catalog is rendered by the host, not by a plugin webview, so
  /// installation always goes through the same verified InstallManager path.
  async function loadMarketplace(query = '') {
    if (!marketplaceView) return;
    const request = requestFor();
    const pane = node('div');
    pane.className = 'plugin-marketplace';
    const bar = node('div');
    const search = node('input');
    search.placeholder = 'Search plugins';
    search.value = query;
    bar.append(search, button('Search', () => loadMarketplace(search.value.trim())));
    bar.append(button('Doctor', async () => {
      try {
        const state = await request({action: 'marketplace_doctor'});
        pane.append(node('pre', JSON.stringify(state, null, 2)));
      } catch (error) { fail(pane, error); }
    }));
    pane.append(bar);
    const list = node('div');
    list.append(node('p', 'Loading signed catalog…'));
    pane.append(list);
    marketplaceView.replaceChildren(pane);
    try {
      const [data, inventory] = await Promise.all([
        request({action: 'marketplace_search', query, refresh: true}),
        request({action: 'list'}).catch(() => ({plugins: []})),
      ]);
      const installed = data.installed ?? {};
      const catalog = inventory.plugins ?? [];
      list.replaceChildren();
      list.append(node('p', `${data.releases.length} release(s) · ${data.repository} · expires ${data.expiresAt}`));
      list.append(node('p', `Catalog: ${data.indexUrl}`));
      if (!data.releases.length) list.append(node('p', 'No matching release in the signed catalog.'));
      for (const release of data.releases) {
        const row = node('div');
        row.className = 'plugin-row';
        row.append(node('span', `${release.plugin} ${release.version} (${release.channel})`));
        if (release.releaseNotes) row.append(node('p', release.releaseNotes));
        const statusText = marketplaceStatus(release, installed, catalog);
        const state = installed[release.plugin];
        const packageInstalled = state?.activeGeneration === release.releaseId
          || catalog.some((plugin) => plugin.id === release.plugin && plugin.version === release.version);
        const runtimeReady = state?.activeGeneration === release.releaseId && Boolean(state.runtime?.kind);
        if (statusText) row.append(node('strong', statusText));

        const runtimePlan = release.runtimePlan?.providers ?? [];
        const runtimeOptions = release.runtimeOptions ?? [];
        const runtimeInfo = new Map(runtimeOptions.map((option) => [`${option.kind}:${option.id}`, option]));
        const runtimeSelect = node('select');
        runtimeSelect.setAttribute('aria-label', `Runtime for ${release.plugin}`);
        const auto = node('option', runtimeOptionLabel({kind: 'auto'}));
        auto.value = 'auto';
        auto.disabled = runtimeOptions.length > 0 && !runtimeOptions.some((option) => option.available);
        runtimeSelect.append(auto);
        for (const provider of runtimePlan) {
          const info = runtimeInfo.get(`${provider.kind}:${provider.id}`);
          const option = node('option', runtimeOptionLabel({...provider, ...(info ?? {})}));
          option.value = provider.kind;
          option.disabled = info?.available === false;
          runtimeSelect.append(option);
        }
        if (!runtimePlan.length) {
          for (const runtime of release.runtimes ?? []) {
            const option = node('option', runtimeOptionLabel({kind: 'bundled', id: runtime.id, detail: `${Math.round(runtime.artifact.bytes / 1e6)} MB`}));
            option.value = 'bundled';
            runtimeSelect.append(option);
          }
        }
        row.append(runtimeSelect);

        const status = node('p', '');
        status.setAttribute('role', 'status');
        const install = button(runtimeReady ? 'Installed' : packageInstalled ? 'Install runtime' : 'Install', async () => {
          const policy = runtimeSelect.value;
          install.disabled = true;
          runtimeSelect.disabled = true;
          status.textContent = `Installing via ${runtimeOptionLabel({kind: policy})}… large runtimes may take several minutes.`;
          try {
            const result = await request({action: 'marketplace_install', pluginId: release.plugin, version: release.version, channel: release.channel, runtime: policy});
            const selected = result.runtime?.kind ? ` via ${result.runtime.kind}` : '';
            status.textContent = `Installed ${result.version}${selected}. Restart or reopen the plugin to use it.`;
            install.textContent = 'Installed';
          } catch (error) {
            status.textContent = `Install failed: ${error.message}`;
            install.disabled = false;
            runtimeSelect.disabled = false;
          }
        });
        install.disabled = runtimeReady;
        row.append(install, status);
        const rollback = button('Rollback', async () => {
          try {
            const result = await request({action: 'marketplace_rollback', pluginId: release.plugin});
            status.textContent = `Rolled back to ${result.release}.`;
          } catch (error) { status.textContent = `Rollback failed: ${error.message}`; }
        });
        rollback.disabled = state?.state !== 'installed';
        row.append(rollback);
        list.append(row);
      }
    } catch (error) { list.replaceChildren(); fail(list, error); }
  }

  async function loadChanges() {
    const pane = node('div');
    const bar = node('div');
    bar.append(button('Refresh', () => loadChanges()));
    pane.append(bar);
    const list = node('div');
    pane.append(list);
    changesView.replaceChildren(pane);
    try { await changes(list, requestFor()); } catch (error) { fail(pane, error); }
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
          // Reattaching a live app must not steal the file or result the user is viewing.
          const reveal = active !== 'changes' && (!panels.has(active) || !!panels.get(active).instanceId);
          await openView(instance, view, undefined, {reveal});
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
      const {plugins, instances} = await request({action: 'list'});
      if (active !== 'plugins' || scope?.id !== key) return;
      const view = node('div');
      view.append(node('h3', 'Plugins'));
      for (const plugin of plugins) {
        const running = instances.filter((instance) => instance.pluginId === plugin.id);
        const session = plugin.session ?? {override: null, enabled: !!plugin.enabled, profiles: ['workspace-basic', 'analysis-read']};
        const effective = !!session.enabled;
        const row = node('div');
        row.className = 'plugin-row';
        const head = node('div');
        head.className = 'plugin-head';
        const name = node('strong', `${plugin.title ?? plugin.id}`);
        if (plugin.version) name.append(node('code', ` v${plugin.version}`));
        head.append(name, node('span', pluginStatus(plugin, session)));
        row.append(head);
        if (plugin.available === false && plugin.reason) row.append(node('p', plugin.reason));
        const advanced = node('details');
        advanced.className = 'plugin-advanced';
        advanced.append(node('summary', 'Advanced'));
        const tools = [...new Map(running.flatMap(instance => instance.tools ?? []).map(tool => [tool.name, tool])).values()];
        const details = node('details');
        details.className = 'plugin-tools';
        details.append(node('summary', tools.length ? `Tools (${tools.length})` : 'Tools'));
        if (!tools.length) details.append(node('p', 'Open the plugin to discover its available tools.'));
        for (const tool of tools) {
          const item = node('details');
          item.append(node('summary', `${tool.name} · ${tool.group ?? 'workspace'} · ${tool.effect}`));
          item.append(node('p', tool.description));
          item.append(node('pre', JSON.stringify(tool.inputSchema, null, 2)));
          details.append(item);
        }
        advanced.append(details);
        if (plugin.package || Object.keys(plugin.sources ?? {}).length) {
          const source = node('details');
          source.className = 'plugin-tools';
          source.append(node('summary', 'Source & package'));
          for (const [label, path] of Object.entries({package: plugin.package, ...plugin.sources})) {
            if (!path) continue;
            const line = node('p', `${label}: `);
            line.append(node('code', path));
            source.append(line);
          }
          advanced.append(source);
        }
        row.append(advanced);
        if (!plugin.configured) {
          if (plugin.package) row.append(node('code', plugin.package));
          row.append(node('span', ' add it to config.json to use it'));
          view.append(row);
          continue;
        }
        const actions = node('div');
        actions.className = 'plugin-actions';
        actions.append(button(plugin.enabled ? 'Disable globally' : 'Enable globally', async () => {
          try { await request({action: 'set_enabled', pluginId: plugin.id, enabled: !plugin.enabled}); await refresh(); }
          catch (error) { fail(view, error); }
        }));
        if (session.override === null) {
          actions.append(button(effective ? 'Disable in this session' : 'Enable in this session', async () => {
            try {
              await request({action: 'set_session', pluginId: plugin.id, enabled: !effective, profiles: session.profiles});
              await refresh();
            } catch (error) { fail(view, error); }
          }));
        } else {
          actions.append(button('Follow global setting', async () => {
            try {
              await request({action: 'set_session', pluginId: plugin.id, enabled: null, profiles: session.profiles});
              await refresh();
            } catch (error) { fail(view, error); }
          }));
        }
        if (effective) actions.append(button('Open\u2026', async () => {
          try { await openPlugin(plugin); } catch (error) { fail(view, error); }
        }));
        row.append(actions);
        if (effective && (plugin.id === 'tachyon' || plugin.scope === 'session')) {
          const profiles = node('span');
          profiles.className = 'plugin-profiles';
          for (const profile of ['workflow-authoring', 'references']) {
            const label = node('label');
            const input = node('input');
            input.type = 'checkbox';
            input.checked = session.profiles.includes(profile);
            input.addEventListener('change', async () => {
              const next = input.checked
                ? [...new Set([...session.profiles, profile])]
                : session.profiles.filter((item) => item !== profile);
              input.disabled = true;
              try {
                await request({action: 'set_session', pluginId: plugin.id, enabled: session.override, profiles: next});
                await refresh();
              } catch (error) {
                input.disabled = false;
                input.checked = !input.checked;
                fail(view, error);
              }
            });
            label.append(input, node('span', profile));
            profiles.append(label);
          }
          advanced.append(profiles);
        }
        view.append(row);
        for (const instance of running) view.append(runningRow(instance, request, view));
      }
      managerSignature = instances.map((instance) => instance.id).join(',');
      manager.replaceChildren(view);
    } catch (error) { manager.replaceChildren(node('p', error.message)); }
  }

  function pluginStatus(plugin, session) {
    if (!plugin.configured) return ' \u00b7 installed, not configured';
    if (plugin.available === false) return ' \u00b7 package unavailable';
    if (session.override === false) return ' \u00b7 disabled in this session';
    if (session.override === true) {
      return plugin.enabled ? ' \u00b7 enabled in this session' : ' \u00b7 enabled in this session (globally off)';
    }
    return plugin.enabled ? ' \u00b7 enabled' : ' \u00b7 globally disabled';
  }

  /// One compact line per running instance: where it is bound, its extra views,
  /// and Stop. The main view is already the tab above.
  function runningRow(instance, request, view) {
    const row = node('div');
    row.className = 'plugin-sub';
    if (instance.bindingResource) row.append(node('code', instance.bindingResource.uri));
    const primary = primaryView(instance);
    for (const item of instance.views ?? []) {
      if (primary && item.id === primary.id) continue;
      row.append(button(item.title, () => {
        const resource = item.kind !== 'webview' && item.renderer !== 'diff'
          ? promptResource(`Workspace content path for ${item.title}`, item.resourceTypes?.[0])
          : undefined;
        if (item.kind === 'webview' || item.renderer === 'diff' || resource) {
          openView(instance, item, resource).catch((error) => fail(view, error));
        }
      }));
    }
    row.append(button('Stop', async () => {
      try { await request({action: 'close', instanceId: instance.id}); await sync(); }
      catch (error) { fail(view, error); }
    }));
    return row;
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
    changesView = node('div');
    changesView.className = 'plugin-panel';
    changesView.hidden = true;
    marketplaceView = node('div');
    marketplaceView.className = 'plugin-panel';
    marketplaceView.hidden = true;
    body.append(manager, changesView, marketplaceView);
    active = 'plugins';
    render();
    refresh();
  }

  /// Show a workspace file in the Preview sidecar: text files keep the
  /// server-rendered preview, media renders its own element. Files clicks use
  /// this instead of a full-screen overlay.
  function showFile(kind, path, payload) {
    const id = `file:${path}`;
    let panel = panels.get(id);
    if (!panel) {
      panel = add(id, path.split('/').at(-1) || path, null, null);
    }
    panel.content.replaceChildren();
    if (kind === 'text') {
      panel.content.innerHTML = payload.html;
      for (const close of panel.content.querySelectorAll('[data-preview-close]')) {
        close.addEventListener('click', () => close(id));
      }
    } else {
      const media = document.createElement(
        kind === 'image' ? 'img'
          : kind === 'video' ? 'video'
          : kind === 'audio' ? 'audio'
          : 'iframe');
      if (kind === 'image') media.alt = path;
      if (kind === 'video' || kind === 'audio') media.controls = true;
      if (kind === 'pdf') media.title = path;
      media.src = payload.url;
      media.referrerPolicy = 'no-referrer';
      media.className = 'preview-media';
      panel.content.append(media);
    }
    active = id;
    render();
    onShow?.();
  }

  return {switchTab, sync, editPath, agentResult, showFile, setVisible: (on) => { if (on) sync(); }};
}
