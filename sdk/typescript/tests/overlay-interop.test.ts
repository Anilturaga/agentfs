import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { beforeEach, describe, expect, it } from 'vitest';
import { createOverlay, openMerged } from '../src/overlay/node.js';

const cases = JSON.parse(readFileSync(fileURLToPath(new URL('../../../conformance/overlay/cases.json', import.meta.url)), 'utf8'));

/** `npm run test:interop` sets INTEROP; a plain `npm test` skips this file. */
describe.skipIf(!process.env.INTEROP)('overlay interop with the agentfs CLI', () => {
  const bin = process.env.AGENTFS_BIN ?? '';
  let root: string;
  let base: string;

  const cli = (...args: string[]) => execFileSync(bin, args, { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
  const changes = (text: string) => text.split('\n').filter((l) => /^[AMD] [fdl?] \//.test(l));
  const text = (c: { change: string; type: string; path: string }[]) => c.map((x) => `${x.change} ${x.type} ${x.path}`);

  beforeEach(() => {
    expect(bin, 'AGENTFS_BIN must name the agentfs binary').not.toBe('');
    root = mkdtempSync(join(tmpdir(), 'overlay-interop-'));
    base = join(root, 'base');
    mkdirSync(base);
    execFileSync('sh', ['-ec', cases.base], { cwd: base });
  });

  it('reads what the CLI wrote and lists the same changes', async () => {
    cli('init', '--base', base, 'r');
    cli('fs', 'r', 'write', '/d/x', 'x2');
    cli('fs', 'r', 'write', '/n', '1');
    const expected = changes(cli('diff', 'r'));
    expect(expected).toEqual(['M d /d', 'M f /d/x', 'A f /n']);
    const fs = await openMerged(join(root, '.agentfs/r.db'));
    try {
      expect(await Promise.all(['/d/x', '/n', '/f'].map((p) => fs.readFile(p, 'utf8')))).toEqual(['x2', '1', 'base']);
      expect(text(await fs.diff())).toEqual(expected);
    } finally {
      await fs.close();
    }
  });

  it('is read by the CLI after the SDK wrote', async () => {
    mkdirSync(join(root, '.agentfs'));
    const db = join(root, '.agentfs/t.db');
    await createOverlay(db, base);
    const fs = await openMerged(db, base);
    await fs.unlink('/f');
    await fs.writeFile('/d/x', 'x2');
    await fs.rename('/d/sub', '/d/s2');
    await fs.symlink('d/x', '/a');
    const expected = text(await fs.diff());
    await fs.close();

    expect(cli('fs', db, 'cat', '/d/x')).toBe('x2');
    expect(cli('fs', db, 'cat', '/d/s2/y')).toBe('y');
    expect(cli('fs', db, 'ls', '/').split('\n')).not.toContain('f f');
    expect(changes(cli('diff', db))).toEqual(expected);
  });
});
