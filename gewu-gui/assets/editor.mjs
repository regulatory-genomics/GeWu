export function node(tag, text) {
  const element = document.createElement(tag);
  if (text !== undefined) element.textContent = text;
  return element;
}

export function button(text, action) {
  const element = node('button', text);
  element.type = 'button';
  element.addEventListener('click', action);
  return element;
}

export async function editor(container, request, resource) {
  let {content, revision} = await request({action:'read_text', resource});
  const input = node('textarea');
  input.setAttribute('aria-label','File content');
  input.value = content;
  const status = node('p', '');
  status.setAttribute('role','status');
  const save = button('Save', async () => {
    save.disabled = true;
    try {
      const content = input.value;
      ({revision} = await request({action:'save_text', resource, content, expected:revision}));
      status.textContent = input.value === content ? 'Saved' : 'Saved earlier text; unsaved edits remain';
    } catch (error) { status.textContent = error.message; }
    finally { save.disabled = false; }
  });
  container.append(input, save, status);
}

export function preview(container, url, resource) {
  const mime = resource.mimeType ?? '';
  let element;
  if (mime.startsWith('image/')) {
    element = node('img'); element.alt = resource.uri;
  } else if (mime.startsWith('video/') || mime.startsWith('audio/')) {
    element = node(mime.split('/')[0]); element.controls = true;
  } else if (mime === 'text/html' || mime === 'application/pdf' || /\.(html?|pdf)$/i.test(resource.uri)) {
    element = node('iframe'); element.title = 'File preview';
    if (mime === 'text/html' || /\.html?$/i.test(resource.uri)) element.setAttribute('sandbox','');
  }
  if (element) {
    element.src = url;
    element.referrerPolicy = 'no-referrer';
    container.append(element);
  }
  const download = node('a','Download');
  download.href = url;
  download.download = decodeURIComponent(resource.uri.split('/').at(-1));
  container.append(download);
}

export async function changes(container, request) {
  const entries = await request({action:'changes'});
  if (!entries.length) container.append(node('p', 'No recorded file changes.'));
  for (const entry of entries) {
    const item = node('details');
    item.append(node('summary', `${entry.uri} · ${entry.source}`));
    if (typeof entry.diff === 'string') {
      if (Number.isSafeInteger(entry.additions) && Number.isSafeInteger(entry.deletions)) item.append(node('p',`+${entry.additions} / −${entry.deletions}`));
      item.append(node('pre',entry.diff || 'No text changes'));
    } else {
      item.append(node('p', entry.afterRevision ? `Updated revision: ${entry.afterRevision}` : `${entry.operation ?? 'Updated'} · no before/after diff recorded`));
    }
    container.append(item);
  }
}
