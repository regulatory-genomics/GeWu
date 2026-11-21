import {pathToFileURL} from 'node:url';
import {expect,test} from '@playwright/test';
import {startFixture} from './fixtures.mjs';

if (process.env.GEWU_APP_SPEC || process.env.GEWU_APP_PLUGIN) {
  if (!process.env.GEWU_APP_SPEC || !process.env.GEWU_APP_PLUGIN) throw Error('GEWU_APP_SPEC and GEWU_APP_PLUGIN must be supplied together');
  const module=await import(pathToFileURL(process.env.GEWU_APP_SPEC).href);
  if (typeof module.register !== 'function') throw Error('Application spec must export register({test, expect, startFixture})');
  module.register({test,expect,startFixture:(options={})=>startFixture({packagePath:process.env.GEWU_APP_PLUGIN,...options})});
}
