import assert from 'node:assert/strict';
import test from 'node:test';
import {dismissedKey, stateKey} from '../assets/panels.mjs';

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
