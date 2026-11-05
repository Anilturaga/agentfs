import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { Database } from '@tursodatabase/database';
import { execFileSync, execFile } from 'node:child_process';
import { linkSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import { createOverlay, hostLower, openMerged } from '../src/overlay/node.js';
import {
  createOverlay as createOnHandle,
  openMerged as openOnHandle,
  type LowerAdapter,
  type OverlayFS,
} from '../src/overlay/overlayfs.js';

const cases = JSON.parse(readFileSync(fileURLToPath(new URL('../../../conformance/overlay/cases.json', import.meta.url)), 'utf8'));
const distNode = fileURLToPath(new URL('../dist/index_node.js', import.meta.url));

const code = async (p: Promise<unknown>) => p.then(() => 'ok', (e) => e.code ?? e.message);
const count = async (db: string, table: string) => {
  const conn = new Database(db);
  await conn.connect();
  const row = (await conn.prepare(`SELECT count(*) AS n FROM ${table}`).get()) as { n: number };
  await conn.close();
  return Number(row.n);
};

/** Opens `dbPath` in a child process; prints the error code or OK. */
async function childOpen(dbPath: string): Promise<string> {
  const script = `import { openMerged } from ${JSON.stringify(distNode)};
    try { await (await openMerged(${JSON.stringify(dbPath)})).close(); console.log('OK'); } catch (e) { console.log(e.code ?? e.message); }
    process.exit(0);`;
  const { stdout } = await promisify(execFile)(process.execPath, ['--input-type=module', '-e', script]);
  return stdout.trim();
}

describe('OverlayFS read path', () => {
  let root: string;
  let base: string;
  let db: string;
  let overlay: OverlayFS | undefined;

  beforeEach(async () => {
    root = mkdtempSync(join(tmpdir(), 'overlay-test-'));
    base = join(root, 'base');
    db = join(root, 'p.db');
    mkdirSync(base);
    execFileSync('sh', ['-ec', cases.base], { cwd: base });
    await createOverlay(db, base);
  });

  afterEach(async () => {
    await overlay?.close();
    overlay = undefined;
    rmSync(root, { recursive: true, force: true });
  });

  it('reads the merged view of the base', async () => {
    overlay = await openMerged(db, base);
    expect(await overlay.readdir('/')).toEqual(['d', 'e', 'f', 'l', 'p']);
    expect((await overlay.stat('/l')).isFile()).toBe(true);
    expect((await overlay.lstat('/l')).isSymbolicLink()).toBe(true);
    expect(await overlay.readFile('/l', 'utf8')).toBe('x');
    expect(await overlay.readlink('/l')).toBe('d/x');
    expect((await overlay.stat('/d')).mode & 0o7777).toBe(0o750);
    expect(await code(overlay.stat('/f/x'))).toBe('ENOTDIR');
    expect(await code(overlay.stat('/nope'))).toBe('ENOENT');
    expect((await overlay.stat('/d/../f')).ino).toBe((await overlay.stat('/f')).ino);
    const opened = await Promise.race([code(overlay.open('/p')), new Promise((r) => setTimeout(r, 1000, 'timeout'))]);
    expect(opened).toBe('ENOTSUP');
    expect(await overlay.diff()).toEqual([]);
  });

  it('reads through a handle and keeps stat identity', async () => {
    overlay = await openMerged(db, base);
    const rows = ['fs_inode', 'fs_dentry', 'fs_whiteout', 'fs_origin'];
    const before = await Promise.all(rows.map((t) => count(db, t)));
    const h = await overlay.open('/d/x');
    expect((await h.pread(0, 10)).toString()).toBe('x');
    expect((await h.fstat()).ino).toBe((await overlay.stat('/d/x')).ino);
    expect(await code(h.pwrite(0, Buffer.from('y')))).toBe('ENOSYS');
    expect((await overlay.stat('/')).ino).toBe(1);
    for (const e of await overlay.readdirPlus('/')) expect(e.stats.ino).toBe((await overlay.lstat(`/${e.name}`)).ino);
    expect(await Promise.all(rows.map((t) => count(db, t)))).toEqual(before);
  });

  it('resolves symlinks from the merged root and fails on loops', async () => {
    symlinkSync('/d', join(base, 'abs'));
    symlinkSync('b', join(base, 'a'));
    symlinkSync('a', join(base, 'b'));
    overlay = await openMerged(db, base);
    expect(await overlay.readFile('/abs/x', 'utf8')).toBe('x');
    expect(await overlay.readdir('/abs/sub/..')).toEqual(['sub', 'x']);
    expect(await code(overlay.stat('/a'))).toBe('ELOOP');
    expect((await overlay.lstat('/a')).isSymbolicLink()).toBe(true);
    expect(await code(overlay.readlink('/f'))).toBe('EINVAL');
  });

  it('sorts names by UTF-8 bytes', async () => {
    const [bmp, astral] = ['\uff5e', '\u{1f600}'];
    writeFileSync(join(base, bmp), '');
    writeFileSync(join(base, astral), '');
    overlay = await openMerged(db, base);
    expect((await overlay.readdir('/')).slice(-2)).toEqual([bmp, astral]);
  });

  it('diffs a large base without reading it', async () => {
    mkdirSync(join(base, 'big'));
    for (let i = 0; i < 10000; i++) writeFileSync(join(base, 'big', `f${i}`), '');
    const host = await hostLower(base);
    let names = 0;
    const counting: LowerAdapter = {
      ...host,
      readdir: async (p) => {
        const list = await host.readdir(p);
        names += list.length;
        return list;
      },
    };
    overlay = await openMerged(db, counting);
    expect(await overlay.diff()).toEqual([]);
    expect(names).toBeLessThan(100);
  });

  describe('writer lock', () => {
    it('refuses a second open of the same file and releases it on close', async () => {
      overlay = await openMerged(db, base);
      linkSync(db, join(root, 'hard.db'));
      symlinkSync(db, join(root, 'soft.db'));
      for (const p of [db, join(root, 'hard.db'), join(root, 'soft.db')]) {
        expect(await code(openMerged(p))).toBe('EBUSY');
        expect(await overlay.readFile('/f', 'utf8')).toBe('base');
      }
      expect(await childOpen(db)).toBe('EBUSY');

      await overlay.close();
      expect(await childOpen(db)).toBe('OK');
      const next = await openMerged(db);
      await overlay.close();
      expect(await next.readFile('/f', 'utf8')).toBe('base');
      await next.close();
      overlay = undefined;
    });

    it('keeps the active overlay open when a handle form retries', async () => {
      const lower = await hostLower(base);
      const handle = async () => {
        const d = new Database(db);
        await d.connect();
        return d;
      };
      const [d1, d2] = [await handle(), await handle()];
      overlay = await openOnHandle(d1, lower);
      expect(await code(openOnHandle(d1, lower))).toBe('EBUSY');
      expect(await overlay.readFile('/f', 'utf8')).toBe('base');
      expect(await code(openOnHandle(d2, lower))).toBe('EBUSY');
      const statement = d2.prepare('SELECT 1 AS one');
      expect(await statement.get()).toEqual({ one: 1 });
      expect(await overlay.readFile('/f', 'utf8')).toBe('base');

      statement.close();
      await d2.close();
      await overlay.close();
      expect(await code(Promise.resolve().then(() => d1.prepare('SELECT 1').get()))).not.toBe('ok');
      expect(await childOpen(db)).toBe('OK');
      overlay = undefined;
    });

    it('closes a handle whose open fails after the lock check', async () => {
      const other = join(root, 'other');
      mkdirSync(other);
      const d3 = new Database(db);
      await d3.connect();
      expect(await code(openOnHandle(d3, await hostLower(other)))).toBe('ESTALE');
      expect(await code(Promise.resolve().then(() => d3.prepare('SELECT 1').get()))).not.toBe('ok');
      expect(await childOpen(db)).toBe('OK');
    });
  });

  describe('management', () => {
    it('creates an overlay over a handle and refuses a database that has tables', async () => {
      const d = new Database(join(root, 'h.db'));
      await d.connect();
      const info = await createOnHandle(d, await hostLower(base));
      expect(info).toMatchObject({ version: 1, basePath: base });
      expect(info.baseIdentity).toMatch(/^host:\d+:\d+$/);
      expect(await code(createOnHandle(d, await hostLower(base)))).toBe('EEXIST');
      await d.close();
    });

    it('removes the database file when creation fails', async () => {
      expect(await code(createOverlay(join(root, 'x.db'), join(root, 'missing')))).toBe('ENOENT');
      expect(await code(openMerged(join(root, 'x.db')))).toBe('ENOENT');
      expect(await code(createOverlay(db, base))).toBe('EEXIST');
    });
  });
});
