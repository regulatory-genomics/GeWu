try {
  const token = location.hash.slice(1);
  history.replaceState(null,'',location.pathname);
  const response = await fetch('/_gewu/bootstrap', {method:'POST',headers:{'Content-Type':'text/plain'},body:token,credentials:'same-origin'});
  if (!response.ok) throw Error('Application session expired. Reload its GeWu view.');
  const {entryPath,viewId,nonce,parentOrigin} = await response.json();
  sessionStorage.setItem('gewu.app.binding',JSON.stringify({viewId,nonce,parentOrigin}));
  location.replace(entryPath);
} catch (error) { document.body.textContent = error.message; }
