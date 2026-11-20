export function bindFrame(frame, url, ready, handlers, onError) {
  let channel, closed = false;
  const target = new URL(url, location.href);
  if (target.origin !== location.origin || !target.pathname.startsWith('/plugin-data/')) throw Error('Invalid plugin frame URL');
  const timer = setTimeout(() => onError(Error('Plugin view did not initialize')), 15000);
  const handshake = async (event) => {
    if (closed || channel || event.source !== frame.contentWindow || event.origin !== 'null' || event.data?.type !== 'gewu.view.ready') return;
    clearTimeout(timer);
    channel = new MessageChannel();
    try {
      const context = await ready();
      if (closed) return;
      channel.port1.onmessage = async ({data}) => {
        if (closed || !data || !Number.isSafeInteger(data.id) || data.viewEpoch !== context.viewEpoch || !Object.hasOwn(handlers, data.method)) return;
        try {
          const result = await handlers[data.method](data.params ?? {}, context);
          if (!closed) channel.port1.postMessage({id:data.id, result});
        } catch (error) {
          if (!closed) channel.port1.postMessage({id:data.id, error:String(error.message ?? error)});
        }
      };
      frame.contentWindow.postMessage({type:'gewu.view.bound',context}, '*', [channel.port2]);
    } catch (error) { onError(error); }
  };
  window.addEventListener('message', handshake);
  frame.setAttribute('sandbox', 'allow-scripts');
  frame.referrerPolicy = 'no-referrer';
  frame.src = target.href;
  return () => {
    closed = true;
    clearTimeout(timer);
    window.removeEventListener('message', handshake);
    channel?.port1.close();
    frame.remove();
  };
}
