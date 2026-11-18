import assert from 'node:assert/strict';
import test from 'node:test';
import {layoutKey} from '../assets/panels.mjs';

test('workspace and conversation delimiters do not alias saved layouts',()=>{
  assert.notEqual(layoutKey('a:b','c'),layoutKey('a','b:c'));
  assert.notEqual(layoutKey('a','s1'),layoutKey('a','s2'));
});
