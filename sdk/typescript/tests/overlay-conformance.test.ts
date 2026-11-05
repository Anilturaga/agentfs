import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterAll, describe, expect, it } from 'vitest';

interface Case {
  id: number;
  xfail_ts?: number[];
  timeout?: number;
}

const tests = fileURLToPath(new URL('.', import.meta.url));
const cases: Case[] = JSON.parse(readFileSync(`${tests}../../../conformance/overlay/cases.json`, 'utf8')).cases;

interface Outcome {
  timedOut: boolean;
  code: number | null;
  signal: NodeJS.Signals | null;
  stderr: string;
}

/** Runs `cmd` in its own process group and kills the group at the deadline and after the child exits. */
function runGroup(cmd: string, args: string[], seconds: number): Promise<Outcome> {
  return new Promise((resolve) => {
    const child = spawn(cmd, args, { detached: true, stdio: ['ignore', 'ignore', 'pipe'] });
    const killGroup = () => {
      try {
        process.kill(-child.pid!, 'SIGKILL');
      } catch {}
    };
    let stderr = '';
    let timedOut = false;
    child.stderr!.on('data', (d) => (stderr += d));
    const timer = setTimeout(() => {
      timedOut = true;
      killGroup();
    }, seconds * 1000);
    child.on('exit', () => {
      clearTimeout(timer);
      killGroup();
    });
    child.on('close', (code, signal) => resolve({ timedOut, code, signal, stderr: stderr.trim() }));
  });
}

const tally = { pass: 0, xfail: 0, fail: 0 };
const fail = (message: string): never => {
  tally.fail++;
  throw new Error(message);
};

/** Only worker exit 1 is a semantic failure. A timeout, a signal, or any other exit code always fails the case. */
function judge(c: Case, r: Outcome): void {
  if (r.timedOut) fail(`case ${c.id} timed out`);
  if (r.code !== 0 && r.code !== 1) fail(`case ${c.id} runner error (code ${r.code}, signal ${r.signal}): ${r.stderr}`);
  const failed = r.code === 1;
  if (c.xfail_ts && !failed) fail(`case ${c.id} passes: remove xfail_ts ${JSON.stringify(c.xfail_ts)}`);
  if (!c.xfail_ts && failed) fail(`case ${c.id} fails: ${r.stderr}`);
  tally[c.xfail_ts ? 'xfail' : 'pass']++;
}

describe('overlay conformance', () => {
  afterAll(() => console.log(`pass ${tally.pass}, xfail ${tally.xfail}, fail ${tally.fail}`));

  cases.forEach((c, index) => {
    const seconds = c.timeout ?? 30;
    it(`case ${c.id}`, { timeout: (seconds + 10) * 1000 }, async () => {
      judge(c, await runGroup('node', [`${tests}overlay-conformance-worker.mjs`, String(index)], seconds));
    });
  });

  it('fails a timed-out worker even with xfail_ts and kills its whole process group', async () => {
    const marker = join(mkdtempSync(join(tmpdir(), 'group-')), 'M');
    const start = Date.now();
    const r = await runGroup('sh', ['-c', `: > ${marker}; (sleep 2; echo late > ${marker}) & wait`], 1);
    expect(Date.now() - start).toBeLessThan(2000);
    const before = tally.fail;
    expect(() => judge({ id: 0, xfail_ts: [41] }, r)).toThrow('timed out');
    tally.fail = before;
    await new Promise((resolve) => setTimeout(resolve, 3000));
    expect(statSync(marker).size).toBe(0);
  });
});
