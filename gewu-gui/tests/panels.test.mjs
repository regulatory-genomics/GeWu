import assert from 'node:assert/strict';
import test from 'node:test';
import {dismissedKey, marketplaceStatus, runtimeOptionLabel, stateKey} from '../assets/panels.mjs';

const scope = (workspace, session) => ({workspace, session_id: session, id: 1});
const instance = (id, pluginId, uri = null) => ({id, pluginId, bindingResource: uri ? {uri} : null});

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
