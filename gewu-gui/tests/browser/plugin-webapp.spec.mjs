import {test,expect} from '@playwright/test';
import {cp,mkdtemp,readFile,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {startFixture,openPreview} from './fixtures.mjs';

for (const appOrigins of [true, 'default']) test(`native webapp works with ${appOrigins === true ? 'configured' : 'default'} origins, SSE and revocation`,async({page})=>{
  const diagnostics=[];
  page.on('pageerror',error=>diagnostics.push(error.message));
  page.on('response',response=>{if(response.status()>=400){const u=new URL(response.url());diagnostics.push(`${response.status()} ${u.origin}${u.pathname}`);}});
  const temp=await mkdtemp(join(tmpdir(),'gewu-webapp-test-'));
  await cp(new URL('../../../gewu-server/tests/fixtures/plugin-adapter/',import.meta.url),temp,{recursive:true});
  await cp(new URL('../../../sdk/python/gewu_plugin.py',import.meta.url),join(temp,'gewu_plugin.py'));
  const manifest=JSON.parse(await readFile(join(temp,'plugin.json'),'utf8'));
  manifest.views=[{id:'studio',title:'Native Studio fixture',kind:'webapp',placement:'main',serviceId:'native',entryPath:'/native/app',resourceTypes:['text'],stateSchema:{type:'object'}}];
  manifest.views.push({...manifest.views[0],id:'secondary',title:'Secondary Studio',placement:'sidebar'});
  await writeFile(join(temp,'plugin.json'),JSON.stringify(manifest));
  const fixture=await startFixture({packagePath:temp,useDemo:true,appOrigins});
  try {
    if (appOrigins === 'default') {
      const old = fixture.url.replace('gui.gewu.localhost', '127.0.0.1');
      const response = await page.request.get(old, {maxRedirects: 0});
      expect(response.status()).toBe(307);
      expect(response.headers().location).toBe(`${fixture.url}/`);
    }
    await page.goto(fixture.url);await openPreview(page);
    await page.locator('.plugin-row').filter({hasText:'Demo adapter'}).getByRole('button',{name:'Open…',exact:true}).click();
    const studio=page.frameLocator('iframe[title="Native Studio fixture"]');
    await expect(studio.getByText('Native application connected',{exact:true})).toBeVisible();
    const frame=page.frames().find(f=>f.url().includes('/native/app'));
    expect(new URL(frame.url()).origin).not.toBe(new URL(page.url()).origin);
    const result=await frame.evaluate(async()=>{
      const saved=await fetch('/native/echo?file=scene%2Fintro.html',{method:'POST',headers:{'Content-Type':'application/json','If-Match':'"previous"'},body:JSON.stringify({value:'webapp'})});
      const redirected=await fetch('/native/redirect',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({value:'redirected webapp'})});
      const media=await fetch('/native/range',{headers:{Range:'bytes=1-3'}});
      const conditional=await fetch('/native/conditional',{headers:{'If-None-Match':'"current"'}});
      const stream=await fetch('/native/events');const reader=stream.body.getReader();const event=await reader.read();await reader.cancel();
      return {saved:await saved.text(),redirected:await redirected.text(),redirectStatus:redirected.status,etag:saved.headers.get('etag'),media:await media.text(),status:media.status,conditional:conditional.status,event:new TextDecoder().decode(event.value),secret:document.cookie};
    });
    expect(result).toMatchObject({saved:'Native service: webapp',redirected:'Native service: redirected webapp',redirectStatus:200,etag:'"next"',media:'bcd',status:206,conditional:304,secret:''});
    expect(result.event).toContain('first');
    const appUrl=frame.url();
    await page.getByRole('button',{name:'Close',exact:true}).click();
    expect((await page.request.get(appUrl)).status()).toBe(404);
    const dialogs=[];
    page.on('dialog',async dialog=>{dialogs.push(dialog.message());await dialog.dismiss();});
    await page.getByRole('button',{name:'Plugins',exact:true}).click();
    await page.getByRole('button',{name:'Secondary Studio',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Secondary Studio"]').getByText('Native application connected',{exact:true})).toBeVisible();
    expect(dialogs).toEqual([]);
  } catch(error) {throw Error(`${error.message}\n${diagnostics.join('\n')}\n${page.frames().map(f=>{try {const u=new URL(f.url());return u.origin+u.pathname;}catch{return f.url();}}).join('\n')}`);}
  finally {await fixture.close();await rm(temp,{recursive:true,force:true});}
});
