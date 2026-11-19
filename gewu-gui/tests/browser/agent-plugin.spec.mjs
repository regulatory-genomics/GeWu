import {expect,test} from '@playwright/test';
import {openPreview,startFixture} from './fixtures.mjs';

test('a real agent turn calls the adapter and Plan does not advertise plugin tools', async ({page})=>{
  const fixture=await startFixture();
  try {
    await page.goto(fixture.url);
    await openPreview(page);
    const demo = page.locator('.plugin-row').filter({hasText:'Demo adapter'});
    await expect(demo.getByRole('button',{name:'Open…',exact:true})).toBeVisible();
    await demo.getByRole('button',{name:'Open…',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected');
    await page.getByRole('tab',{name:'FILES'}).click();
    const prompt=page.getByRole('textbox',{name:'Message'});
    await prompt.fill('Read the adapter evidence');
    await prompt.press('Enter');
    await expect(page.getByText('The adapter returned a result.',{exact:true})).toBeVisible();
    expect(fixture.requests.some(turn=>turn.messages.some(m=>m.role==='tool' && m.content.includes('adapter evidence')))).toBeTruthy();
    await page.reload();
    await page.getByRole('button',{name:'Open application result'}).click();
    await expect(page.locator('.plugin-panel:not([hidden]) pre')).toContainText('adapter evidence');
    await page.getByRole('tab',{name:'FILES'}).click();
    await page.getByRole('button',{name:'plan',exact:true}).click();
    await expect(page.getByRole('button',{name:'plan',exact:true})).toHaveAttribute('aria-pressed','true');
    const first=fixture.requests.length;
    await prompt.fill('Do not invoke applications');
    await prompt.press('Enter');
    await expect.poll(()=>fixture.requests.length).toBeGreaterThan(first);
    expect(fixture.requests.slice(first).every(turn=>!turn.tools?.some(t=>t.function.name.startsWith('plugin_') || t.function.name.startsWith('plg_')))).toBeTruthy();
  } finally {await fixture.close();}
});

test('a conversation opens the application in Preview without manual setup', async ({page})=>{
  const fixture=await startFixture();
  try {
    await page.goto(fixture.url);
    const prompt=page.getByRole('textbox',{name:'Message'});
    await expect(prompt).toBeEnabled();
    await prompt.fill('Please open the demo');
    await prompt.press('Enter');
    // The agent's plugin_open reaches the same host action the panel uses, so
    // the view mounts and the dock reveals itself with no Start click.
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected');
    await expect(page.getByRole('tab',{name:'PREVIEW'})).toHaveAttribute('aria-selected','true');
  } finally {await fixture.close();}
});

test('the conversation names a plugin tool and renders its result', async ({page})=>{
  const fixture=await startFixture();
  try {
    await page.goto(fixture.url);
    await expect(page.getByRole('textbox',{name:'Message'})).toBeEnabled();
    // Plugin tools reach the agent, never a button wall in the panels. They
    // exist only while the application instance is open.
    await openPreview(page);
    await page.locator('.plugin-row').filter({hasText:'Demo adapter'}).getByRole('button',{name:'Open…',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected');
    const prompt=page.getByRole('textbox',{name:'Message'});
    await prompt.fill('call echo {"value":9}');
    await prompt.press('Enter');
    await expect(page.locator('pre.tool-output').filter({hasText:'"value"'})).toContainText('9');
    await expect(page.getByRole('button',{name:'Open application result'})).toBeVisible();
  } finally {await fixture.close();}
});
