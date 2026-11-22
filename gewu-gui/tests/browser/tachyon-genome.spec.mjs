import {expect, test} from '@playwright/test';
import {openPreview, startFixture} from './fixtures.mjs';

test.skip(!process.env.TACHYON_CONTEXT_PACKAGE || !process.env.TACHYON_TEST_BROWSER || !process.env.TACHYON_REAL_DATASETS,
  'provide the staged package, Browser and real dataset root');

test('1M ATAC search, binding and scientific-view tool render real genome signal', async ({page}) => {
  test.setTimeout(120000);
  const fixture = await startFixture({packagePath:process.env.TACHYON_CONTEXT_PACKAGE, settings:{service:{
    kind:'managed',endpoint:'http://127.0.0.1:0',executable:process.env.TACHYON_TEST_BROWSER,
    args:['--datasets',process.env.TACHYON_REAL_DATASETS],
  }}});
  const queries = [];
  page.on('response', async response => {
    if (!response.url().includes('/plugin-data/') || response.request().method() !== 'POST') return;
    const data = await response.json().catch(()=>null);
    if (Array.isArray(data?.rows) && Array.isArray(data?.errors)) queries.push({...data,request:response.request().postDataJSON()});
  });
  const lastResult = () => {
    const message = fixture.requests.at(-1).messages.filter(message=>message.role === 'tool').at(-1);
    const text = typeof message.content === 'string' ? message.content : message.content.map(part=>part.text ?? '').join('');
    return JSON.parse(text.split('\n[GeWu plugin result:')[0]);
  };
  const ask = async (tool,args) => {
    const count = await page.getByText('The adapter returned a result.',{exact:true}).count();
    const prompt = page.getByRole('textbox',{name:'Message'});
    await expect(prompt).toBeEnabled();
    await prompt.fill(`call ${tool} ${JSON.stringify(args)}`);
    await page.getByRole('button',{name:/^Send/}).click();
    await expect(page.getByText('The adapter returned a result.',{exact:true})).toHaveCount(count+1,{timeout:30000});
    return lastResult();
  };
  try {
    await page.goto(fixture.url);
    const opened = await ask('plugin_open',{pluginId:'tachyon'});
    expect(opened.instructions[0].complete).toBe(true);
    expect(opened.tools).toContain('canvas.view.upsert');
    expect(opened.tools).not.toContain('workflow.run');
    const catalog = await ask('catalog.list',{category:'dataset',query:'1m atac'});
    expect(catalog.data.total).toBe(1);
    const entry = catalog.data.entries[0];
    expect(entry.uri).toContain('pseudobulk1m_atac');
    expect(entry.capabilities).not.toContain('workspace_describe');
    const target = {uri:entry.uri,type:'tachyon.dataset',revision:entry.revision};
    await ask('context.update',{expected_revision:0,datasets:[target]});
    await openPreview(page);
    const canvas = page.frameLocator('iframe[title="Tachyon Workspace"]');
    await expect(canvas.locator('.react-flow')).toBeVisible();
    await ask('canvas.view.upsert',{command_id:'genome-create',id:'genome-atac',component:'genome',target,
      expected_context_revision:1,expected_canvas_revision:0,config:{}});
    const node = canvas.locator('[data-workspace-component="genome"]');
    await expect(node.getByRole('textbox',{name:'Genome locus'})).toHaveValue('chr1:1-100,000',{timeout:20000});
    await expect(node.locator('.tc-workspace-view-status')).toHaveText('ready',{timeout:45000});
    await expect.poll(()=>queries.length).toBeGreaterThan(0);
    const tracks = queries.at(-1).rows.filter(row=>row.kind === 'track');
    expect(queries.at(-1).errors).toEqual([]);
    expect(tracks.some(row=>row.track?.values?.some(value=>value>0))).toBe(true);
    expect(await node.locator('.tb-genome-row-canvas').first().evaluate(el=>el.width)).toBeGreaterThan(200);
    let saved = await ask('canvas.get',{});
    expect(saved.data.snapshot.nodes).toHaveLength(1);
    expect(saved.data.render.status).toBe('ready');
    const beforeWide = queries.length;
    const wideStart = Date.now();
    await ask('canvas.view.upsert',{command_id:'genome-wide',id:'genome-atac',component:'genome',target,
      expected_context_revision:1,expected_canvas_revision:saved.data.snapshot.revision,
      config:{locus:{chr:'chr1',start:1,end:50000000}}});
    await expect.poll(()=>queries.length,{timeout:15000}).toBeGreaterThan(beforeWide);
    await expect(node.locator('.tc-workspace-view-status')).toHaveText('ready',{timeout:15000});
    expect(queries.at(-1).request.end).toBe(50000000);
    expect(queries.at(-1).stats.provider).toBe('ucf-cell-signal');
    expect(queries.at(-1).stats.chunks_read).toBe(0);
    expect(Date.now()-wideStart).toBeLessThan(15000);
    saved = await ask('canvas.get',{});
    expect(saved.data.render.status).toBe('ready');
    await test.info().attach('genome',{body:await page.screenshot(),contentType:'image/png'});

    // A failed data query must not be reported as a rendered browser.
    await page.route('**/plugin-data/**', async route => {
      const body = route.request().postDataJSON();
      if (body?.chrom && Array.isArray(body?.rows)) return route.fulfill({status:503,contentType:'application/json',body:'{"error":"injected genome failure"}'});
      await route.continue();
    });
    await ask('canvas.view.upsert',{command_id:'genome-error',id:'genome-atac',component:'genome',target,
      expected_context_revision:1,expected_canvas_revision:saved.data.snapshot.revision,
      config:{locus:{chr:'chr1',start:1000000,end:1200000}}});
    await expect(node.locator('.tc-workspace-view-status')).toHaveText('error',{timeout:15000});
    const failed = await ask('canvas.get',{});
    expect(failed.data.render.status).toBe('error');
    expect(failed.data.render.nodes[0].message).toContain('injected genome failure');
    expect(failed.data.snapshot.nodes).toHaveLength(1);
  } finally {
    await test.info().attach('fixture-log',{body:fixture.log(),contentType:'text/plain'});
    await fixture.close();
  }
});
