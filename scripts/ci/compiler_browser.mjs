#!/usr/bin/env node
// Probe the exact installed Playwright and browser that the pinned suite uses.
// Retain the failure too; a failed launch is environment evidence, not a
// successful compiler regression or permission to disable the sandbox.
import { execFileSync } from 'node:child_process';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const upstream = path.join(root, '.ci/web/proto-ui');
const output = path.join(root, '.ci/evidence/compiler-browser.json');
const receipt = {
  status: 'preparing',
  candidate: process.env.GITHUB_SHA ?? null,
  uid: process.getuid?.() ?? null,
  node: process.version,
  runnerImage: process.env.ImageOS ?? null,
  runnerImageVersion: process.env.ImageVersion ?? null,
  executablePath: process.env.CHROME_PATH ?? null,
  chromiumSandbox: true,
  startedAt: new Date().toISOString(),
};
await mkdir(path.dirname(output), { recursive: true });
let browser;
try {
  if (receipt.uid === null || receipt.uid === 0) {
    throw new Error('The pinned Chromium suite requires a non-root Linux runner');
  }
  if (!receipt.executablePath) throw new Error('CHROME_PATH must select the actual browser');
  receipt.upstream = JSON.parse(await readFile(path.join(root, '.ci/web/inputs.json'), 'utf8')).pin;
  receipt.executableVersion = execFileSync(receipt.executablePath, ['--version'], { encoding: 'utf8' }).trim();
  const requireUpstream = createRequire(path.join(upstream, 'apps/www/package.json'));
  const { chromium } = requireUpstream('playwright-core');
  receipt.playwright = requireUpstream('playwright-core/package.json').version;
  browser = await chromium.launch({
    executablePath: receipt.executablePath,
    chromiumSandbox: true,
    headless: true,
    args: ['--disable-dev-shm-usage'],
  });
  receipt.browserVersion = browser.version();
  const page = await browser.newPage();
  await page.goto('chrome://sandbox');
  receipt.sandboxStatus = await page.locator('body').innerText();
  receipt.status = 'passed';
} catch (error) {
  receipt.status = 'failed';
  receipt.error = error instanceof Error ? error.stack ?? error.message : String(error);
  process.exitCode = 1;
} finally {
  try {
    await browser?.close();
  } catch (error) {
    receipt.status = 'failed';
    receipt.closeError = String(error);
    process.exitCode = 1;
  }
  receipt.finishedAt = new Date().toISOString();
  await writeFile(output, JSON.stringify(receipt, null, 2) + '\n');
  console.log(JSON.stringify(receipt, null, 2));
}
