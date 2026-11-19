import {expect,test} from '@playwright/test';
import {readFile,writeFile} from 'node:fs/promises';
import {join} from 'node:path';
import {openPreview,startFixture} from './fixtures.mjs';

test('a files click renders images in the dock instead of the text reader', async ({page})=>{
  const fixture=await startFixture();
  try {
    // 1x1 transparent PNG.
    await writeFile(join(fixture.root,'shot.png'),Buffer.from(
      'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==','base64'));
    await page.goto(fixture.url);
    await expect(page.getByRole('textbox',{name:'Message'})).toBeEnabled();
    await openPreview(page);
    await page.getByRole('tab',{name:'FILES'}).click();
    await page.getByRole('button',{name:'Refresh files'}).click();
    await page.locator('[data-path="shot.png"]').click();
    // The sidecar, not a full-screen overlay: a Preview tab titled with the file.
    const preview = page.locator('.plugin-panel:not([hidden])');
    await expect(preview.locator('img.preview-media')).toBeVisible();
    await expect(page.getByRole('navigation',{name:'Application views'}).getByRole('button',{name:'shot.png',exact:true})).toHaveAttribute('aria-pressed','true');
    const image = preview.locator('img.preview-media');
    await expect(image).toHaveAttribute('src',/^\/preview\//);
    // The binary never goes through the text protocol, so no protocol error.
    await expect(page.getByText(/protocol error|binary files cannot be previewed/)).toHaveCount(0);
    // Text files land in the same sidecar.
    await page.getByRole('tab',{name:'FILES'}).click();
    await page.locator('[data-path="example.txt"]').click();
    await expect(page.locator('.plugin-panel:not([hidden]) pre.preview-content')).toContainText('original text');
  } finally {await fixture.close();}
});

test('editor saves workspace text and refuses a stale second edit', async ({page})=>{
  const fixture=await startFixture();
  try {
    await page.goto(fixture.url);
    await expect(page.getByRole('textbox',{name:'Message'})).toBeEnabled();
    await openPreview(page);
    await page.locator('.plugin-row').filter({hasText:'Demo adapter'}).getByRole('button',{name:'Open…',exact:true}).click();
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected');
    await page.getByRole('button',{name:'Plugins',exact:true}).click();
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
    await page.getByRole('button',{name:'Changes',exact:true}).click();
    await page.getByRole('button',{name:'Refresh', exact:true}).click();
    await page.getByText('workspace:///example.txt · editor',{exact:true}).click();
    const diff = page.locator('.plugin-panel:not([hidden]) pre');
    await expect(diff).toContainText('+from the editor');
    await expect(diff).toContainText('-original text');
  } finally {await fixture.close();}
});
