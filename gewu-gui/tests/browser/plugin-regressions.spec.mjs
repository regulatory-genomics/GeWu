import {expect,test} from '@playwright/test';
import {writeFile} from 'node:fs/promises';
import {join} from 'node:path';
import {startFixture} from './fixtures.mjs';

for (const disabled of [false,true]) {
  test(`ordinary previews, attachments and sessions with plugin disabled=${disabled}`, async ({page})=>{
    const fixture=await startFixture({disabled});
    try {
      await writeFile(join(fixture.root,'preview.html'),'<h1>Workspace HTML preview</h1>');
      await page.goto(fixture.url);
      const prompt=page.getByRole('textbox',{name:'Message'});
      await expect(prompt).toBeEnabled();
      // Upstream defers the cwd tree until this tab owns a session.
      await prompt.fill('Initialize the conversation');
      await expect(page.getByRole('button',{name:/^Send/})).toBeEnabled();
      await prompt.press('Enter');
      await expect(page.getByText('No plugin tool selected.',{exact:true})).toBeVisible();
      const reveal=page.getByRole('button',{name:'Show files'});
      if (await reveal.isVisible()) await reveal.click();
      await page.getByRole('button',{name:'Refresh files'}).click();
      await page.locator('[data-path="example.txt"]').click();
      const overlay=page.getByRole('dialog',{name:'File preview',exact:true});
      await expect(overlay).toBeVisible();
      await expect(overlay.locator('pre')).toContainText('original text');
      await page.keyboard.press('Escape');
      await expect(overlay).toBeHidden();

      await page.locator('[data-path="preview.html"]').click({button:'right'});
      const [preview]=await Promise.all([
        page.waitForEvent('popup'),
        page.getByRole('menuitem',{name:'Preview',exact:true}).click(),
      ]);
      await expect(preview.getByRole('heading')).toHaveText('Workspace HTML preview');
      const response=await preview.request.get(preview.url());
      expect(response.headers()['content-security-policy']).toContain('sandbox');
      await preview.close();

      await page.locator('#attachment-upload').setInputFiles({
        name:'shot.png', mimeType:'image/png',
        buffer:Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==','base64'),
      });
      await expect(page.locator('#attachments')).toContainText('shot.png');
      await prompt.fill('Inspect this attachment');
      await prompt.press('Enter');
      await expect(page.locator('#attachments')).toBeHidden();
      await expect.poll(()=>fixture.requests.some(turn=>turn.messages.some(message=>
        message.role==='user' && Array.isArray(message.content) && message.content.some(part=>part.type==='image_url'),
      ))).toBe(true);
      await expect(prompt).toBeEnabled();
      const first=await page.locator('[data-session-tab]').first().getAttribute('data-session-tab');
      await page.locator('[data-session-new-workspace="cwd"]').click();
      await expect(page.locator('[data-session-tab]')).toHaveCount(2);
      await page.locator(`[data-session-tab="${first}"]`).click();
      await expect(page.locator('#transcript')).toContainText('Inspect this attachment');
    } finally {await fixture.close();}
  });
}
