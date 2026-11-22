import {expect, test} from '@playwright/test';
import {createHash, generateKeyPairSync, sign} from 'node:crypto';
import {once} from 'node:events';
import {execFile} from 'node:child_process';
import {createServer} from 'node:http';
import {chmod, mkdir, readFile, writeFile} from 'node:fs/promises';
import {join} from 'node:path';
import {promisify} from 'node:util';
import {openPreview, startFixture} from './fixtures.mjs';

test('Marketplace shows one latest release per plugin and copies the right repair report', async ({page}) => {
  const assets = new Map();
  const downloads = new Set();
  const http = createServer((req, res) => {
    downloads.add(req.url);
    const asset = assets.get(req.url);
    const reply = () => res.writeHead(asset ? 200 : 404).end(asset);
    if (req.url === '/runtime-demo-slow') setTimeout(reply, 8000);
    else reply();
  });
  http.listen(0, '127.0.0.1');
  await once(http, 'listening');
  const origin = `http://127.0.0.1:${http.address().port}`;
  const zip = promisify(execFile);
  let fixture;
  try {
    fixture = await startFixture({marketplace: async ({temp, pkg}) => {
      const {privateKey, publicKey} = generateKeyPairSync('ed25519');
      const key = publicKey.export({format:'jwk'}).x;
      const target = {os:process.platform === 'darwin' ? 'macos' : 'linux', arch:process.arch === 'arm64' ? 'aarch64' : 'x86_64', ...(process.platform === 'linux' ? {libc:'glibc'} : {})};
      const asset = (path, bytes) => {
        assets.set(path, bytes);
        return {url:origin + path, sha256:createHash('sha256').update(bytes).digest('hex'), bytes:Buffer.byteLength(bytes)};
      };
      const baseManifest = JSON.parse(await readFile(join(pkg, 'plugin.json'), 'utf8'));
      const releases = [];
      for (const {id, version, fail, fallback, reuse, missingExternal, slow} of [
        {id:'demo', version:'0.1.0', fail:false},
        {id:'demo-fail', version:'0.2.0', fail:true},
        {id:'demo-fallback', version:'0.1.0', fail:true, fallback:true},
        {id:'demo-reuse', version:'0.1.0', fail:true, reuse:true},
        {id:'demo-bundle-fallback', version:'0.1.0', fail:false, missingExternal:true},
        {id:'demo-slow', version:'0.1.0', fail:false, slow:true},
      ]) {
        const runtime = join(temp, `runtime-${id}`);
        await mkdir(join(runtime, 'bin'), {recursive:true});
        const manifest = JSON.stringify({schemaVersion:'gewu.runtime/v1', id:`${id}-runtime`, version, plugin:id, target, launcher:'bin/launch', probe:'bin/probe', capabilities:[]});
        await writeFile(join(runtime, 'runtime.json'), manifest);
        await writeFile(join(runtime, 'bin/launch'), '#!/bin/sh\nexit 0\n', {mode:0o755});
        await writeFile(join(runtime, 'bin/probe'), fail ? '#!/bin/sh\necho "fixture dependency was not found" >&2\necho "https://user:credential@example.org/private" >&2\nexit 17\n' : '#!/bin/sh\nexit 0\n', {mode:0o755});
        await writeFile(join(pkg, 'plugin.json'), JSON.stringify({...baseManifest, id, version}));
        await chmod(join(pkg, 'adapter.py'), 0o755);
        await zip('zip', ['-qr', join(temp, `pkg-${id}.zip`), '.'], {cwd:pkg});
        await zip('zip', ['-qr', join(temp, `rt-${id}.zip`), '.'], {cwd:runtime});
        const ref = 'a'.repeat(40);
        const bundled = {id:`${id}-runtime`, version, target, artifact:asset(`/runtime-${id}`, await readFile(join(temp, `rt-${id}.zip`))), manifest:asset(`/manifest-${id}`, manifest)};
        releases.push({schemaVersion:(fallback || reuse || missingExternal) ? 'gewu.release/v2' : 'gewu.release/v1', releaseId:`${id}-${version}`, plugin:id, version,
          channel:'stable', releaseNotes:'Create scientific figures.\n\nPackaging changes for this release.', gewu:{api:'gewu.plugin/v1', host:'>=0.1.0'},
          package:asset(`/package-${id}`, await readFile(join(temp, `pkg-${id}.zip`))),
          runtimes:[bundled],
          ...((fallback || reuse || missingExternal) ? {runtimePlan:{schemaVersion:'gewu.runtime-plan/v1', policies:fallback ? ['bundled','external'] : ['external','bundled'], providers:[
            {kind:'bundled', ...bundled},
            {kind:'external', id:'fixture-python', target, executable:{commands:['python3']}, probe:{args:['-c',missingExternal ? 'raise RuntimeError("no usable browser")' : 'print("ok")'],timeoutSeconds:5}},
          ]}} : {}),
          capabilities:[], permissions:[], provenance:{builder:'fixture', source:'https://github.com/example/demo', sourceCommit:ref,
            buildInputs:[`application-source=https://github.com/example/demo/tree/${ref}`, `plugin-source=https://github.com/example/demo/tree/${ref}/plugin`, `runtime-recipe=https://github.com/example/demo/blob/${ref}/runtime.toml`, `installation-guide=https://github.com/example/demo/blob/${ref}/INSTALL.md`], createdAt:new Date().toISOString()}});
      }
      const index = Buffer.from(JSON.stringify({schemaVersion:'gewu.marketplace/v2',repository:'official',keyId:'official-v1',
        generatedAt:new Date().toISOString(), expiresAt:new Date(Date.now() + 3_600_000).toISOString(), releases}));
      assets.set('/index.v1.json', index);
      assets.set('/index.v1.json.sig', sign(null, index, privateKey).toString('base64'));
      return {GEWU_MARKETPLACE_BASE_URL:origin, GEWU_MARKETPLACE_PUBLIC_KEY:Buffer.from(key, 'base64url').toString('base64')};
    }});
    await page.addInitScript(() => Object.defineProperty(navigator, 'clipboard', {value:undefined}));
    await page.goto(fixture.url);
    const prompt = page.getByRole('textbox', {name:'Message'});
    await expect(prompt).toBeEnabled();
    await prompt.fill('Initialize the conversation');
    await prompt.press('Enter');
    await expect(page.getByText('No plugin tool selected.', {exact:true})).toBeVisible();
    await openPreview(page);
    await page.getByRole('button', {name:'Marketplace', exact:true}).click();
    await expect(page.locator('.plugin-marketplace .plugin-row')).toHaveCount(6);
    for (const [id, kind] of [['demo-reuse', 'external'], ['demo-bundle-fallback', 'bundled']]) {
      const row = page.locator('.plugin-marketplace .plugin-row').filter({hasText:`${id} 0.1.0`});
      await row.getByRole('button', {name:'Install',exact:true}).click();
      await expect(row.getByRole('button', {name:'Open…',exact:true})).toBeVisible();
      const state = JSON.parse(await readFile(join(fixture.home, '.gewu/plugins/state.json'), 'utf8'));
      expect(state.plugins[id].generations[`${id}-0.1.0`].runtime.kind).toBe(kind);
    }
    expect(downloads.has('/runtime-demo-reuse')).toBe(false);
    expect(downloads.has('/runtime-demo-bundle-fallback')).toBe(true);
    const good = page.locator('.plugin-marketplace .plugin-row').filter({hasText:'demo 0.1.0'});
    const bad = page.locator('.plugin-marketplace .plugin-row').filter({hasText:'demo-fail 0.2.0'});
    const fallback = page.locator('.plugin-marketplace .plugin-row').filter({hasText:'demo-fallback 0.1.0'});
    await expect(fallback.locator('select')).toHaveValue('auto');
    await fallback.getByRole('button', {name:'Install',exact:true}).click();
    await expect(fallback.getByRole('button', {name:'Open…',exact:true})).toBeVisible();
    const installed = JSON.parse(await readFile(join(fixture.home,'.gewu/plugins/state.json'), 'utf8'));
    expect(installed.plugins['demo-fallback'].generations['demo-fallback-0.1.0'].runtime.kind).toBe('external');
    await expect(fallback.locator('.plugin-card-runtime')).toContainText('Using existing environment');
    const slow = page.locator('.plugin-marketplace .plugin-row').filter({hasText:'demo-slow 0.1.0'});
    await slow.getByRole('button', {name:'Install',exact:true}).click();
    await expect(slow.locator('progress')).toBeVisible();
    await page.reload();
    await openPreview(page);
    await page.getByRole('button', {name:'Marketplace', exact:true}).click();
    await expect(page.locator('.plugin-marketplace .plugin-row').filter({hasText:'demo-slow 0.1.0'}).locator('progress')).toBeVisible();
    await prompt.fill('concurrency probe');
    await prompt.press('Enter');
    await expect(page.getByText('session responsive while installing', {exact:true})).toBeVisible({timeout:2500});
    const tabsBefore = await page.locator('[data-session-tab]').count();
    await page.locator('[data-session-new-workspace="cwd"]').click();
    await expect(page.locator('[data-session-tab]')).toHaveCount(tabsBefore + 1, {timeout:2500});
    await expect.poll(async () => {
      const state = JSON.parse(await readFile(join(fixture.home, '.gewu/plugins/state.json'), 'utf8'));
      return state.plugins['demo-slow']?.activeGeneration;
    }, {timeout:20000}).toBe('demo-slow-0.1.0');
    await expect(page.locator('.plugin-marketplace .plugin-row').filter({hasText:'demo-slow 0.1.0'}).locator('progress')).toBeHidden();
    await page.getByRole('button', {name:'Marketplace', exact:true}).click();
    await expect(good.locator('.plugin-card-description')).toHaveText('Create scientific figures.');
    await expect(good.getByRole('button', {name:'Install from source',exact:true})).not.toBeVisible();
    await good.getByRole('button', {name:'Copy install instructions',exact:true}).click();
    const instructions = good.getByRole('textbox', {name:'Installation instructions',exact:true});
    await expect(instructions).toHaveValue(/demo 0.1.0 \(stable\)/);
    await expect(instructions).toHaveValue(/gewu plugin marketplace-install demo --version 0.1.0 --channel stable --runtime auto/);
    await expect(instructions).not.toHaveValue(/staged-package/);
    await good.getByRole('button', {name:'Copy instructions',exact:true}).click();
    await expect(instructions).toBeFocused();
    await good.locator('summary').filter({hasText:'Advanced'}).click();
    await good.getByRole('button', {name:'Install from source',exact:true}).click();
    const source = good.getByRole('textbox', {name:'Source installation instructions'});
    await expect(source).toHaveValue(/INSTALL.md/);
    await expect(source).toHaveValue(/gewu plugin install/);
    await expect(good.getByRole('button', {name:'Install',exact:true})).toBeEnabled();
    await good.getByRole('button', {name:'Install',exact:true}).click();
    await good.getByRole('button', {name:'Open…',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected');
    await page.getByRole('button', {name:'Marketplace',exact:true}).click();
    await bad.getByRole('button', {name:'Install',exact:true}).click();
    await expect(bad.getByRole('status')).toContainText('Install failed · runtime_probe');
    await bad.getByRole('button', {name:'Copy repair info for Agent'}).click();
    const repair = bad.getByRole('textbox', {name:'Agent repair information'});
    await expect(repair).toHaveValue(/fixture dependency was not found/);
    await expect(repair).toHaveValue(/exitCode=17/);
    await expect(repair).not.toHaveValue(/user:credential/);
    await expect(bad.getByRole('link', {name:'runtime-recipe'})).toHaveAttribute('href', /\/blob\/[a-f]{40}\/runtime.toml$/);
    await expect(bad.getByRole('link', {name:'installation-guide'})).toHaveAttribute('href', /\/blob\/[a-f]{40}\/INSTALL.md$/);
    const first = await repair.inputValue();
    expect(first.length).toBeLessThan(6144);
    await bad.getByRole('button', {name:'View details'}).click();
    await expect(repair).toHaveValue(/Subprocess outcomes:/);
    await expect(repair).toHaveValue(/"exitCode":17/);
    await bad.getByRole('button', {name:'Copy repair info for Agent'}).click();
    await bad.getByRole('button', {name:'Retry', exact:true}).click();
    await bad.getByRole('button', {name:'Copy repair info for Agent'}).click();
    await expect(repair).not.toHaveValue(first);
    const after = JSON.parse(await readFile(join(fixture.home,'.gewu/plugins/state.json'), 'utf8'));
    expect(after.plugins.demo.activeGeneration).toBe('demo-0.1.0');
  } finally {
    await fixture?.close();
    http.closeAllConnections();
    await new Promise(resolve => http.close(resolve));
  }
});
