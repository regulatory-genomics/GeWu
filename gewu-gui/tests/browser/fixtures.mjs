import {once} from 'node:events';
import {createServer as createHttp} from 'node:http';
import {createServer as createTcp} from 'node:net';
import {spawn} from 'node:child_process';
import {cp, mkdtemp, readFile, mkdir, rm, writeFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import {join} from 'node:path';

const repo = fileURLToPath(new URL('../../../', import.meta.url));
const binary = process.env.GEWU_BIN ?? join(repo,'target/debug/gewu');

async function port() {
  const server=createTcp(); server.listen(0,'127.0.0.1'); await once(server,'listening');
  const {port}=server.address(); server.close(); await once(server,'close'); return port;
}

export async function startFixture({packagePath, settings = {}, useDemo = !packagePath} = {}) {
  const temp=await mkdtemp(join(process.env.GEWU_TEST_TMP ?? process.env.TMPDIR ?? '/tmp','gewu-addon-'));
  const root=join(temp,'workspace');
  const other=join(temp,'other');
  const pkg=join(temp,'plugin');
  for (const dir of [root,other]) { await mkdir(dir); await writeFile(join(dir,'example.txt'),'original text'); }
  await cp(packagePath ?? join(repo,'gewu-server/tests/fixtures/plugin-adapter'),pkg,{recursive:true});
  const manifest=JSON.parse(await readFile(join(pkg,'plugin.json'),'utf8'));
  if (useDemo) await cp(join(repo,'sdk/python/gewu_plugin.py'),join(pkg,'gewu_plugin.py'));
  const requests=[];
  let nativeStream;
  const provider=createHttp(async (req,res)=>{
    const chunks=[];
    for await (const chunk of req) chunks.push(chunk);
    const body=Buffer.concat(chunks).toString();
    if (req.url === '/native/events') {
      if (req.headers['x-demo-key'] !== 'native-secret') {res.writeHead(401).end();return;}
      nativeStream=res;
      res.writeHead(200,{'content-type':'text/event-stream'}).write('data: first\n\n');
      return;
    }
    if (req.url === '/native/echo') {
      if (req.headers['x-demo-key'] !== 'native-secret') {res.writeHead(401).end();return;}
      if (req.headers.authorization || req.headers.cookie) {res.writeHead(400).end();return;}
      res.writeHead(200,{'content-type':'text/plain','set-cookie':'private=1'}).end(`Native service: ${JSON.parse(body).value}`);
      return;
    }
    if (req.url !== '/v1/chat/completions') {res.writeHead(404).end();return;}
    const turn=JSON.parse(body);
    requests.push(turn);
    const plugin=turn.tools?.find(t=>t.function.name.startsWith('plg_demo_echo_'));
    const openTool=turn.tools?.find(t=>t.function.name==='plugin_open');
    const afterTool=turn.messages.some(m=>m.role==='tool');
    const asked=turn.messages.filter(m=>m.role==='user').map(m=>typeof m.content==='string'?m.content:(m.content??[]).map(p=>p.text??'').join(' ')).join(' ');
    const send=(delta,finish_reason=null)=>res.write(`data: ${JSON.stringify({id:'fixture',model:'fixture',object:'chat.completion.chunk',choices:[{index:0,delta,finish_reason}]})}\n\n`);
    res.writeHead(200,{'content-type':'text/event-stream'});
    // Any plugin conformance spec can say `call <tool> <json>`; the agent, not
    // the browser, is the surface that names plugin tools.
    const askedCall = /^call\s+(\S+)\s+(\{[\s\S]*\})$/.exec(asked.trim());
    if (askedCall && !afterTool) {
      const name = turn.tools?.find(t => t.function.name === askedCall[1]
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
    workspaces:{other},plugins:{[manifest.id]:{package:pkg,enabled:true,settings:useDemo ? {service:origin} : settings}},
  }));
  const [socket,tcp,http,gui]=await Promise.all([port(),port(),port(),port()]);
  let output='';
  // A private HOME keeps ~/.gewu/plugins out of the workspace's inventory.
  const env={...process.env,HOME:temp,GEWU_GUI_TOKEN:'',GEWU_SERVE_TOKEN:'',NO_PROXY:'127.0.0.1,localhost',no_proxy:'127.0.0.1,localhost'};
  for (const key of ['OPENAI_MODEL','GEWU_PROVIDER','OPENAI_API_KEY','OPENAI_BASE_URL','OPENAI_ENDPOINT']) delete env[key];
  const start=(args)=>{
    const child=spawn(binary,['--no-update-check',...args],{cwd:root,env,stdio:['ignore','pipe','pipe']});
    child.on('error',err=>{output+=err.message;});
    for (const channel of [child.stdout,child.stderr]) channel.on('data',data=>{output=(output+data).slice(-24000);});
    return child;
  };
  const server=start(['serve','--llm-config',config,'--db-path',join(temp,'test.db'),'--ws-port',`${socket}`,'--tcp-port',`${tcp}`,'--http-port',`${http}`]);
  let guiChild;
  const url=`http://127.0.0.1:${gui}`;
  const close=async()=>{
    for (const child of [guiChild,server]) {
      if (!child?.pid || child.exitCode!==null || child.signalCode!==null) continue;
      const exit=once(child,'exit'); child.kill('SIGINT');
      const kill=setTimeout(()=>child.kill('SIGKILL'),6500);
      await exit; clearTimeout(kill);
    }
    provider.closeAllConnections();
    await new Promise(resolve=>provider.close(resolve));
    await rm(temp,{recursive:true,force:true});
  };
  for (let i=0;i<150;i++) {
    try {
      await fetch(`http://127.0.0.1:${http}/ready`,{signal:AbortSignal.timeout(150)});
      guiChild ??= start(['gui','--port',`${gui}`,'--server',`ws://127.0.0.1:${socket}`,'--server-http',`http://127.0.0.1:${http}`]);
      const response=await fetch(`${url}/api/config`,{signal:AbortSignal.timeout(150)});
      if (response.ok) return {url,close,root,other,requests,log:()=>output,manifest, finishNativeStream:()=>nativeStream.end('data: done\n\n')};
    } catch {}
    if (server.exitCode!==null || (guiChild && guiChild.exitCode!==null)) break;
    await new Promise(resolve=>setTimeout(resolve,50));
  }
  await close(); throw Error(`GeWu fixture could not start:\n${output}`);
}

/// The files sidebar starts closed and remembers being opened, so a reload
/// leaves it open. Tests only need the Preview tab visible either way.
export async function openPreview(page) {
  const reveal = page.getByRole('button', {name: 'Show files'});
  if (await reveal.isVisible()) await reveal.click();
  await page.getByRole('tab', {name: 'PREVIEW'}).click();
}
