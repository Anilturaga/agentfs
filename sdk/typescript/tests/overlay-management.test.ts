import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Database } from '@tursodatabase/database';
import { execFileSync, spawn } from 'node:child_process';
import { copyFileSync, existsSync, linkSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createServerlessAdapter } from '../src/integrations/serverless/index.js';
import { createOverlay, deleteOverlay, hostLower, listOverlays, openMerged, rebindOverlay } from '../src/overlay/node.js';
import {
  createOverlay as createOnHandle,
  openMerged as openOnHandle,
  rebindOverlay as rebindOnHandle,
  type OverlayFS,
} from '../src/overlay/overlayfs.js';

const cases = JSON.parse(readFileSync(fileURLToPath(new URL('../../../conformance/overlay/cases.json', import.meta.url)), 'utf8'));
const distNode = fileURLToPath(new URL('../dist/index_node.js', import.meta.url));
const legacyRust = fileURLToPath(new URL('../../rust/tests/fixtures/legacy_rust_overlay.db', import.meta.url));

const code = async (p: Promise<unknown>) => p.then(() => 'ok', (e) => e.code ?? e.message);

async function raw<T>(db: string, sql: string): Promise<T[]> {
  const conn = new Database(db);
  await conn.connect();
  try {
    return (await conn.prepare(sql).all()) as T[];
  } finally {
    await conn.close();
  }
}

