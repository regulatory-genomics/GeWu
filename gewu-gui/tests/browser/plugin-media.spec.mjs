import {test,expect} from '@playwright/test';
import {startFixture} from './fixtures.mjs';
import {spawnSync} from 'node:child_process';
import {readFile,mkdtemp,rm} from 'node:fs/promises';
import {join} from 'node:path';
import {tmpdir} from 'node:os';

test('media Preview renews an expired resource and mints a fresh ticket when downloading',async({page})=>{
  const temp=await mkdtemp(join(tmpdir(),'gewu-media-'));const path=join(temp,'video.mp4');
  const encoded=spawnSync('ffmpeg',['-hide_banner','-loglevel','error','-f','lavfi','-i','color=c=blue:s=160x90:r=10:d=1','-c:v','libx264','-pix_fmt','yuv420p',path]);
  if(encoded.status!==0)throw Error(encoded.stderr.toString());
  const bytes=await readFile(path),fixture=await startFixture();
  try {
    await page.route('**/expired.mp4',r=>r.fulfill({status:404,body:'expired'}));
    await page.route('**/fresh.mp4*',r=>r.fulfill({status:200,contentType:'video/mp4',body:bytes}));
    await page.goto(fixture.url);
    await page.evaluate(async()=>{
      const {preview}=await import('/editor.mjs');const container=document.createElement('section');container.id='renewable-preview';document.body.append(container);
      window.resourceResolves=0;
      preview(container,'/expired.mp4',{uri:'workspace:///video.mp4',type:'demo.video',mimeType:'video/mp4'},async()=>`/fresh.mp4?ticket=${++window.resourceResolves}`);
    });
    await expect.poll(()=>page.locator('#renewable-preview video').evaluate(v=>v.readyState)).toBeGreaterThan(0);
    expect(await page.evaluate(()=>window.resourceResolves)).toBe(1);
    const download=page.waitForEvent('download');await page.locator('#renewable-preview').getByRole('link',{name:'Download'}).click();
    expect((await download).suggestedFilename()).toBe('video.mp4');
    expect(await page.evaluate(()=>window.resourceResolves)).toBe(2);
  }finally{await fixture.close();await rm(temp,{recursive:true,force:true});}
});
