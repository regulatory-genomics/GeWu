import {expect, test} from '@playwright/test';
import {mkdtemp, mkdir, writeFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {openPreview, startFixture} from './fixtures.mjs';

test.skip(!process.env.TACHYON_CONTEXT_PACKAGE || !process.env.TACHYON_TEST_BROWSER,
  'stage the matching Tachyon package and set TACHYON_CONTEXT_PACKAGE and TACHYON_TEST_BROWSER');

test('agent and GUI share bindings and one Canvas for two datasets', async ({page}) => {
  const datasets = await mkdtemp(join(tmpdir(),'tachyon-context-test-'));
  let fixture;
  try {
    for (const [id, value] of [['first', 'one'], ['second', 'two']]) {
      const root = join(datasets,id);
      await mkdir(join(root,'payloads'),{recursive:true});
      if (id === 'first') await writeFile(join(root,'observations.tsv'), `observation_id\tlabel\nsame-id\t${value}\n`);
      await writeFile(join(root,'payloads/fixture.ucf'), '');
      await writeFile(join(root,'dataset.json'),JSON.stringify({
        schema:'tachyon-data/3', id, title:id, snapshot_id:`sha256:${id}`,
        references:{genome:{assembly:'GRCh38',coordinates:'0-based-half-open'}},
        observation_axis:{id:'libraries',kind:'library',path:'observations.tsv',id_column:'observation_id',fields:['label']},
        resources:[{id:'events',kind:'native_events',format:'ucf',path:'payloads/fixture.ucf',assay:'ATAC',binding:{kind:'single',axis:'libraries',observation_id:'same-id'}}],
      }));
    }
    fixture = await startFixture({packagePath:process.env.TACHYON_CONTEXT_PACKAGE, settings:{service:{
      kind:'managed',endpoint:'http://127.0.0.1:0',executable:process.env.TACHYON_TEST_BROWSER,
      args:['--datasets',datasets],
    }}});
    await page.goto(fixture.url);
    const prompt = page.getByRole('textbox',{name:'Message'});
    const ask = async (tool,args) => {
      const count = await page.getByText('The adapter returned a result.',{exact:true}).count();
      const command = `call ${tool} ${JSON.stringify(args)}`;
      await expect(prompt).toBeEnabled();
      await prompt.fill(command);
      await page.getByRole('button',{name:/^Send/}).click();
      await expect(page.locator('main').getByText(command,{exact:false})).toBeVisible();
      await expect(page.getByText('The adapter returned a result.',{exact:true})).toHaveCount(count+1,{timeout:25000});
    };
    await ask('plugin_activate',{pluginId:'tachyon'});
    await ask('context.update',{expected_revision:0,datasets:[{uri:'tachyon://dataset/local/first',type:'tachyon.dataset',revision:'sha256:first'}]});
    // Legacy page tools cannot run against a session document; the agent must
    // see the backend Canvas tools instead.
    const advertised = fixture.requests.at(-1).tools.map(tool => tool.function.name);
    expect(advertised).not.toContain('canvas_add');
    expect(advertised).not.toContain('workspace_describe');
    expect(advertised.some(name => name.endsWith('.canvas.commit'))).toBe(true);
    await openPreview(page);
    await expect(page.locator('iframe[title="Tachyon Workspace"]')).toHaveCount(0);
    await page.getByRole('button',{name:'Tachyon Context',exact:true}).click();
    const sidebar = page.frameLocator('iframe[title="Tachyon Context"]');
    await expect(sidebar.getByRole('status')).toHaveText('Binding revision 1');
    await expect(sidebar.getByText('tachyon://dataset/local/first',{exact:false})).toBeVisible();
    await sidebar.getByText('second',{exact:true}).click();
    await sidebar.getByRole('button',{name:'Bind dataset'}).click();
    await expect(sidebar.getByRole('status')).toHaveText('Binding revision 2');
    await sidebar.getByRole('button',{name:'Open session Canvas'}).click();

    const canvas = page.frameLocator('iframe[title="Tachyon Workspace"]');
    // An agent commit made while the Canvas is open must appear through the
    // revision poll without a manual reload.
    await ask('canvas.commit',{command_id:'agent-add-view',expected_canvas_revision:0,expected_context_revision:2,edit:{op:'upsert_node',node:{
      id:'agent-view',type:'workspace_view',position:{x:320,y:40},width:640,height:480,
      data:{kind:'workspace_view',label:'Agent view',payload:{schema:'tachyon.workspace-view.v2',component:'cohort',
        target:{uri:'tachyon://dataset/local/first',type:'tachyon.dataset',revision:'sha256:first'},title:'Agent view',config:{}}}}}});
    await expect(canvas.getByText('Agent view',{exact:true})).toBeVisible({timeout:10000});
    const select = canvas.getByRole('combobox',{name:'Dataset for new view'});
    await expect(select.getByRole('option')).toHaveCount(2);
    const add = canvas.getByRole('button',{name:'Add dataset view'});
    await canvas.getByRole('combobox',{name:'Scientific view type'}).selectOption('cohort');
    for (const id of ['first','second']) {
      // The second axis was absent at startup, discovery, binding and the
      // first dataset's query. Only its own view may open it.
      if (id === 'second') await writeFile(join(datasets,id,'observations.tsv'), 'observation_id\tlabel\nsame-id\ttwo\n');
      await select.selectOption(`tachyon://dataset/local/${id}`);
      await add.click();
      await expect(canvas.getByText(`Cohort · ${id}`,{exact:true})).toBeVisible({timeout:15000});
    }
    await expect(canvas.getByRole('checkbox',{name:'one',exact:true}).first()).toBeVisible();
    await expect(canvas.getByRole('checkbox',{name:'two',exact:true}).first()).toBeVisible();
    await expect(sidebar.getByRole('status')).toHaveText('Binding revision 2');
    await page.getByRole('button',{name:'Close',exact:true}).click();
    await sidebar.getByRole('button',{name:'Open session Canvas'}).click();
    await expect(canvas.getByText('Cohort · first',{exact:true})).toBeVisible();
    await expect(canvas.getByText('Cohort · second',{exact:true})).toBeVisible();
    await sidebar.locator('p').filter({hasText:'tachyon://dataset/local/second'}).getByRole('button',{name:'Unbind'}).click();
    await expect(sidebar.getByRole('status')).toHaveText('Binding revision 3');
    await expect(canvas.getByRole('checkbox',{name:'two',exact:true})).toHaveCount(0);
    await expect(canvas.getByRole('checkbox',{name:'one',exact:true}).first()).toBeVisible();
    await expect(canvas.getByText('Cohort · second',{exact:true})).toBeVisible();
  } finally {
    await fixture?.close();
    await rm(datasets,{recursive:true,force:true});
  }
});