describe('overlay management', () => {
  let root: string;
  let base: string;
  let db: string;
  let overlay: OverlayFS | undefined;

  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), 'overlay-mgmt-'));
    [base, db] = [join(root, 'base'), join(root, 'p.db')];
    mkdirSync(base);
    execFileSync('sh', ['-ec', cases.base], { cwd: base });
  });

  afterEach(async () => {
    vi.restoreAllMocks();
    await overlay?.close();
    overlay = undefined;
  });

  it('lists the overlays of a directory, filtered by base, and reads a held one through its instance', async () => {
    const other = join(root, 'other');
    mkdirSync(other);
    await createOverlay(join(root, 'b.db'), other);
    await createOverlay(db, base);
    writeFileSync(join(root, 'junk.db'), 'not a database');
    writeFileSync(join(root, 'p.txt'), 'x');
    overlay = await openMerged(db, base);

    const all = await listOverlays(root);
    expect(all.map((o) => o.path)).toEqual([join(root, 'b.db'), db]);
    expect(all[1]).toMatchObject({ version: 1, basePath: base });
    expect((await listOverlays(root, base)).map((o) => o.path)).toEqual([db]);
    expect((await listOverlays(root, other)).map((o) => o.path)).toEqual([join(root, 'b.db')]);
  });

  it('omits an overlay that another process holds open', async () => {
    await createOverlay(db, base);
    const script = `import { openMerged } from ${JSON.stringify(distNode)};
      await openMerged(${JSON.stringify(db)}); console.log('READY'); setInterval(() => {}, 1000);`;
    const child = spawn(process.execPath, ['--input-type=module', '-e', script], { stdio: ['ignore', 'pipe', 'inherit'] });
    try {
      await new Promise((resolve) => child.stdout.once('data', resolve));
      expect(await listOverlays(root)).toEqual([]);
      expect(await code(deleteOverlay(db))).toBe('EBUSY');
    } finally {
      child.kill('SIGKILL');
      await new Promise((resolve) => child.once('exit', resolve));
    }
    expect((await listOverlays(root)).map((o) => o.path)).toEqual([db]);
  });

  it('deletes the database and its sidecars, and refuses a held, missing or non-overlay database', async () => {
    await createOverlay(db, base);
    const files = ['-shm', '-wal-revert', '-info', '-changes'].map((s) => db + s);
    for (const f of files) writeFileSync(f, 'x');
    overlay = await openMerged(db, base);
    expect(await code(deleteOverlay(db))).toBe('EBUSY');
    await overlay.close();
    overlay = undefined;

    await deleteOverlay(db);
    expect([db, ...files].filter(existsSync)).toEqual([]);
    expect(await code(deleteOverlay(db))).toBe('ENOENT');

    const plain = new Database(join(root, 'plain.db'));
    await plain.connect();
    await plain.exec('CREATE TABLE t (a)');
    await plain.close();
    expect(await code(deleteOverlay(join(root, 'plain.db')))).toBe('EINVAL');
    expect(existsSync(join(root, 'plain.db'))).toBe(true);
  });

  it('refuses a hard-linked database before any write', async () => {
    await createOverlay(db, base);
    const link = join(root, 'link.db');
    linkSync(db, link);
    const [before, names] = [readFileSync(db), readdirSync(root)];
    const attempts = [openMerged(link), deleteOverlay(link), rebindOverlay(link, base), openMerged(db)];
    expect(await Promise.all(attempts.map(code))).toEqual(Array(4).fill('ENOTSUP'));
    expect(await listOverlays(root)).toEqual([]);
    expect(readFileSync(db).equals(before)).toBe(true);
    expect(readdirSync(root)).toEqual(names);
  });

  it('rebinds to a base, refuses a held database, and rejects other versions', async () => {
    const other = join(root, 'other');
    mkdirSync(other);
    await createOverlay(db, base);
    overlay = await openMerged(db, base);
    expect(await code(rebindOverlay(db, other))).toBe('EBUSY');
    expect(await overlay.readFile('/f', 'utf8')).toBe('base');
    await overlay.close();
    overlay = undefined;

    expect(await code(openMerged(db, other))).toBe('ESTALE');
    await rebindOverlay(db, other);
    expect(await code(openMerged(db, base))).toBe('ESTALE');
    expect((await listOverlays(root, other)).map((o) => o.path)).toEqual([db]);

    await raw(db, "UPDATE fs_overlay_config SET value = '2' WHERE key = 'version'");
    expect(await code(rebindOverlay(db, base))).toBe('ENOTSUP');
    await raw(db, "DELETE FROM fs_overlay_config WHERE key = 'version'");
    expect(await code(rebindOverlay(db, base))).toBe('EINVAL');
  });

  it('refuses a rebind through a second handle of a held database and keeps the overlay readable', async () => {
    const lower = await hostLower(base);
    const open = async () => {
      const d = new Database(db);
      await d.connect();
      return d;
    };
    await createOverlay(db, base);
    const [d1, d2] = [await open(), await open()];
    overlay = await openOnHandle(d1, lower);
    expect(await code(rebindOnHandle(d2, lower))).toBe('EBUSY');
    expect(await overlay.readFile('/f', 'utf8')).toBe('base');
    await d2.close();
  });

  it('rejects a legacy database with ENOTSUP and leaves it unchanged', async () => {
    const rust = join(root, 'rust.db');
    copyFileSync(legacyRust, rust);
    const before = readFileSync(rust);
    const err = await openMerged(rust, base).catch((e) => e);
    expect(err.code).toBe('ENOTSUP');
    expect(err.message).toContain('open it once with the agentfs CLI or Rust SDK to upgrade to format v1');
    expect(readFileSync(rust).equals(before)).toBe(true);

    const go = join(root, 'go.db');
    await raw(go, 'CREATE TABLE fs_whiteout (path TEXT PRIMARY KEY, parent_path TEXT NOT NULL, created_at INTEGER NOT NULL)');
    expect(await code(openMerged(go, base))).toBe('ENOTSUP');
  });

  it('rejects a regular file as base with ENOTDIR and keeps the old binding', async () => {
    const file = join(base, 'f');
    expect(await code(createOverlay(join(root, 'new.db'), file))).toBe('ENOTDIR');
    expect(existsSync(join(root, 'new.db'))).toBe(false);
    await createOverlay(db, base);
    expect(await code(rebindOverlay(db, file))).toBe('ENOTDIR');
    expect(await code(openMerged(db, file))).toBe('ENOTDIR');
    expect((await listOverlays(root, base)).map((o) => o.path)).toEqual([db]);
    overlay = await openMerged(db);
    expect(await overlay.readFile('/f', 'utf8')).toBe('base');
  });

  it('reserves the key while listing, so a concurrent open fails with EBUSY', async () => {
    await createOverlay(db, base);
    const connect = Database.prototype.connect;
    let reading!: () => void;
    let release!: () => void;
    const started = new Promise<void>((r) => (reading = r));
    const gate = new Promise<void>((r) => (release = r));
    vi.spyOn(Database.prototype, 'connect').mockImplementation(async function (this: Database) {
      reading();
      await gate;
      return connect.call(this);
    });
    const listing = listOverlays(root);
    await started;
    expect(await code(openMerged(db, base))).toBe('EBUSY');
    release();
    expect((await listing).map((o) => o.path)).toEqual([db]);
    overlay = await openMerged(db, base);
  });

  const replaceAfter = (method: 'connect' | 'close', path = db) => {
    const original = Database.prototype[method];
    vi.spyOn(Database.prototype, method).mockImplementation(async function (this: Database) {
      await original.call(this);
      writeFileSync(path + '.new', 'replacement');
      renameSync(path + '.new', path);
    });
  };

  it('refuses to use a database whose path names another file after the open', async () => {
    await createOverlay(db, base);
    replaceAfter('connect');
    expect(await code(rebindOverlay(db, base))).toBe('ENOENT');
    expect(readFileSync(db, 'utf8')).toBe('replacement');
  });

  it('does not create over, or list, a path that names another file after the open', async () => {
    replaceAfter('connect');
    expect(await code(createOverlay(db, base))).toBe('ENOENT');
    expect(readFileSync(db, 'utf8')).toBe('replacement');
    vi.restoreAllMocks();

    const listed = join(root, 'listed.db');
    await createOverlay(listed, base);
    replaceAfter('connect', listed);
    expect(await listOverlays(root)).toEqual([]);
    expect(readFileSync(listed, 'utf8')).toBe('replacement');
  });

  it('refuses to unlink a path that was replaced during deleteOverlay', async () => {
    await createOverlay(db, base);
    replaceAfter('close');
    expect(await code(deleteOverlay(db))).toBe('ENOENT');
    vi.restoreAllMocks();
    writeFileSync(db + '-wal', 'sidecar');
    expect([readFileSync(db, 'utf8'), readFileSync(db + '-wal', 'utf8')]).toEqual(['replacement', 'sidecar']);
  });

  it('rejects a lockless handle before any query and never closes it', async () => {
    const lower = await hostLower(base);
    const spy = { lockless: true, name: 'serverless', memory: false, prepare: vi.fn(), exec: vi.fn(), close: vi.fn() };
    const db = spy as unknown as Parameters<typeof createOnHandle>[0];
    const results = [createOnHandle(db, lower), openOnHandle(db, lower), rebindOnHandle(db, lower)];
    expect(await Promise.all(results.map(code))).toEqual(Array(3).fill('ENOTSUP'));
    expect([spy.prepare, spy.exec, spy.close].map((f) => f.mock.calls.length)).toEqual([0, 0, 0]);
    expect((createServerlessAdapter({} as never) as { lockless: boolean }).lockless).toBe(true);
  });
});
