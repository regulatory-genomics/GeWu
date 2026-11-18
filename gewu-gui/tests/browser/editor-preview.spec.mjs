import {expect,test} from '@playwright/test';
import {readFile,writeFile} from 'node:fs/promises';
import {join} from 'node:path';
import {startFixture} from './fixtures.mjs';

test('editor saves workspace text and refuses a stale second edit', async ({page})=>{
  const fixture=await startFixture();
  try {
    await page.goto(fixture.url);
    await expect(page.getByRole('textbox',{name:'Message'})).toBeEnabled();
    await page.getByRole('button',{name:'Applications',exact:true}).click();
    await page.getByRole('button',{name:'Start',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected');
    await page.getByRole('button',{name:'Services',exact:true}).click();
    page.once('dialog',dialog=>dialog.accept('example.txt'));
    await page.getByRole('button',{name:'Source',exact:true}).click();
    const text=page.getByRole('textbox',{name:'File content'});
    await expect(text).toHaveValue('original text');
    await text.fill('from the editor');
    await page.getByRole('button',{name:'Save',exact:true}).click();
    await expect(page.getByText('Saved',{exact:true})).toBeVisible();
    const path=join(fixture.root,'example.txt');
    expect(await readFile(path,'utf8')).toBe('from the editor');
    await writeFile(path,'external edit');
    await text.fill('would overwrite');
    await page.getByRole('button',{name:'Save',exact:true}).click();
    await expect(page.getByText(/REVISION_CONFLICT/)).toBeVisible();
    expect(await readFile(path,'utf8')).toBe('external edit');
    expect(await readFile(join(fixture.other,'example.txt'),'utf8')).toBe('original text');
    await page.getByRole('button',{name:'File changes',exact:true}).click();
    await page.getByRole('button',{name:'Refresh changes'}).click();
    await page.getByText('workspace:///example.txt · editor',{exact:true}).click();
    await expect(page.locator('#plugin-host pre')).toContainText('+from the editor');
    await expect(page.locator('#plugin-host pre')).toContainText('-original text');
  } finally {await fixture.close();}
});
