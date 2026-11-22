import {expect,test} from '@playwright/test';
import {openPreview,startFixture} from './fixtures.mjs';

const openCalls = (page) => {
  const calls = [];
  page.on('websocket', ws => ws.on('framesent', ({payload}) => {
    try {
      const message = JSON.parse(String(payload));
      if (message.op === 'plugin' && message.plugin?.action === 'open') calls.push(message.plugin);
    } catch {}
  }));
  return calls;
};

test('Preview shares the Files/Tools dock and resizing moves the transcript edge', async ({page})=>{
  const fixture=await startFixture();
  try {
    await page.setViewportSize({width:1440,height:900});
    await page.goto(fixture.url);
    await expect(page.getByRole('textbox',{name:'Message'})).toBeEnabled();
    await page.getByRole('button',{name:'Show files'}).click();
    const tabs=page.locator('[data-sidebar-tab]');
    await expect(tabs).toHaveText(['FILES','TOOLS','PREVIEW']);
    await expect.poll(async()=>Math.round((await page.locator('#sidebar').boundingBox()).width)).toBe(240);

    const transcript=page.locator('#transcript');
    const narrow=await transcript.boundingBox();
    await page.getByRole('tab',{name:'PREVIEW'}).click();
    const wide=await transcript.boundingBox();
    const dock=await page.locator('#sidebar').boundingBox();
    // Switching the manager tab keeps the Files width; opening an application
    // is the action that asks the dock for more room.
    expect(wide.width).toBeCloseTo(narrow.width,0);
    // The dock shares the row: it must not sit on top of the transcript.
    expect(wide.x + wide.width).toBeLessThanOrEqual(dock.x + 2);

    const resizer=page.getByRole('separator',{name:'Resize panels'});
    const before=(await page.locator('#sidebar').boundingBox()).width;
    const box=await resizer.boundingBox();
    await page.mouse.move(box.x+box.width/2, box.y+box.height/2);
    await page.mouse.down();
    await page.mouse.move(box.x-160, box.y+box.height/2, {steps:8});
    await page.mouse.up();
    const after=(await page.locator('#sidebar').boundingBox()).width;
    expect(after).toBeGreaterThan(before+120);
    await page.reload();
    await openPreview(page);
    await expect.poll(async()=>(await page.locator('#sidebar').boundingBox()).width).toBeGreaterThan(before+120);
  } finally {await fixture.close();}
});

test('stopping a plugin then reloading never starts it again', async ({page})=>{
  const fixture=await startFixture();
  try {
    const opens=openCalls(page);
    await page.goto(fixture.url);
    await expect(page.getByRole('textbox',{name:'Message'})).toBeEnabled();
    await openPreview(page);
    await page.locator('.plugin-row').filter({hasText:'Demo adapter'}).getByRole('button',{name:'Open…',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected');
    expect(opens.length).toBe(1);

    await page.getByRole('button',{name:'Plugins',exact:true}).click();
    await page.getByRole('button',{name:'Stop',exact:true}).click();
    await expect(page.locator('iframe[title="Demo View"]')).toHaveCount(0);
    await expect(page.locator('.plugin-row').filter({hasText:'Demo adapter'}).getByRole('button',{name:'Open…',exact:true})).toBeVisible();

    const before=opens.length;
    await page.reload();
    await openPreview(page);
    await expect(page.locator('.plugin-row').filter({hasText:'Demo adapter'}).getByRole('button',{name:'Open…',exact:true})).toBeVisible();
    await expect(page.locator('iframe[title="Demo View"]')).toHaveCount(0);
    expect(opens.length).toBe(before);
  } finally {await fixture.close();}
});
