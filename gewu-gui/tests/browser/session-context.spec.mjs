import {expect, test} from '@playwright/test';
import {openPreview, startFixture} from './fixtures.mjs';

test('agent plugin_open mounts Preview while the same turn is still running', async ({page}) => {
  let resume;
  const gate = new Promise(resolve => {resume=resolve;});
  let allowOpen;
  const opening = new Promise(resolve => {allowOpen=resolve;});
  let sawEmptyCatalog = false;
  page.on('websocket', socket => socket.on('framereceived', ({payload}) => {
    const message = JSON.parse(payload);
    if (message.type === 'plugin' && message.result?.instances?.length === 0) sawEmptyCatalog = true;
  }));
  const fixture = await startFixture({contextProvider:true, respond:async(turn,res)=>{
    const afterTool = turn.messages.some(message=>message.role === 'tool');
    res.writeHead(200,{'content-type':'text/event-stream'});
    const send = (delta,finish_reason) => res.write(`data: ${JSON.stringify({id:'open-fixture',object:'chat.completion.chunk',choices:[{index:0,delta,finish_reason}]})}\n\n`);
    if (!afterTool) {
      await opening;
      send({role:'assistant',tool_calls:[{index:0,id:'open-1',type:'function',function:{name:'plugin_open',arguments:'{"pluginId":"demo"}'}}]},null);
      send({},'tool_calls');
    } else {
      await gate;
      send({role:'assistant',content:'The application is visible.'},null);
      send({},'stop');
    }
    res.end('data: [DONE]\n\n');
  }});
  try {
    await page.goto(fixture.url);
    await page.getByRole('textbox',{name:'Message'}).fill('Open the demo and keep working.');
    sawEmptyCatalog = false;
    await page.getByRole('button',{name:/^Send/}).click();
    await expect.poll(()=>sawEmptyCatalog).toBe(true);
    allowOpen();
    await expect(page.frameLocator('iframe[title="Demo View"]').getByRole('heading')).toHaveText('GeWu connected',{timeout:10000});
    await expect(page.getByRole('tab',{name:'PREVIEW'})).toHaveAttribute('aria-selected','true');
    await expect(page.getByText('The application is visible.',{exact:true})).toHaveCount(0);
    resume();
    await expect(page.getByText('The application is visible.',{exact:true})).toBeVisible();
  } finally {allowOpen();resume();await fixture.close();}
});

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
