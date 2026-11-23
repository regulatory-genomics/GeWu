import {test} from 'node:test';
import assert from 'node:assert/strict';
import {bindAppFrame} from '../assets/plugin-app-bridge.mjs';

test('webapp handshake refuses the wrong origin and binds only the exact view nonce', async () => {
  const listeners = new Map();
  globalThis.window = {addEventListener:(name, fn)=>listeners.set(name,fn), removeEventListener:()=>{}};
  let posted = 0, ready = 0;
  const contentWindow = {postMessage:()=>posted++};
  const frame = {contentWindow, setAttribute(){}, remove(){}};
  const release = bindAppFrame(frame, {url:'http://view.localhost/_gewu/bootstrap#one',origin:'http://view.localhost',viewId:'view',nonce:'one'}, async()=>{ready++;return {viewId:'view',viewEpoch:1};}, {}, error=>{throw error;});
  const send = (origin, nonce)=>listeners.get('message')({source:contentWindow,origin,data:{type:'gewu.view.ready',viewId:'view',nonce}});
  await send('http://other.localhost','one'); await send('http://view.localhost','wrong');
  assert.equal(ready,0);
  await send('http://view.localhost','one');
  assert.equal(ready,1); assert.equal(posted,1);
  release();
});
