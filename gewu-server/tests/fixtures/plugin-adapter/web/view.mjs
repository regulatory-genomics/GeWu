const status = document.querySelector('#status');
let rpc;
window.addEventListener('message', ({source,data,ports}) => {
  if (source !== parent || data?.type !== 'gewu.view.bound' || !ports[0]) return;
  const port=ports[0];
  const {viewEpoch,state}=data.context;
  const pending=new Map();
  let id=0;
  port.onmessage=({data})=>{
    const handler=pending.get(data.id);
    pending.delete(data.id);
    if (data.error) handler?.reject(Error(data.error)); else handler?.resolve(data.result);
  };
  rpc=(method,params)=>new Promise((resolve,reject)=>{
    pending.set(++id,{resolve,reject});
    port.postMessage({id,method,params,viewEpoch});
  });
  status.textContent=state.saved ? 'Saved state restored' : 'GeWu connected';
}, {once:true});
document.querySelector('#save').onclick=async()=>{
  await rpc('saveState',{state:{saved:true}});
  status.textContent='View state saved';
};
document.querySelector('#native').onclick=async()=>{
  const {url}=await rpc('requestService',{serviceId:'native',path:'/native/echo',method:'POST'});
  const response=await fetch(url,{method:'POST',credentials:'omit',headers:{'content-type':'application/json'},body:'{"value":7}'});
  status.textContent=await response.text();
};
document.querySelector('#events').onclick=async()=>{
  const {url}=await rpc('requestService',{serviceId:'native',path:'/native/events',method:'GET'});
  const response=await fetch(url,{credentials:'omit',headers:{accept:'text/event-stream'}});
  const reader=response.body.pipeThrough(new TextDecoderStream()).getReader();
  let text='';
  for (;;) {
    const {value,done}=await reader.read();
    if (done) break;
    text+=value;
    if (text.includes('data: done')) status.textContent='Last event arrived';
    else if (text.includes('data: first')) status.textContent='First event arrived';
  }
};
parent.postMessage({type:'gewu.view.ready'},'*');
