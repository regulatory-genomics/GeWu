import assert from 'node:assert/strict';
import test from 'node:test';
import {dismissedKey, installProgress, latestReleases, marketplaceStatus, runtimeOptionLabel, stateKey} from '../assets/panels.mjs';
import * as panels from '../assets/panels.mjs';

const scope = (workspace, session) => ({workspace, session_id: session, id: 1});
const instance = (id, pluginId, uri = null) => ({id, pluginId, bindingResource: uri ? {uri} : null});

test('source installation handoff pins the release without choosing an undeclared runtime', () => {
  assert.equal(typeof panels.sourceInstallInstructions, 'function');
  const release = {plugin: 'demo', version: '1.2.3', channel: 'beta', releaseId: 'demo-1.2.3',
    package: {sha256: 'b'.repeat(64)}, provenance: {source: 'https://github.com/example/demo', sourceCommit: 'a'.repeat(40),
      buildInputs: ['installation-guide=https://example.org/INSTALL.md', 'plugin-source=https://example.org/adapter',
        'runtime-recipe=https://user:secret@example.org/recipe', 'application-source=javascript:alert(1)']}};
  const text = panels.sourceInstallInstructions(release);
  assert.match(text, /demo 1\.2\.3 \(beta\)/);
  assert.ok(text.includes('a'.repeat(40)));
  assert.ok(text.includes('b'.repeat(64)));
  assert.match(text, /https:\/\/example.org\/INSTALL.md/);
  assert.match(text, /https:\/\/example.org\/adapter/);
  assert.doesNotMatch(text, /secret|javascript:|--runtime managed|--runtime bundled/);
  assert.match(text, /--resource-type/);
  assert.match(text, /gewu plugin install/);
  assert.match(panels.sourceInstallInstructions({plugin: 'legacy', version: '0.1.0', channel: 'stable'}), /not supplied/);
});

test('normal handoff installs the selected signed release with Auto, without source prerequisites', () => {
  const text = panels.installInstructions({plugin:'gewu-browser', version:'0.1.2', channel:'beta',
    runtimePlan:{policies:['external', 'bundled']}});
  assert.match(text, /gewu plugin marketplace-install gewu-browser --version 0\.1\.2 --channel beta --runtime auto/);
  assert.match(text, /external → bundled/);
  assert.doesNotMatch(text, /SDK|staged-package|Build the application/);
  assert.match(panels.installInstructions({plugin:'demo', version:'1.0.0', channel:'stable'}), /Runtime order: bundled/);
});

test('install progress uses real download bytes inside the runtime phase', () => {
  const partial = installProgress({stage: 'runtime_download', progress: {completedBytes: 163_916_475, totalBytes: 327_832_950}});
  assert.equal(partial.indeterminate, false);
  assert.equal(partial.value, 50);
  assert.match(partial.label, /runtime/i);
  const complete = installProgress({stage: 'runtime_download', progress: {completedBytes: 327_832_950, totalBytes: 327_832_950}});
  assert.equal(complete.value, 75);
  const unknown = installProgress({stage: 'runtime_download'});
  assert.equal(unknown.indeterminate, true);
  assert.equal(installProgress({stage: 'runtime_probe'}).value, 92);
  assert.equal(installProgress({stage: 'registry_refresh'}).value, 100);
});

test('workspace and conversation delimiters do not alias saved panel state', () => {
  assert.notEqual(dismissedKey(scope('a:b', 'c')), dismissedKey(scope('a', 'b:c')));
  assert.notEqual(dismissedKey(scope('a', 's1')), dismissedKey(scope('a', 's2')));
});

test('view state is scoped per session document so sessions keep separate selections', () => {
  const one = stateKey(scope('w', 's'), instance('i1', 'manimble', 'workspace:///one'), {id: 'editor'});
  const two = stateKey(scope('w', 's'), instance('i2', 'manimble', 'workspace:///two'), {id: 'editor'});
  assert.notEqual(one, two);
  assert.notEqual(one, stateKey(scope('w', 'other-session'), instance('i1', 'manimble', 'workspace:///one'), {id: 'editor'}));
});

test('marketplace defaults to the newest stable release for each plugin', () => {
  const releases = [
    {plugin: 'manimble', version: '0.2.0', channel: 'stable'},
    {plugin: 'manimble', version: '0.2.1', channel: 'stable'},
    {plugin: 'manimble', version: '0.3.0', channel: 'beta'},
    {plugin: 'paper', version: '0.2.0', channel: 'stable'},
    {plugin: 'paper', version: '0.10.0', channel: 'stable'},
  ];
  assert.deepEqual(latestReleases(releases).map((release) => `${release.plugin}@${release.version}`), [
    'manimble@0.2.1',
    'paper@0.10.0',
  ]);
  assert.deepEqual(latestReleases([{plugin: 'demo', version: '1.0.0', channel: 'beta'}]).map((release) => release.version), ['1.0.0']);
});

test('latest release uses SemVer precedence, including prerelease identifiers', () => {
  const versions = ['1.0.0-alpha', '1.0.0-alpha.1', '1.0.0-alpha.beta', '1.0.0-beta',
    '1.0.0-beta.2', '1.0.0-beta.11', '1.0.0-rc.1', '1.0.0', '1.0.1', '1.10.0', '2.0.0'];
  for (let i = 1; i < versions.length; i++) {
    for (const pair of [[versions[i - 1], versions[i]], [versions[i], versions[i - 1]]]) {
      assert.equal(latestReleases(pair.map(version => ({plugin: 'demo', version, channel: 'stable'})))[0].version, versions[i]);
    }
  }
});

test('marketplace metadata exposes provider choices and installation state', () => {
  assert.equal(runtimeOptionLabel({kind: 'managed', manager: 'micromamba', detail: 'micromamba detected'}), 'Managed · micromamba · micromamba detected');
  assert.equal(marketplaceStatus({plugin: 'paper', releaseId: 'paper-0.2.0', version: '0.2.0'}, {
    paper: {activeGeneration: 'paper-0.2.0', runtime: {kind: 'bundled'}},
  }), 'Installed · bundled');
  assert.equal(marketplaceStatus({plugin: 'paper', releaseId: 'paper-0.2.0', version: '0.2.0'}, {}, [
    {id: 'paper', version: '0.2.0'},
  ]), 'Installed locally · 0.2.0');
  assert.equal(marketplaceStatus({plugin: 'gewu-browser', releaseId: 'gewu-browser-0.1.1', version: '0.1.1'}, {
    'gewu-browser': {activeGeneration: 'gewu-browser-0.1.1'},
  }), 'Package installed · runtime not verified');
});
