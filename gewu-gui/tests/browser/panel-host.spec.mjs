import {expect,test} from '@playwright/test';
import {appendFile} from 'node:fs/promises';
import {join} from 'node:path';
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
    // Chat emits tabs/snapshots. Neither may recreate the live iframe document.
    await frame.locator('body').evaluate(body => { body.dataset.live = 'original'; });
    const prompt = page.getByRole('textbox', {name:'Message'});
    await prompt.fill('Read the adapter evidence');
    await prompt.press('Enter');
    await expect(page.getByText('The adapter returned a result.', {exact:true})).toBeVisible();
    await expect(frame.locator('body')).toHaveAttribute('data-live', 'original');
    await frame.getByRole('button',{name:'Read native service'}).click();
    await expect(frame.getByRole('heading')).toHaveText('Native service: 7');
    await page.getByRole('button', {name:'Plugins',exact:true}).click();
    await page.getByText('Tools (4)', {exact:true}).click();
    await expect(page.locator('.plugin-tools').first()).toContainText('echo');
    await page.locator('.plugin-tools summary').filter({hasText:'wait · read'}).click();
    await expect(page.getByText('Wait in the fixture', {exact:true})).toBeVisible();
    await page.getByRole('button', {name:'Demo View',exact:true}).click();
    const originalAsset = await page.locator('iframe[title="Demo View"]').getAttribute('src');
    await appendFile(join(fixture.packagePath, 'web/index.html'), '<p>Updated web asset</p>');
    await page.getByRole('button', {name:'Reload view',exact:true}).click({timeout:5000});
    await expect(frame.getByRole('heading')).toHaveText('Saved state restored');
    await expect(frame.getByText('Updated web asset')).toBeVisible();
    await expect.poll(async()=> (await page.request.get(originalAsset)).status()).toBe(404);
    await page.reload();
    await openPreview(page);
    await expect(frame.getByRole('heading')).toHaveText('Saved state restored');
    const asset=await page.locator('iframe[title="Demo View"]').getAttribute('src');
    await page.getByRole('button',{name:'Close',exact:true}).click();
    await expect.poll(async()=> (await page.request.get(asset)).status()).toBe(404);
  } finally { await fixture.close(); }
});
