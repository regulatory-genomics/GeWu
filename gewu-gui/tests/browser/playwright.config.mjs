import {defineConfig} from '@playwright/test';

export default defineConfig({testDir:'.', workers:1, timeout:45000, reporter:'line'});
