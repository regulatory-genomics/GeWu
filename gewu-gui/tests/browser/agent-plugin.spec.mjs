import {expect,test} from '@playwright/test';
import {startFixture} from './fixtures.mjs';

test('a real agent turn calls the adapter and Plan does not advertise plugin tools', async ({page})=>{
  const fixture=await startFixture();
  try {
    await page.goto(fixture.url);
    await page.getByRole('button',{name:'Applications',exact:true}).click();
    await expect(page.getByRole('button',{name:'Start',exact:true})).toBeVisible();
    await page.getByRole('button',{name:'Start',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected');
    await page.getByRole('button',{name:'Hide panels'}).click();
    const prompt=page.getByRole('textbox',{name:'Message'});
    await prompt.fill('Read the adapter evidence');
    await prompt.press('Enter');
    await expect(page.getByText('The adapter returned a result.',{exact:true})).toBeVisible();
    expect(fixture.requests.some(turn=>turn.messages.some(m=>m.role==='tool' && m.content.includes('adapter evidence')))).toBeTruthy();
    await page.reload();
    await page.getByRole('button',{name:'Open application result'}).click();
    await expect(page.locator('#plugin-host pre')).toContainText('adapter evidence');
    await page.getByRole('button',{name:'Hide panels'}).click();
    await page.getByRole('button',{name:'plan',exact:true}).click();
    await expect(page.getByRole('button',{name:'plan',exact:true})).toHaveAttribute('aria-pressed','true');
    const first=fixture.requests.length;
    await prompt.fill('Do not invoke applications');
    await prompt.press('Enter');
    await expect.poll(()=>fixture.requests.length).toBeGreaterThan(first);
    expect(fixture.requests.slice(first).every(turn=>!turn.tools?.some(t=>t.function.name.startsWith('plugin_') || t.function.name.startsWith('plg_')))).toBeTruthy();
  } finally {await fixture.close();}
});
