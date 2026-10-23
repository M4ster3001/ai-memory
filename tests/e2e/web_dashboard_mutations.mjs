#!/usr/bin/env node
// A passing baseline is required before a deliberate behavioral fault can count.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const manifest = {baseline: null, mutants: []};
const faults = [
  ['all-panels-visible', 'tabs'],
  ['ignore-recipient-workspace', 'recipients'],
  ['ignore-recipient-project', 'recipients'],
  ['retain-write-token', 'delivery'],
  ['disable-live-refresh', 'live-usage'],
  ['forget-theme', 'themes'],
  ['steal-browser-shortcut', 'tabs'],
  ['enable-send-while-pending', 'delivery'],
  ['enable-send-for-empty-project-after-request', 'delivery'],
  ['disable-release-auto-submit', 'documentation'],
  ['restore-stale-release-on-history', 'documentation'],
  ['show-false-inbox-alert', 'mailbox-context'],
  ['ignore-mailbox-workspace', 'mailbox-context'],
];

async function run(mutation) {
  const child = spawn(process.execPath, [path.join(here, 'web_dashboard_browser.mjs'), ...(mutation ? ['--mutation=' + mutation] : [])], {env: process.env});
  let output = '', errors = '';
  child.stdout.on('data', data => {output += data;});
  child.stderr.on('data', data => {errors += data;});
  const timer = setTimeout(() => child.kill('SIGKILL'), 90000);
  const code = await new Promise((resolve, reject) => {child.on('error', reject);child.on('exit', resolve);});
  clearTimeout(timer);
  const directory = output.match(/^Artifacts: (.+)$/m)?.[1];
  assert(directory, 'Browser runner did not produce a report: ' + errors.slice(-500));
  return {code, directory, result: JSON.parse(fs.readFileSync(path.join(directory, 'report.json'), 'utf8'))};
}

try {
  const baseline = await run();
  assert.equal(baseline.code, 0, 'Baseline must pass; inspect ' + baseline.directory);
  assert(baseline.result.cases.length >= 8 && baseline.result.cases.every(c => c.passed));
  manifest.baseline = baseline.directory;
  console.log('PASS unmodified server and browser baseline');
  for (const [fault, name] of faults) {
    const mutant = await run(fault);
    assert.equal(mutant.code, 1, fault + ' survived or the runner crashed');
    assert(mutant.result.mutationApplied > 0, fault + ' never reached the served application');
    assert.equal(mutant.result.error, 'Browser regression checks failed', 'An unrelated failure must not count as a killed mutant');
    assert(mutant.result.cases.some(c => c.name === name && c.passed === false), fault + ' escaped its behavioral test');
    manifest.mutants.push({fault, caughtBy: name, killed: true, artifacts: mutant.directory});
    console.log('KILLED ' + fault + ' by ' + name);
  }
} catch (error) {manifest.error = error.message;console.error(error.message);process.exitCode = 1;}
finally {
  const output = path.resolve(here, '../../target/web-dashboard-mutations.json');
  fs.mkdirSync(path.dirname(output), {recursive: true});
  fs.writeFileSync(output, JSON.stringify(manifest, null, 2));
  console.log('Mutation report: ' + output);
}
