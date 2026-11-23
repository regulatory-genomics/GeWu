export function bindAppFrame(frame, binding, ready, handlers, onError) {
  let channel, closed = false;
  const target = new URL(binding.url);
  if (target.origin !== binding.origin) throw Error('Invalid application origin');
  const timer = setTimeout(() => onError(Error('Application view did not initialize')), 30000);
  const handshake = async event => {
    if (closed || channel || event.source !== frame.contentWindow || event.origin !== binding.origin || event.data?.type !== 'gewu.view.ready' || event.data.viewId !== binding.viewId || event.data.nonce !== binding.nonce) return;
    clearTimeout(timer);
    channel = new MessageChannel();
    try {
      const context = await ready();
      if (closed) return;
      channel.port1.onmessage = async ({data}) => {
        if (closed || !data || !Number.isSafeInteger(data.id) || data.viewEpoch !== context.viewEpoch || !Object.hasOwn(handlers, data.method)) return;
        try { channel.port1.postMessage({id:data.id, result:await handlers[data.method](data.params ?? {}, context)}); }
        catch (error) { channel.port1.postMessage({id:data.id, error:String(error.message ?? error)}); }
      };
      frame.contentWindow.postMessage({type:'gewu.view.bound',context}, binding.origin, [channel.port2]);
    } catch (error) { onError(error); }
  };
  window.addEventListener('message', handshake);
  frame.setAttribute('sandbox','allow-scripts allow-same-origin allow-downloads');
  frame.setAttribute('allow','fullscreen');
  frame.referrerPolicy = 'no-referrer';
  frame.src = target.href;
  return () => {closed = true; clearTimeout(timer); window.removeEventListener('message',handshake); channel?.port1.close(); channel?.port2.close(); frame.remove();};
}
