import {bindFrame} from './plugin-bridge.mjs';
import {bindAppFrame} from './plugin-app-bridge.mjs';
import {button, changes, editor, node, preview} from './editor.mjs';

export const dismissedKey = (scope) => `gewu.dismissed.v1:${JSON.stringify([scope.workspace, scope.session_id ?? scope.id])}`;
export const stateKey = (scope, instance, view) => `gewu.viewstate.v2:${JSON.stringify([scope.workspace, scope.session_id ?? scope.id, instance.pluginId, instance.bindingResource?.uri ?? '', view.id])}`;

function installAttemptId() {
  // getRandomValues is available on HTTP LAN deployments too.
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 15) | 64;
  bytes[8] = (bytes[8] & 63) | 128;
  const hex = Array.from(bytes, (n) => n.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

export function runtimeOptionLabel(provider) {
  if (provider.kind === 'auto') return 'Auto · first compatible option';
  const kind = provider.kind ? provider.kind[0].toUpperCase() + provider.kind.slice(1) : 'Runtime';
  const name = provider.manager ?? provider.id ?? provider.kind;
  const detail = provider.detail ? ` · ${provider.detail}` : '';
  return `${kind} · ${name}${detail}`;
}

function brief(value, limit = 280) {
  const line = String(value ?? '').split('\n').find((item) => item.trim())?.trim() ?? '';
  return line.length > limit ? `${line.slice(0, limit - 1)}…` : line;
}

export function installInstructions(release) {
  const policies = release.runtimePlan?.policies ?? ['bundled'];
  return `# Install ${release.plugin} ${release.version} (${release.channel})

Run on the GeWu host, as the user who runs GeWu. Use the official signed Marketplace at https://plugin.regulogen.cn. Preserve existing documents and working installations.

gewu plugin marketplace-install ${release.plugin} --version ${release.version} --channel ${release.channel} --runtime auto

Runtime order: ${policies.join(' → ')}. Auto follows this release's signed plan, probes existing environments when offered, and falls back to the next provider if unavailable. Let the installer choose; do not create a new environment or build dependencies before trying Auto. To reuse a specific existing interpreter, add --executable /absolute/path/to/python to the command above.

After installation:
1. Run gewu plugin doctor ${release.plugin}. Read the installed plugin.json, then run gewu plugin check ${release.plugin} --version ${release.version} --probe --workspace /absolute/path/to/workspace. Supply --session for session-scoped plugins, --resource and --resource-type for resource-scoped plugins, and --settings if required by the manifest.
2. Restart a running GeWu after CLI installation. Open the plugin and complete one real operation; verify the resulting page, document, render or export is usable.
3. Report the installed version, selected runtime/interpreter and verification result. If installation fails, read the install-report for the attempt ID printed by the command and report the specific blocker.
`;
}

export function sourceInstallInstructions(release) {
  const provenance = release.provenance ?? {};
  const sources = Object.fromEntries((provenance.buildInputs ?? []).map(entry => {
    const at = entry.indexOf('=');
    return at < 0 ? [entry, ''] : [entry.slice(0, at), entry.slice(at + 1)];
  }));
  const source = value => {
    try {
      const url = new URL(value?.replace(/^git@github.com:/, 'https://github.com/'));
      if (['https:', 'ssh:'].includes(url.protocol) && !url.password && !url.search && !url.hash
          && (!url.username || (url.protocol === 'ssh:' && url.username === 'git'))) return url.href;
    } catch {}
    return 'not supplied';
  };
  return `# Install ${release.plugin} ${release.version} (${release.channel}) from source

Run on the GeWu host, as the user who runs GeWu. Build the plugin adapter from the pinned source. Reuse a compatible application runtime when available; create an isolated environment only for missing dependencies. Preserve existing documents and working installations. Do not compile Python, Chromium or other application dependencies from source unless explicitly requested or required by the recipe.

Release: ${release.releaseId ?? 'not supplied'}
Repository: ${source(provenance.source)}
Original package source commit: ${provenance.sourceCommit ?? 'not supplied'}
Published package SHA-256 (reference, not the expected hash of a local build): ${release.package?.sha256 ?? 'not supplied'}
Application source: ${source(sources['application-source'])}
Plugin adapter source: ${source(sources['plugin-source'])}
Runtime recipe: ${source(sources['runtime-recipe'])}
Installation guide: ${source(sources['installation-guide'])}

1. Read the guide and use the immutable revision in each source link. A published snapshot's SOURCE.json records its original package source commit above; that original commit need not belong to the snapshot repository. Verify that the adapter manifest matches ${release.plugin} ${release.version}. If exact sources are missing or describe a different version, report the blocker rather than installing a newer plugin.
2. Follow the guide's compatibility check to select an existing runtime. If none works, prepare only the missing dependencies in an isolated environment. Use the documented builder/staging command and the SDK supplied by that revision or the GeWu SDK path specified in the guide. A Python interpreter alone is not a native application runtime.
3. Use the guide's local-source staging option to bind the selected runtime into the staged launcher, then register it with: gewu plugin install /absolute/path/to/staged-package. This command copies the package without executing it or acquiring a runtime; setting an arbitrary host environment variable alone is insufficient. If the guide supplies no supported binding path, report the blocker rather than silently installing a different package from Marketplace.
4. Run gewu plugin check for ${release.plugin} with --version ${release.version}, --probe and --workspace /absolute/path/to/workspace. Read plugin.json first: supply --resource and --resource-type for a resource-scoped plugin, and --settings if required. Use the paths and resource types declared by that revision, not invented defaults.
5. Restart GeWu after CLI installation, open the plugin, and complete one real operation. Confirm the resulting document, render or export is usable. Report the source revision, commands and verification result.
`;
}

const CHANNEL_RANK = {stable: 3, beta: 2, nightly: 1};

function compareVersions(left, right) {
  const split = version => version.split('+')[0].split(/-(.*)/s);
  const [a, preA] = split(left), [b, preB] = split(right);
  const coreA = a.split('.'), coreB = b.split('.');
  for (let i = 0; i < 3; i++) {
    const difference = BigInt(coreA[i]) - BigInt(coreB[i]);
    if (difference) return difference > 0n ? 1 : -1;
  }
  if (preA === undefined) return preB === undefined ? 0 : 1;
  if (preB === undefined) return -1;
  const partsA = preA.split('.'), partsB = preB.split('.');
  for (let i = 0; i < Math.max(partsA.length, partsB.length); i++) {
    if (partsA[i] === undefined) return -1;
    if (partsB[i] === undefined) return 1;
    if (partsA[i] === partsB[i]) continue;
    const numericA = /^\d+$/.test(partsA[i]), numericB = /^\d+$/.test(partsB[i]);
    if (numericA && numericB) {
      const difference = BigInt(partsA[i]) - BigInt(partsB[i]);
      if (difference) return difference > 0n ? 1 : -1;
    } else return numericA !== numericB ? (numericA ? -1 : 1) : (partsA[i] > partsB[i] ? 1 : -1);
  }
  return 0;
}

export function latestReleases(releases = []) {
  const selected = new Map();
  const usable = (r) => r.installCompatible !== false && !r.revocation;
  for (const release of releases) {
    const current = selected.get(release.plugin);
    if (current && usable(release) !== usable(current)) {
      if (usable(release)) selected.set(release.plugin, release);
      continue;
    }
    const channel = current ? (CHANNEL_RANK[release.channel] ?? 0) - (CHANNEL_RANK[current.channel] ?? 0) : 1;
    if (!current || channel > 0 || (channel === 0 && compareVersions(release.version, current.version) > 0)) {
      selected.set(release.plugin, release);
    }
  }
  return [...selected.values()];
}

export function marketplaceStatus(release, installed = {}, catalog = []) {
  const state = installed[release.plugin];
  if (state?.activeGeneration === release.releaseId) {
    return state.runtime?.kind ? `Installed · ${state.runtime.kind}` : 'Package installed · runtime not verified';
  }
  const local = catalog.find((plugin) => plugin.id === release.plugin && plugin.version === release.version);
  return local ? `Installed locally · ${release.version}` : '';
}

function runtimeDescription(runtime) {
  if (runtime?.kind === 'external') return `Using existing environment · ${runtime.executable}`;
  if (runtime?.kind === 'managed') return `Using managed environment · ${runtime.prefix}`;
  if (runtime?.kind === 'bundled') return `Using bundled runtime · ${runtime.provider}`;
  return '';
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
    if (view.kind === 'webview' || view.kind === 'webapp') panel.reload = async () => {
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
      if (view.kind === 'webview' || view.kind === 'webapp') {
        const frame = node('iframe');
        frame.title = view.title;
        panel.content.append(frame);
        const bind = view.kind === 'webapp' ? bindAppFrame : bindFrame;
        const renewal = view.kind === 'webapp' ? setInterval(() => request({action:'renew_view',viewId}).catch(error=>fail(panel.content,error)),60000) : null;
        const local = bind(frame, view.kind === 'webapp' ? binding : binding.url, async () => {
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
            const link=document.createElement('a');link.href=url;link.download=resource.uri.split('/').at(-1)||'artifact';document.body.append(link);link.click();link.remove();return {downloaded:true};
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
        release = () => { clearInterval(renewal); local(); return detach(); };
      } else if (view.renderer === 'diff') await changes(panel.content, request);
      else if (!resource) throw Error('Select a content resource before opening this view.');
      else if (view.renderer === 'text') await editor(panel.content, request, resource);
      else {
        const resolveUrl = async () => (await request({action:'resource',instanceId:instance.id,resource})).url;
        preview(panel.content,await resolveUrl(),resource,resolveUrl);
      }
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
    const resolveUrl = async () => (await request({action:'resource',instanceId:instance.id,resource})).url;
    resolveUrl().then(url => preview(panel.content,url,resource,resolveUrl)).catch((error) => fail(panel.content,error));
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
    bar.className = 'marketplace-bar';
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
    list.className = 'marketplace-list';
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
      const latest = latestReleases(data.releases.map(release => ({...release,
        installCompatible: release.installCompatible !== false &&
          !(data.appOriginReady === false && release.requiredFeatures?.includes('webapp-origin')),
      })));
      list.replaceChildren();
      const catalogMeta = node('p', `${latest.length} plugin(s) · latest release · ${data.releases.length} release(s) in catalog · ${data.repository} · expires ${data.expiresAt}`);
      catalogMeta.className = 'catalog-meta';
      const catalogUrl = node('p', `Catalog: ${data.indexUrl}`);
      catalogUrl.className = 'catalog-meta';
      const latestNote = node('p', 'Older signed releases remain in the catalog for rollback and direct links.');
      latestNote.className = 'catalog-meta';
      list.append(catalogMeta, catalogUrl, latestNote);
      if (!latest.length) list.append(node('p', 'No matching release in the signed catalog.'));
      for (const release of latest) {
        const row = node('div');
        row.className = 'plugin-row marketplace-card';
        const statusText = marketplaceStatus(release, installed, catalog);
        const state = installed[release.plugin];
        let selectedRuntime = state?.runtime;
        const packageInstalled = state?.activeGeneration === release.releaseId
          || catalog.some((plugin) => plugin.id === release.plugin && plugin.version === release.version);
        const runtimeReady = state?.activeGeneration === release.releaseId && Boolean(state.runtime?.kind);
        row.classList.toggle('is-installed', runtimeReady);

        const head = node('div');
        head.className = 'plugin-card-head';
        const title = node('div');
        title.className = 'plugin-card-title';
        title.append(node('strong', release.plugin), node('span', ` ${release.version}`), node('span', ` ${release.channel}`));
        const badge = node('span', statusText || 'Available');
        badge.className = `plugin-badge${runtimeReady ? ' is-installed' : ''}`;
        head.append(title, badge);
        row.append(head);
        if (release.releaseNotes) {
          const description = node('p', brief(release.releaseNotes));
          description.className = 'plugin-card-description';
          description.title = release.releaseNotes;
          row.append(description);
        }

        const runtimeOptions = release.runtimeOptions ?? [];
        const prebuilt = release.schemaVersion === 'gewu.release/v3';
        const blocker = prebuilt && release.requiredFeatures?.includes('webapp-origin') && data.appOriginReady === false
          ? 'The GeWu GUI needs an app origin configured by the host administrator.'
          : prebuilt && !runtimeOptions.some(option => option.available && (option.profile ?? 'core') === 'core')
            ? 'This release requires a different GeWu version or execution platform.' : '';
        const runtimeSelect = node('select');
        runtimeSelect.setAttribute('aria-label', `Runtime for ${release.plugin}`);
        const auto = node('option', runtimeOptionLabel({kind: 'auto'}));
        auto.value = 'auto';
        runtimeSelect.append(auto);
        for (const provider of runtimeOptions) {
          const option = node('option', runtimeOptionLabel(provider));
          option.value = provider.kind;
          option.disabled = provider.available === false;
          runtimeSelect.append(option);
        }
        const advanced = node('details');
        advanced.className = 'plugin-card-advanced';
        advanced.append(node('summary', 'Advanced'));
        if (!prebuilt) {
          advanced.append(runtimeSelect);
          advanced.append(node('p', `Auto order: ${(release.runtimePlan?.policies ?? ['bundled']).join(' → ')}. To use a specific interpreter, add --executable /absolute/path/to/python to the copied install command.`));
        }
        for (const option of runtimeOptions.filter(option => !option.available)) advanced.append(node('p', runtimeOptionLabel(option)));

        const actions = node('div');
        actions.className = 'plugin-card-actions';
        const status = node('p', '');
        status.className = 'plugin-card-status';
        status.setAttribute('role', 'status');
        const runtimeInfo = node('p', runtimeDescription(selectedRuntime));
        runtimeInfo.className = 'plugin-card-runtime';
        const next = node('div');
        next.className = 'plugin-card-next';
        const details = node('div');
        details.className = 'plugin-card-details';
        const setStatus = (text, kind = '') => {
          status.textContent = text;
          if (kind) status.dataset.kind = kind;
          else delete status.dataset.kind;
        };
        const setBadge = (text, kind = '') => {
          badge.textContent = text;
          badge.className = `plugin-badge${kind ? ` is-${kind}` : ''}`;
        };
        const showOpen = (plugin) => {
          next.replaceChildren();
          row.classList.toggle('is-warn', plugin?.settingsValid === false);
          if (!plugin || plugin.version !== release.version || !plugin.configured || plugin.available === false) {
            setStatus('This release is not active in Plugins', 'warn');
            setBadge('Installed', 'muted');
          } else if (plugin.settingsValid === false) {
            setStatus(`Needs setup: ${plugin.settingsError ?? 'check settings'}`, 'warn');
            setBadge('Needs setup', 'warn');
          } else if (!(plugin.session?.enabled ?? plugin.enabled)) {
            setStatus('Installed but disabled in this session', 'muted');
            setBadge('Disabled', 'muted');
          } else {
            setStatus('Ready to open', 'ok');
            setBadge('Installed', 'installed');
            const open = button('Open…', async () => {
              try { await openPlugin(plugin, request); }
              catch (error) { fail(details, error); }
            });
            open.className = 'primary';
            next.append(open);
          }
        };
        const showInstalled = async () => {
          install.textContent = 'Installed';
          install.className = 'installed';
          install.disabled = true;
          install.hidden = true;
          runtimeSelect.disabled = true;
          row.classList.remove('is-installing');
          row.classList.add('is-installed');
          try {
            if (!selectedRuntime) {
              const saved = await request({action: 'marketplace_doctor'});
              const plugin = saved[release.plugin];
              selectedRuntime = plugin?.generations?.[plugin.activeGeneration]?.runtime;
            }
            runtimeInfo.textContent = runtimeDescription(selectedRuntime);
            const {plugins} = await request({action: 'list'});
            showOpen(plugins.find(p => p.id === release.plugin));
          } catch (error) {
            setStatus(`Could not refresh Plugins: ${error.message}`, 'error');
            next.replaceChildren(button('Refresh status', showInstalled));
          }
        };
        const install = button(runtimeReady ? 'Installed' : packageInstalled ? 'Install runtime' : 'Install', async () => {
          const policy = runtimeSelect.value;
          const attemptId = installAttemptId();
          let report;
          next.replaceChildren();
          details.replaceChildren();
          install.disabled = true;
          install.hidden = false;
          runtimeSelect.disabled = true;
          row.classList.remove('is-installed');
          row.classList.add('is-installing');
          setStatus(`Installing via ${runtimeOptionLabel({kind: policy})}… large runtimes may take several minutes.`, 'progress');
          setBadge('Installing…', 'progress');
          const stages = {release_selection: 'Checking compatibility', runtime_selection: 'Selecting runtime',
            package_download: 'Downloading plugin', runtime_download: 'Downloading runtime', extract: 'Unpacking',
            runtime_preparation: 'Preparing runtime', runtime_probe: 'Checking plugin operation',
            commit: 'Activating', registry_refresh: 'Refreshing Plugins'};
          let polling = false;
          let finished = false;
          const statusTimer = setInterval(async () => {
            if (polling || finished) return;
            polling = true;
            try {
              const {report: current} = await request({action: 'marketplace_install_report', attemptId});
              const label = stages[current?.stage];
              if (!finished && label) setStatus(`${label}…`, 'progress');
            } catch { /* The report may not exist yet. */ }
            finally { polling = false; }
          }, 1500);
          try {
            const result = await request({action: 'marketplace_install', pluginId: release.plugin, version: release.version, channel: release.channel, runtime: policy, attemptId});
            selectedRuntime = result.runtime;
            finished = true;
            await showInstalled();
          } catch (error) {
            finished = true;
            const read = () => request({action: 'marketplace_install_report', attemptId});
            const describe = async () => {
              try { report = await read(); } catch { report = undefined; }
              const {outcome, stage, error: cause} = report?.report ?? {};
              if (outcome === 'succeeded') return showInstalled();
              if (outcome === 'running' || (!outcome && /timed out|Disconnected/i.test(error.message))) {
                setStatus(outcome === 'running' ? `Still installing · ${stage}` : 'Installation status unknown', 'progress');
                setBadge('Installing…', 'progress');
                next.replaceChildren(button('Check status', describe));
                return;
              }
              setStatus(`${outcome === 'interrupted' ? 'Install interrupted' : 'Install failed'}${stage ? ` · ${stage}` : ''}: ${brief(cause ?? error.message)}`, 'error');
              setBadge('Install failed', 'error');
              row.classList.remove('is-installing');
              install.textContent = 'Retry';
              install.className = 'primary';
              install.disabled = false;
              install.hidden = false;
              runtimeSelect.disabled = false;
              const summary = () => report?.markdown ?? `# GeWu plugin installation repair (incomplete report)\n\nPlugin: ${release.plugin} ${release.version} (${release.channel})\nAttempt: ${attemptId}\nError: ${brief(error.message)}\n\nInstall diagnostics could not be retrieved. Read the host-local attempt log and retry.`;
              const fullDetails = () => report?.detailsMarkdown ?? summary();
              const view = (value = fullDetails()) => {
                const links = node('p');
                const provenance = report?.report?.release;
                for (const [role, url] of Object.entries({...provenance?.sources, 'release repository': provenance?.source})) {
                  try {
                    const target = new URL(url);
                    if (target.protocol !== 'https:' || target.username || target.password) continue;
                    const link = node('a', role);
                    link.href = target.href;
                    link.target = '_blank';
                    link.rel = 'noopener noreferrer';
                    links.append(link, ' ');
                  } catch {}
                }
                const textarea = node('textarea');
                textarea.readOnly = true;
                textarea.rows = 10;
                textarea.value = value;
                textarea.setAttribute('aria-label', 'Agent repair information');
                details.replaceChildren(links, textarea);
                return textarea;
              };
              next.replaceChildren(button('View details', () => view()), button('Copy repair info for Agent', async () => {
                try {
                  await navigator.clipboard.writeText(summary());
                  details.replaceChildren(node('p', 'Repair information copied.'));
                } catch {
                  const textarea = view(summary());
                  textarea.focus();
                  textarea.select();
                }
              }));
            };
            await describe();
          } finally {
            clearInterval(statusTimer);
          }
        });
        install.className = runtimeReady ? 'installed' : 'primary';
        install.disabled = runtimeReady || Boolean(blocker);
        install.hidden = runtimeReady;
        if (runtimeReady) setStatus('Ready to open', 'ok');
        else if (blocker) setStatus(blocker, 'warn');
        const showInstructions = async (source) => {
          const textarea = node('textarea');
          textarea.readOnly = true;
          textarea.rows = 10;
          textarea.value = source ? sourceInstallInstructions(release) : installInstructions(release);
          textarea.setAttribute('aria-label', source ? 'Source installation instructions' : 'Installation instructions');
          const copyText = async () => {
            try {
              await navigator.clipboard.writeText(textarea.value);
              copy.textContent = 'Copied';
            } catch {
              textarea.focus();
              textarea.select();
            }
          };
          const copy = button('Copy instructions', copyText);
          details.replaceChildren(textarea, copy);
          if (!source) await copyText();
        };
        actions.append(install, button('Copy install instructions', () => showInstructions(false)));
        if (prebuilt && !blocker) {
          for (const option of runtimeOptions.filter(option => option.available && ['pdf', 'tex'].includes(option.profile))) {
            const label = option.profile === 'pdf' ? 'PDF export' : 'TeX rendering';
            const prepare = button(`Prepare ${label} (${Math.ceil(option.bytes / 1e6)} MB)`, async () => {
              prepare.disabled = true;
              setStatus(`Preparing ${label}…`, 'progress');
              try {
                const result = await request({action: 'marketplace_install', pluginId: release.plugin, version: release.version,
                  channel: release.channel, runtime: 'bundled', profile: option.profile, attemptId: installAttemptId()});
                selectedRuntime = result.runtime;
                await showInstalled();
              } catch (error) {
                setStatus(brief(error.message), 'error');
                prepare.disabled = false;
              }
            });
            advanced.append(prepare);
          }
        }
        advanced.append(button('Install from source', () => showInstructions(true)));
        row.append(actions, status, runtimeInfo, next, details, advanced);
        const rollback = button('Rollback', async () => {
          try {
            const result = await request({action: 'marketplace_rollback', pluginId: release.plugin});
            setStatus(`Rolled back to ${result.release}.`, 'ok');
            setBadge('Rolled back', 'muted');
          } catch (error) { setStatus(`Rollback failed: ${error.message}`, 'error'); }
        });
        rollback.disabled = state?.state !== 'installed';
        if (state?.state === 'installed' && state?.previousGeneration) advanced.append(rollback);
        if (runtimeReady) showOpen(catalog.find(p => p.id === release.plugin));
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
    return instance.views?.find((v) => ['webview','webapp'].includes(v.kind) && v.placement === 'main')
      ?? instance.views?.find((v) => ['webview','webapp'].includes(v.kind));
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

  async function openPlugin(plugin, request = requestFor()) {
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
        if (plugin.settingsValid === false && plugin.settingsError) row.append(node('p', `Needs setup: ${plugin.settingsError}`));
        if (effective && plugin.settingsValid !== false) actions.append(button('Open\u2026', async () => {
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
    if (plugin.settingsValid === false) return ' \u00b7 needs setup';
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
        const resource = item.kind === 'builtin' && item.renderer !== 'diff'
          ? promptResource(`Workspace content path for ${item.title}`, item.resourceTypes?.[0])
          : undefined;
        if (item.kind === 'webview' || item.kind === 'webapp' || item.renderer === 'diff' || resource) {
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

  return {switchTab, sync, agentResult, setVisible: (on) => { if (on) sync(); }};
}
