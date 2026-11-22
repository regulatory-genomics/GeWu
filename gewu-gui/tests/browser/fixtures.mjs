import {once} from 'node:events';
import {createServer as createHttp, request as httpRequest} from 'node:http';
import {createServer as createTcp} from 'node:net';
import {spawn} from 'node:child_process';
import {cp, mkdtemp, readFile, mkdir, rm, symlink, writeFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import {join} from 'node:path';

const repo = fileURLToPath(new URL('../../../', import.meta.url));
const binary = process.env.GEWU_BIN ?? join(repo,'target/debug/gewu');

async function port() {
  const server=createTcp(); server.listen(0,'127.0.0.1'); await once(server,'listening');
  const {port}=server.address(); server.close(); await once(server,'close'); return port;
}

export async function startFixture({packagePath, settings = {}, useDemo = !packagePath, disabled = false, marketplace, appOrigins = false, respond, installed} = {}) {
  const temp=await mkdtemp(join(process.env.GEWU_TEST_TMP ?? process.env.TMPDIR ?? '/tmp','gewu-addon-'));
  const root=join(temp,'workspace');
  const other=join(temp,'other');
  const pkg=installed?.package ?? join(temp,'plugin');
  for (const dir of [root,other]) { await mkdir(dir); await writeFile(join(dir,'example.txt'),'original text'); }
  if (installed) {
    // The host resolves the active generation and runtime from ~/.gewu, so use
    // the installed state instead of a source package or a PATH interpreter.
    await symlink(join(installed.home,'.gewu'),join(temp,'.gewu'),'dir');
  } else {
    await cp(packagePath ?? join(repo,'gewu-server/tests/fixtures/plugin-adapter'),pkg,{recursive:true});
  }
  const manifest=JSON.parse(await readFile(join(pkg,'plugin.json'),'utf8'));
  if (useDemo && !installed) await cp(join(repo,'sdk/python/gewu_plugin.py'),join(pkg,'gewu_plugin.py'));
  const requests=[];
  let nativeStream;
  const provider=createHttp(async (req,res)=>{
    const chunks=[];
    for await (const chunk of req) chunks.push(chunk);
    const body=Buffer.concat(chunks).toString();
    const pathname=new URL(req.url,'http://fixture').pathname;
    if(appOrigins && pathname==='/native/app') {
      res.writeHead(200,{'content-type':'text/html','content-security-policy':`frame-ancestors ${req.headers['x-gewu-parent-origin']}`}).end('<!doctype html><body>Connecting…<script type="module" src="/native/app.mjs"></script>');return;
    }
    if(appOrigins && pathname==='/native/app.mjs') {
      res.writeHead(200,{'content-type':'text/javascript'}).end(`const b=JSON.parse(sessionStorage.getItem('gewu.app.binding'));window.addEventListener('message',e=>{if(e.source===parent&&e.origin===b.parentOrigin&&e.data?.type==='gewu.view.bound'&&e.ports.length===1)document.body.textContent='Native application connected';});parent.postMessage({type:'gewu.view.ready',viewId:b.viewId,nonce:b.nonce},b.parentOrigin);`);return;
    }
    if(appOrigins && pathname==='/native/range') {res.writeHead(206,{'content-type':'video/mp4','content-range':'bytes 1-3/6','accept-ranges':'bytes'}).end('bcd');return;}
    if(appOrigins && pathname==='/native/conditional') {
      const unchanged=req.headers['x-demo-key']==='native-secret'&&req.headers['if-none-match']==='"current"';
      res.writeHead(unchanged?304:412,{'etag':'"current"'}).end();return;
    }
    if (req.url === '/native/events') {
      if (req.headers['x-demo-key'] !== 'native-secret') {res.writeHead(401).end();return;}
      nativeStream=res;
      res.writeHead(200,{'content-type':'text/event-stream'}).write('data: first\n\n');
      return;
    }
    if (pathname === '/native/echo') {
      if (req.headers['x-demo-key'] !== 'native-secret') {res.writeHead(401).end();return;}
      if (req.headers.authorization || req.headers.cookie) {res.writeHead(400).end();return;}
      res.writeHead(200,{'content-type':'text/plain','set-cookie':'private=1','etag':'"next"'}).end(`Native service: ${JSON.parse(body).value}`);
      return;
    }
    if (req.url !== '/v1/chat/completions') {res.writeHead(404).end();return;}
    const turn=JSON.parse(body);
    requests.push(turn);
    if(respond) {await respond(turn,res);return;}
    const plugin=turn.tools?.find(t=>t.function.name.startsWith('plg_demo_echo_'));
    const openTool=turn.tools?.find(t=>t.function.name==='plugin_open');
    const humanUser=message=>message.role==='user'&&!(Array.isArray(message.content)&&/^Tool(?: '[^']*')? returned additional media:/.test(message.content[0]?.text??''));
    const lastUser=turn.messages.reduce((index,message,current) => humanUser(message) ? current : index, -1);
    const afterTool=lastUser >= 0 && turn.messages.slice(lastUser + 1).some(m=>m.role==='tool');
    const userMessages=turn.messages.filter(humanUser);
    const askedMessage=userMessages.at(-1);
    const asked=typeof askedMessage?.content==='string'
      ? askedMessage.content
      : (askedMessage?.content??[]).map(p=>p.text??'').join(' ');
    const send=(delta,finish_reason=null)=>res.write(`data: ${JSON.stringify({id:'fixture',model:'fixture',object:'chat.completion.chunk',choices:[{index:0,delta,finish_reason}]})}\n\n`);
    res.writeHead(200,{'content-type':'text/event-stream'});
    // Any plugin conformance spec can say `call <tool> <json>`; the agent, not
    // the browser, is the surface that names plugin tools.
    const askedCall = /^call\s+(\S+)\s+(\{[^\n]*\})$/m.exec(asked.trim());
    if (askedCall && !afterTool) {
      const name = turn.tools?.find(t => t.function.name === askedCall[1]
        || t.function.name.endsWith(`.${askedCall[1]}`)
        || (t.function.name.startsWith('plg_') && t.function.name.includes(`_${askedCall[1]}_`)))?.function.name;
      if (name) {
        send({role:'assistant',tool_calls:[{index:0,id:'call-1',type:'function',function:{name,arguments:askedCall[2]}}]});
        send({},'tool_calls');
      } else {
        send({role:'assistant',content:`No tool named ${askedCall[1]}.`});
        send({},'stop');
      }
    } else if (openTool && asked.includes('open the demo') && !afterTool) {
      send({role:'assistant',tool_calls:[{index:0,id:'open-1',type:'function',function:{name:openTool.function.name,arguments:'{"pluginId":"demo"}'}}]});
      send({},'tool_calls');
    } else if (asked.includes('concurrency probe')) {
      send({role:'assistant',content:'session responsive while installing'});
      send({},'stop');
    } else if (plugin && !afterTool) {
      send({role:'assistant',tool_calls:[{index:0,id:'echo-1',type:'function',function:{name:plugin.function.name,arguments:'{"evidence":"adapter evidence"}'}}]});
      send({},'tool_calls');
    } else {
      send({role:'assistant',content:afterTool ? 'The adapter returned a result.' : 'No plugin tool selected.'});
      send({},'stop');
    }
    res.end('data: [DONE]\n\n');
  });
  provider.listen(0,'127.0.0.1');
  await once(provider,'listening');
  const origin=`http://127.0.0.1:${provider.address().port}`;
  const config=join(temp,'config.json');
  await writeFile(config,JSON.stringify({
    model:'local/fixture',endpoints:{local:{base_url:`${origin}/v1`,api_key:'test-only',models:{fixture:{provider:'openai-compatible'}}}},
    workspaces:{other},plugins:marketplace ? {} : {[manifest.id]:{package:installed ? '' : pkg,enabled:!disabled,settings:useDemo && !installed ? {service:origin} : settings}},
  }));
  const [socket,tcp,http,gui]=await Promise.all([port(),port(),port(),port()]);
  let output='';
  // A private HOME keeps ~/.gewu/plugins out of the workspace's inventory.
  const env={...process.env,HOME:temp,GEWU_GUI_TOKEN:'',GEWU_SERVE_TOKEN:'',NO_PROXY:'127.0.0.1,localhost',no_proxy:'127.0.0.1,localhost'};
  for (const key of ['OPENAI_MODEL','GEWU_PROVIDER','OPENAI_API_KEY','OPENAI_BASE_URL','OPENAI_ENDPOINT']) delete env[key];
  if (marketplace) {
    Object.assign(env, await marketplace({temp, pkg}));
    delete env.GEWU_MARKETPLACE_ASSET_ORIGIN;
  }
  const start=(args)=>{
    const child=spawn(binary,['--no-update-check',...args],{cwd:root,env,stdio:['ignore','pipe','pipe']});
    child.on('error',err=>{output+=err.message;});
    for (const channel of [child.stdout,child.stderr]) channel.on('data',data=>{output=(output+data).slice(-24000);});
    return child;
  };
  const serve=()=>start(['serve','--llm-config',config,'--db-path',join(temp,'test.db'),'--ws-port',`${socket}`,'--tcp-port',`${tcp}`,'--http-port',`${http}`]);
  let server=serve();
  let guiChild;
  const explicitAppOrigins=appOrigins === true;
  const url=`http://gui.gewu.localhost:${gui}`;
  const stop=async()=>{
    for (const child of [guiChild,server]) {
      if (!child?.pid || child.exitCode!==null || child.signalCode!==null) continue;
      const exit=once(child,'exit'); child.kill('SIGINT');
      const kill=setTimeout(()=>child.kill('SIGKILL'),6500);
      await exit; clearTimeout(kill);
    }
  };
  const close=async()=>{
    await stop();
    provider.closeAllConnections();
    await new Promise(resolve=>provider.close(resolve));
    await rm(temp,{recursive:true,force:true});
  };
  const ready=async()=>{
  for (let i=0;i<150;i++) {
    try {
      await fetch(`http://127.0.0.1:${http}/ready`,{signal:AbortSignal.timeout(150)});
        guiChild ??= start(['gui','--port',`${gui}`,'--server',`ws://127.0.0.1:${socket}`,'--server-http',`http://127.0.0.1:${http}`,...(explicitAppOrigins?['--public-origin',url,'--plugin-app-origin-template',`http://{view}.apps.gewu.localhost:${gui}`]:[])]);
       const status=await new Promise((resolve,reject)=>{
          const req=httpRequest({hostname:'127.0.0.1',port:gui,path:'/api/config',headers:{Host:new URL(url).host},timeout:150},res=>{res.resume();resolve(res.statusCode);});
         req.on('error',reject);req.on('timeout',()=>req.destroy(Error('GUI probe timed out')));req.end();
       });
       if (status===200) return;
    } catch {}
    if (server.exitCode!==null || (guiChild && guiChild.exitCode!==null)) break;
    await new Promise(resolve=>setTimeout(resolve,50));
  }
  throw Error(`GeWu fixture could not start:\n${output}`);
  };
  try { await ready(); } catch (error) { await close(); throw error; }
  const restart=async()=>{
    await stop();
    guiChild=undefined;
    server=serve();
    await ready();
  };
  return {url,close,restart,root,other,home:temp,start,packagePath:pkg,requests,log:()=>output,manifest, finishNativeStream:()=>nativeStream.end('data: done\n\n')};
}

/// The files sidebar starts closed and remembers being opened, so a reload
/// leaves it open. Tests only need the Preview tab visible either way.
export async function openPreview(page) {
  const reveal = page.getByRole('button', {name: 'Show files'});
  if (await reveal.isVisible()) await reveal.click();
  await page.getByRole('tab', {name: 'PREVIEW'}).click();
}
