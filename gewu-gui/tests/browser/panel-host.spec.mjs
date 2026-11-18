import {expect,test} from '@playwright/test';
import {openPreview,startFixture} from './fixtures.mjs';

test('binds an opaque frame, streams native HTTP, restores state and revokes stale assets', async ({page})=>{
  const fixture=await startFixture();
  try {
    await page.goto(fixture.url);
    await expect(page.getByRole('textbox',{name:'Message'})).toBeEnabled();
    await openPreview(page);
    await page.getByRole('button',{name:'Open…',exact:true}).click();
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
    await openPreview(page);
    await expect(frame.getByRole('heading')).toHaveText('Saved state restored');
    const asset=await page.locator('iframe[title="Demo View"]').getAttribute('src');
    await page.getByRole('button',{name:'Close',exact:true}).click();
    await expect.poll(async()=> (await page.request.get(asset)).status()).toBe(404);
  } finally { await fixture.close(); }
});
