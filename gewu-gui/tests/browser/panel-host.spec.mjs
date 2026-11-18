import {expect,test} from '@playwright/test';
import {startFixture} from './fixtures.mjs';

test('binds an opaque frame, streams native HTTP, restores state and revokes stale assets', async ({page})=>{
  const fixture=await startFixture();
  try {
    await page.goto(fixture.url);
    await expect(page.getByRole('textbox',{name:'Message'})).toBeEnabled();
    await page.getByRole('button',{name:'Applications',exact:true}).click();
    await page.getByRole('button',{name:'Start',exact:true}).click();
    const frame=page.frameLocator('iframe[title="Demo View"]');
    await expect(frame.getByRole('heading')).toHaveText('GeWu connected');
    await expect(page.locator('iframe[title="Demo View"]')).toHaveAttribute('sandbox','allow-scripts');
    await frame.getByRole('button',{name:'Read native service'}).click();
    await expect(frame.getByRole('heading')).toHaveText('Native service: 7');
    await frame.getByRole('button',{name:'Read native stream'}).click();
    await expect(frame.getByRole('heading')).toHaveText('First event arrived');
    fixture.finishNativeStream();
    await expect(frame.getByRole('heading')).toHaveText('Last event arrived');
    await frame.getByRole('button',{name:'Save view state'}).click();
    await expect(frame.getByRole('heading')).toHaveText('View state saved');
    await page.reload();
    await expect(frame.getByRole('heading')).toHaveText('Saved state restored');
    const asset=await page.locator('iframe[title="Demo View"]').getAttribute('src');
    await page.getByRole('button',{name:'Close view',exact:true}).click();
    await expect.poll(async()=> (await page.request.get(asset)).status()).toBe(404);
  } finally { await fixture.close(); }
});

test('cancels a running tool and replays its settled state after browser reconnect', async ({page})=>{
  const fixture=await startFixture();
  try {
    await page.goto(fixture.url);
    await expect(page.getByRole('textbox',{name:'Message'})).toBeEnabled();
    await page.getByRole('button',{name:'Applications',exact:true}).click();
    await page.getByRole('button',{name:'Start',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected');
    await page.getByRole('button',{name:'Services',exact:true}).click();
    page.once('dialog',dialog=>dialog.accept('{}'));
    await page.getByRole('button',{name:'wait',exact:true}).click();
    await expect(page.getByText(/wait · running · Waiting/)).toBeVisible();
    await page.getByRole('button',{name:'Cancel',exact:true}).click();
    await expect(page.getByText(/wait · cancelled/)).toBeVisible();
    await page.reload();
    await page.getByRole('button',{name:'Services',exact:true}).click();
    await expect(page.getByText(/wait · cancelled/)).toBeVisible();
  } finally { await fixture.close(); }
});
