import {bindFrame} from './plugin-bridge.mjs';
import {button, changes, editor, node, preview} from './editor.mjs';

export const layoutKey = (workspace, session) => `gewu.panels.v1:${JSON.stringify([workspace, session])}`;

export function panelHost(root, toggle, rpc) {
  let scope, scopeKey, restoring, visible = false, seq = 0, active = 'services', timer;
  const panels = new Map();
  const tabs = node('nav');
  tabs.setAttribute('aria-label','Application views');
  const body = node('div');
  const toolbar = node('div');
  toolbar.className = 'plugin-toolbar';
  const main = button('Toggle split', () => {
    root.classList.toggle('plugin-main');
    store();
  });
  toolbar.append(main,button('Hide panels', () => setVisible(false)));
  root.append(toolbar,tabs,body);
  toggle.addEventListener('click', () => setVisible(!visible));

  const fail = (target,error) => target.append(node('p',String(error.message ?? error)));
  const requestFor = (tab = scope?.id) => (action) => rpc(tab,action);
  function store() {
    if (!scope?.session_id || restoring===scopeKey) return;
    const views = [...panels.values()].map(({spec}) => spec).filter(Boolean);
    try { localStorage.setItem(layoutKey(scope.workspace,scope.session_id), JSON.stringify({views,active,visible,split:root.classList.contains('plugin-main')})); } catch {}
  }
  function setVisible(next) {
    visible = next;
    root.hidden = !visible;
    toggle.setAttribute('aria-expanded',String(visible));
    if (visible) { focus(active); if (active === 'services') refresh(); }
    else { clearTimeout(timer); toggle.focus(); }
    store();
  }
  function focus(id) {
    if (!panels.has(id)) id = 'services';
    active = id;
    root.classList.toggle('panel-prefers-main', panels.get(id)?.placement === 'main');
    tabs.replaceChildren();
    for (const [key,panel] of panels) {
      const tab = button(panel.title, () => {focus(key); if (key === 'services') refresh();});
      tab.setAttribute('aria-pressed',String(key === active));
      tabs.append(tab);
      if (key === active && key !== 'services' && key !== 'changes') tabs.append(button('Close view', () => close(key)));
      panel.content.hidden = key !== active;
    }
    if (scope?.session_id && (panels.get(id)?.viewId || [...panels.values()].some(p=>p.viewId))) {
      requestFor()({action:'focus_view',viewId:panels.get(id)?.viewId}).catch(()=>{});
    }
    store();
  }
  function close(id) {
    const panel = panels.get(id);
    panel?.dispose?.();
    panel?.content.remove();
    panels.delete(id);
    if (active === id) { focus('services'); refresh(); }
  }
  function add(id,title,spec,placement) {
    const content = node('div');
    content.className = 'plugin-panel';
    content.hidden = true;
    const p = {title,spec,placement,content,dispose:null};
    panels.set(id,p);
    body.append(content);
    return p;
  }
  async function openView(instance, view, resource, stored) {
    if (!scope || !instance) return;
    const request = requestFor();
    const spec = stored ?? {id:`view-${++seq}`,pluginId:instance.pluginId,bindingResource:instance.bindingResource,viewType:view.id, resource, state:{}};
    if (panels.has(spec.id)) {focus(spec.id); return;}
    const panel = add(spec.id,view.title,spec,view.placement);
    let disposed = false;
    let release = () => {};
    panel.dispose = () => { disposed = true; release(); };
    if (!stored) focus(spec.id);
    try {
      const binding = await request({action:'attach',instanceId:instance.id,viewType:view.id,resource});
      const {viewId} = binding;
      const detach = () => request({action:'detach',viewId}).catch(() => {});
      if (disposed) {detach(); return;}
      release = detach;
      if (view.kind === 'webview') {
        const frame = node('iframe');
        frame.title = view.title;
        panel.content.append(frame);
        const local = bindFrame(frame,binding.url,async () => {
          const context=await request({action:'view_ready',viewId});
          if (!disposed) {
            panel.viewId=viewId;
            if (active===spec.id) await request({action:'focus_view',viewId});
          }
          return {...context,state:spec.state};
        },{
          getContext: (_,ctx) => ctx,
          callTool: ({tool,args}) => request({action:'call',instanceId:instance.id,viewId,tool,args}),
          resolveResource: ({resource}) => request({action:'resource',instanceId:instance.id,viewId,resource}),
          openResource: ({resource}) => { openResultResource(instance,resource); return {opened:true}; },
          publishContext: ({summary}) => request({action:'publish_context',viewId,summary}),
          saveState: async ({state}) => {spec.state=await request({action:'save_state',viewId,state}); store(); return spec.state;},
          requestService: ({serviceId,path,method}) => request({action:'service',viewId,serviceId,path,method}),
          getCall: ({callId}) => request({action:'call_status',callId}),
          cancelCall: ({callId}) => request({action:'cancel',callId}),
        }, error => fail(panel.content,error));
        release = () => {local(); detach();};
      } else if (view.renderer === 'diff') await changes(panel.content, request);
      else if (!resource) throw Error('Select a content resource before opening this view.');
      else if (view.renderer === 'text') await editor(panel.content,request,resource);
      else preview(panel.content,(await request({action:'resource',instanceId:instance.id,resource})).url,resource);
    } catch (error) { if (!disposed) fail(panel.content,error); }
  }
  function openResultResource(instance,resource) {
    const declared = instance.views.find(v => v.resourceTypes.includes(resource.type) && (!v.mimeTypes || v.mimeTypes.includes(resource.mimeType)));
    if (declared) {openView(instance,declared,resource); return;}
    const request = requestFor();
    const id = `preview-${++seq}`;
    const panel = add(id,resource.uri,null,'main');
    focus(id);
    request({action:'resource',instanceId:instance.id,resource}).then(({url})=>preview(panel.content,url,resource)).catch(error=>fail(panel.content,error));
  }
  function editPath(path) {
    const id = `edit-${++seq}`;
    const resource = {uri:`workspace:///${path.split('/').map(encodeURIComponent).join('/')}`,type:'text'};
    const panel = add(id,path,null,'main');
    setVisible(true);
    focus(id);
    editor(panel.content,requestFor(),resource).catch(error=>fail(panel.content,error));
  }
  async function agentResult(resultId) {
    const request=requestFor(), key=scopeKey;
    setVisible(true);
    const id=`result-${++seq}`;
    const panel=add(id,'Application result',null,'main');
    focus(id);
    try {
      const {pluginId,bindingResource,result}=await request({action:'agent_result',resultId});
      if (scopeKey!==key) return;
      panel.content.append(node('pre',JSON.stringify(result,null,2)));
      for (const resource of result.resources ?? []) panel.content.append(button(resource.uri,async()=>{
        try {
          const instance=await request({action:'open',pluginId,bindingResource:bindingResource ?? undefined});
          if (scopeKey===key) openResultResource(instance,resource);
        } catch(error){fail(panel.content,error);}
      }));
    } catch(error) {fail(panel.content,error);}
  }
  async function refresh() {
    clearTimeout(timer);
    if (!visible || active !== 'services' || !scope) return;
    const config = panels.get('services');
    const request = requestFor();
    try {
      const [{plugins,instances}, calls] = await Promise.all([request({action:'list'}),request({action:'calls'})]);
      if (config !== panels.get('services') || active !== 'services') return;
      const services = node('div');
      services.append(node('h3','Applications'));
      if (!plugins.length) services.append(node('p','No configured plugins.'));
      for (const p of plugins) {
        const row = node('div');
        row.append(node('span',p.title ?? p.id));
        if (p.enabled) row.append(button('Start', async () => {
          try {
            const type = p.scope === 'resource' ? window.prompt('Binding resource type', p.views?.[0]?.resourceTypes?.[0] ?? 'document') : null;
            if (p.scope === 'resource' && !type) return;
            const bindingResource = type ? readResource('Workspace path of document or deck',type) : undefined;
            if (p.scope === 'resource' && !bindingResource) return;
            const instance = await request({action:'open',pluginId:p.id,bindingResource});
            if (config !== panels.get('services')) return;
            const view = instance.views.find(v=>v.kind==='webview');
            if (view) await openView(instance,view);
            else await refresh();
          } catch (error) {fail(config.content,error);}
        }));
        else row.append(node('span',' · Disabled'));
        row.append(button(p.enabled ? `Disable ${p.title ?? p.id}` : `Enable ${p.title ?? p.id}`,async()=>{
          try { await request({action:'set_enabled',pluginId:p.id,enabled:!p.enabled}); await refresh(); }
          catch (error) {fail(config.content,error);}
        }));
        services.append(row);
      }
      for (const instance of instances) {
        const group = node('details');
        group.open = true;
        group.append(node('summary',instance.title),button('Stop', async () => {
          try {await request({action:'close',instanceId:instance.id}); await refresh();} catch(error){fail(config.content,error);}
        }));
        for (const view of instance.views) group.append(button(view.title, () => {
          const resource = view.kind !== 'webview' && view.renderer !== 'diff' ? readResource('Workspace content path',view.resourceTypes[0]) : undefined;
          if (resource || view.kind === 'webview' || view.renderer === 'diff') openView(instance,view,resource);
        }));
        for (const tool of instance.tools.filter(t => t.target==='backend')) group.append(button(tool.name,async () => {
          try {
            const text = window.prompt(`${tool.description}\nArguments (JSON object)`, '{}');
            if (text === null) return;
            await request({action:'call',instanceId:instance.id,tool:tool.name,args:JSON.parse(text)});
            await refresh();
          } catch(error){ fail(config.content,error); }
        }));
        services.append(group);
      }
      services.append(node('h3','Calls'));
      for (const call of calls) {
        const row = node('div');
        row.append(node('span',`${call.tool} · ${call.status}${call.progress ? ` · ${call.progress}` : ''}`));
        if (call.status === 'running') row.append(button('Cancel', async () => {
          try { await request({action:'cancel',callId:call.id}); await refresh(); } catch(error){fail(config.content,error);}
        }));
        else row.append(button('Result',async () => {
          try {
            const {result} = await request({action:'call_status',callId:call.id});
            if (config !== panels.get('services')) return;
            const id = `result-${++seq}`;
            const panel = add(id,`Result · ${call.tool}`,null,'main');
            panel.content.append(node('pre',JSON.stringify(result,null,2)));
            const instance = instances.find(i=>i.id===call.instanceId);
            if (instance) for (const resource of result.resources ?? []) panel.content.append(button(resource.uri,()=>openResultResource(instance,resource)));
            focus(id);
          } catch(error){fail(config.content,error);}
        }));
        const instance = instances.find(i=>i.id===call.instanceId);
        if (instance) for (const resource of call.resources ?? []) row.append(button(resource.uri,()=>openResultResource(instance,resource)));
        services.append(row);
      }
      config.content.replaceChildren(services);
      if (calls.some(c=>c.status==='running')) timer=setTimeout(refresh,700);
    } catch (error) { if (config === panels.get('services')) config.content.replaceChildren(node('p',error.message)); }
  }
  function readResource(message,type='document') {
    const path = window.prompt(message)?.trim();
    return path ? {uri:`workspace:///${path.split('/').map(encodeURIComponent).join('/')}`,type} : undefined;
  }
  async function switchTab(next) {
    if (!next) return;
    const key = layoutKey(next.workspace,next.session_id ?? `local-${next.id}`);
    if (key === scopeKey) return;
    if (scope && scope.id===next.id && scope.workspace===next.workspace && !scope.session_id && next.session_id) {
      scope=next; scopeKey=key; store(); return;
    }
    const showOnFirstConnect = !scope && visible;
    scope=next; scopeKey=key;
    restoring=key;
    clearTimeout(timer);
    for (const panel of panels.values()) panel.dispose?.();
    panels.clear(); body.replaceChildren();
    add('services','Services',null,'sidebar');
    const c = add('changes','File changes',null,'sidebar');
    c.content.append(button('Refresh changes', async () => {
      c.content.querySelector('section')?.remove();
      const target = node('section'); c.content.append(target);
      try {await changes(target,requestFor());} catch(error){fail(target,error);}
    }));
    let saved;
    try {saved=JSON.parse(localStorage.getItem(key));} catch {}
    visible=Boolean(saved?.visible ?? showOnFirstConnect);
    root.hidden=!visible;
    toggle.setAttribute('aria-expanded',String(visible));
    root.classList.toggle('plugin-main',Boolean(saved?.split));
    focus('services');
    if (visible) refresh();
    const request=requestFor(next.id);
    let restoredActive=saved?.active === 'changes' ? 'changes' : 'services';
    if (Array.isArray(saved?.views)) for (const [index, spec] of saved.views.slice(0,12).entries()) {
      if (scopeKey!==key) return;
      if (spec.id===saved.active) restoredActive=`restored-${index}`;
      spec.id=`restored-${index}`;
      try {
        const instance=await request({action:'open',pluginId:spec.pluginId,bindingResource:spec.bindingResource});
        if (scopeKey!==key) return;
        const view=instance.views.find(v=>v.id===spec.viewType);
        if (!view) throw Error('View is no longer available.');
        await openView(instance,view,spec.resource,spec);
      } catch(error) {
        if (scopeKey!==key) return;
        const placeholder=add(spec.id,spec.pluginId,spec,'sidebar');
        fail(placeholder.content,error);
      }
    }
    if (scopeKey!==key) return;
    restoring=undefined;
    focus(restoredActive);
  }
  return {switchTab,editPath,agentResult};
}
