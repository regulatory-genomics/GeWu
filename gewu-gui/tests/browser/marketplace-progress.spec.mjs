import {test, expect} from '@playwright/test';
import {readFile} from 'node:fs/promises';

async function mount(page, restore = false) {
  await page.route('http://marketplace.test/**', async route => {
    const path = new URL(route.request().url()).pathname;
    if (path === '/') return route.fulfill({contentType:'text/html', body:'<div id="root"></div>'});
    if (!/^\/[a-z-]+\.mjs$/.test(path)) return route.fulfill({status:404});
    await route.fulfill({contentType:'text/javascript', body:await readFile(new URL(`../../assets${path}`, import.meta.url), 'utf8')});
  });
  await page.goto('http://marketplace.test/');
  await page.evaluate(async restore => {
    const {panelHost} = await import('/panels.mjs');
    window.reports = [];
    window.installs = [];
    window.finished = false;
    if (restore) localStorage.setItem('gewu.marketplace.install.v1:' + JSON.stringify(['w','s','demo','1.0.0','stable']), JSON.stringify({attemptId:'saved-attempt', startedAt:Date.now()}));
    const host = panelHost(document.getElementById('root'), async (_tab, action) => {
      if (action.action === 'list') return {plugins:window.finished ? [{id:'demo',version:'1.0.0',configured:true,enabled:true}] : [], instances:[]};
      if (action.action === 'marketplace_search') return {repository:'test', releases:[{plugin:'demo',releaseId:'demo-1.0.0',version:'1.0.0',channel:'stable'}]};
      if (action.action === 'marketplace_install') return new Promise((resolve,reject) => window.installs.push({resolve,reject}));
      if (action.action === 'marketplace_install_report') return new Promise((resolve,reject) => window.reports.push({resolve,reject,attemptId:action.attemptId}));
      return {};
    });
    await host.switchTab({id:'a', workspace:'w', session_id:'s'});
  }, restore);
  await page.getByRole('button', {name:'Marketplace', exact:true}).click();
}

test('a late running report cannot repaint a successful installation', async ({page}) => {
  await mount(page);
  await page.getByRole('button', {name:'Install', exact:true}).click();
  await page.waitForFunction(() => window.reports.length === 1 && window.installs.length === 1);
  await page.evaluate(() => {window.finished = true; window.installs[0].resolve({runtime:{kind:'bundled'}});});
  await expect(page.locator('.plugin-card-status')).toHaveText('Ready to open');
  await page.evaluate(async () => {window.reports[0].resolve({report:{outcome:'running',stage:'runtime_download',progress:{completedBytes:50,totalBytes:100}}}); await new Promise(resolve => setTimeout(resolve,0));});
  await expect(page.locator('progress')).toBeHidden();
  await expect(page.locator('.plugin-card-status')).toHaveText('Ready to open');
});

test('report polling waits for the previous reply', async ({page}) => {
  await page.clock.install();
  await mount(page);
  await page.getByRole('button', {name:'Install', exact:true}).click();
  await page.clock.runFor(2500);
  expect(await page.evaluate(() => window.reports.length)).toBe(1);
});

test('a restored failure keeps its repair controls and attempt identity', async ({page}) => {
  await mount(page, true);
  await page.waitForFunction(() => window.reports.length === 1);
  await page.evaluate(() => window.reports[0].resolve({report:{outcome:'failed',stage:'runtime_probe',error:'fixture probe failed'},markdown:'Pinned repair summary',detailsMarkdown:'Pinned repair details'}));
  await expect(page.getByRole('button', {name:'Retry', exact:true})).toBeEnabled();
  await page.getByRole('button', {name:'View details', exact:true}).click();
  await expect(page.getByRole('textbox', {name:'Agent repair information'})).toHaveValue('Pinned repair details');
  expect(await page.evaluate(() => Object.keys(localStorage).filter(key => key.startsWith('gewu.marketplace.install.v1:')).length)).toBe(1);
});

test('completed stages remain visible without download byte counts', async ({page}) => {
  await mount(page);
  await page.getByRole('button', {name:'Install', exact:true}).click();
  await page.waitForFunction(() => window.reports.length === 1);
  await page.evaluate(() => window.reports[0].resolve({report:{outcome:'running',stage:'runtime_probe'}}));
  await expect(page.locator('progress')).toHaveAttribute('value', '92');
  await expect(page.locator('progress')).toHaveAttribute('aria-valuetext', /stage.*not.*time/i);
});

test('an older install response cannot clear a newer retry', async ({page}) => {
  await mount(page);
  await page.getByRole('button', {name:'Install', exact:true}).click();
  await page.waitForFunction(() => window.reports.length === 1);
  await page.evaluate(() => window.reports[0].resolve({report:{outcome:'failed',error:'try again'}}));
  await page.getByRole('button', {name:'Retry', exact:true}).click();
  await page.waitForFunction(() => window.installs.length === 2 && window.reports.length === 2);
  await page.evaluate(async () => {window.installs[0].resolve({runtime:{kind:'bundled'}}); await new Promise(resolve => setTimeout(resolve,0));});
  await expect(page.locator('.plugin-badge')).toHaveText('Installing…');
  expect(await page.evaluate(() => JSON.parse(localStorage.getItem(Object.keys(localStorage).find(key => key.startsWith('gewu.marketplace.install.v1:')))).attemptId === window.reports[1].attemptId)).toBe(true);
});

test('an expired missing report leaves an actionable retry', async ({page}) => {
  await page.clock.install();
  await mount(page, true);
  await page.clock.runFor(31 * 60 * 1000);
  await page.evaluate(() => window.reports[0].reject(Error('install report not found')));
  await expect(page.getByRole('button', {name:'Retry', exact:true})).toBeEnabled();
  await expect(page.getByRole('button', {name:'View details', exact:true})).toBeVisible();
});
