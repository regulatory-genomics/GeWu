import {expect, test} from '@playwright/test';
import {openPreview, startFixture} from './fixtures.mjs';

test('an agent can use a headless provider without opening Preview', async ({page}) => {
  const fixture = await startFixture({contextProvider:true});
  try {
    await page.goto(fixture.url);
    const prompt = page.getByRole('textbox', {name:'Message'});
    await prompt.fill('Read the adapter evidence');
    await prompt.press('Enter');
    await expect(page.getByText('No plugin tool selected.', {exact:true})).toBeVisible();
    expect(JSON.stringify(fixture.requests[0].messages)).not.toContain('gewu.application-context/1');
    await prompt.fill('call plugin_activate {"pluginId":"demo"}');
    await prompt.press('Enter');
    await expect(page.getByText('The adapter returned a result.', {exact:true})).toBeVisible();
    expect(JSON.stringify(fixture.requests.at(-1).messages)).toContain('gewu.application-context/1');

    await openPreview(page);
    const row = page.locator('.plugin-sub').filter({has:page.getByRole('button',{name:'Demo View',exact:true})});
    await expect(row.getByRole('button',{name:'Stop',exact:true})).toBeVisible();
    await expect(page.locator('iframe[title="Demo View"]')).toHaveCount(0);
    await row.getByRole('button',{name:'Demo View',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected');
    await page.getByRole('button',{name:'Plugins',exact:true}).click();
    await row.getByRole('button',{name:'Demo Context',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Demo Context"]').getByRole('heading')).toHaveText('GeWu connected');
    await page.getByRole('navigation',{name:'Application views'}).getByRole('button',{name:'Demo View',exact:true}).click();
    await expect(page.locator('iframe[title="Demo View"]')).toBeVisible();
    await expect(page.locator('iframe[title="Demo Context"]')).toBeVisible();
  } finally { await fixture.close(); }
});
