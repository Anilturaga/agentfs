import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { Database } from '@tursodatabase/database';
import { execFileSync, execFile } from 'node:child_process';
import { chmodSync, linkSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
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

  it('keeps the directory requirement of symlink targets', async () => {
    symlinkSync('f/.', join(base, 'dot-file'));
    symlinkSync('f/', join(base, 'slash-file'));
    symlinkSync('d/.', join(base, 'dot-dir'));
    overlay = await openMerged(db, base);
    expect(await code(overlay.readFile('/dot-file'))).toBe('ENOTDIR');
    expect(await code(overlay.readFile('/slash-file'))).toBe('ENOTDIR');
    expect(await overlay.readFile('/dot-dir/x', 'utf8')).toBe('x');
    expect(await overlay.readdir('/dot-dir')).toEqual(['sub', 'x']);
  });

  it('keeps the instance inode when the lower reports its own', async () => {
    const host = await hostLower(base);
    const lower: LowerAdapter = { ...host, lstat: async (p) => ({ ...(await host.lstat(p))!, ino: 1 }) as any };
    overlay = await openMerged(db, lower);
    const ino = (await overlay.stat('/f')).ino;
    expect(ino).not.toBe(1);
    expect((await (await overlay.open('/f')).fstat()).ino).toBe(ino);
    expect((await overlay.readdirPlus('/')).find((e) => e.name === 'f')!.stats.ino).toBe(ino);
  });

  it('reads a populated delta: delta over whiteouts, opaque and merged directories, links, diff, reopen', async () => {
    mkdirSync(join(base, 'g'));
    writeFileSync(join(base, 'g', 'h'), 'h');
    const [FILE, DIR, LINK] = [0o100644, 0o040755, 0o120777];
    const inode = (ino: number, mode: number, size = 0, nlink = 1) =>
      `INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime) VALUES (${ino}, ${mode}, ${nlink}, 0, 0, ${size}, 1, 1, 1)`;
    const dentry = (parent: number, name: string, ino: number) =>
      `INSERT INTO fs_dentry (name, parent_ino, ino) VALUES ('${name}', ${parent}, ${ino})`;
    const data = (ino: number, text: string) => `INSERT INTO fs_data VALUES (${ino}, 0, X'${Buffer.from(text).toString('hex')}')`;
    const raw = new Database(db);
    await raw.connect();
    await raw.exec(
      [
        inode(10, FILE, 3), data(10, 'new'), dentry(1, 'f', 10), "INSERT INTO fs_whiteout VALUES ('/f', 1)",
        inode(11, DIR), dentry(1, 'd', 11), "INSERT INTO fs_whiteout VALUES ('/d', 1)",
        inode(12, FILE, 1), data(12, 'n'), dentry(11, 'n', 12),
        inode(13, DIR), dentry(1, 'g', 13), inode(14, FILE, 1), data(14, 'q'), dentry(13, 'n', 14),
        inode(15, FILE, 2, 2), data(15, 'hl'), dentry(1, 'a', 15), dentry(13, 'a2', 15),
        inode(16, LINK), "INSERT INTO fs_symlink VALUES (16, '/d/n')", dentry(1, 's', 16),
        "INSERT INTO fs_whiteout VALUES ('/l', 1)",
      ].join(';')
    );
    await raw.close();

    const expected = [
      'A f /a', 'M d /d', 'A f /d/n', 'D d /d/sub', 'D f /d/sub/y', 'D f /d/x',
      'M f /f', 'M d /g', 'A f /g/a2', 'A f /g/n', 'D l /l', 'A l /s',
    ];
    const check = async (o: OverlayFS) => {
      expect(await o.readdir('/')).toEqual(['a', 'd', 'e', 'f', 'g', 'p', 's']);
      expect(await o.readdir('/d')).toEqual(['n']);
      expect(await o.readdir('/g')).toEqual(['a2', 'h', 'n']);
      expect(await o.readFile('/f', 'utf8')).toBe('new');
      expect(await o.readFile('/s', 'utf8')).toBe('n');
      expect(await o.readFile('/g/h', 'utf8')).toBe('h');
      expect(await code(o.stat('/l'))).toBe('ENOENT');
      expect(await code(o.stat('/d/x'))).toBe('ENOENT');
      const [a, a2] = [await o.stat('/a'), await o.stat('/g/a2')];
      expect([a.ino, a.nlink]).toEqual([a2.ino, 2]);
      expect((await o.diff()).map((c) => `${c.change} ${c.type} ${c.path}`)).toEqual(expected);
    };
    overlay = await openMerged(db, base);
    await check(overlay);
    await overlay.close();
    overlay = await openMerged(db);
    await check(overlay);
  });

  describe('writes', () => {
    const text = async (o: OverlayFS) => (await o.diff()).map((c) => `${c.change} ${c.type} ${c.path}`).join('; ');

    it('copies up on the first write through a handle and keeps identity, mode and base', async () => {
      chmodSync(join(base, 'f'), 0o600);
      overlay = await openMerged(db, base);
      const ino = (await overlay.stat('/f')).ino;
      const h = await overlay.open('/f');
      await h.pwrite(0, Buffer.from('B'));
      expect((await h.pread(0, 10)).toString()).toBe('Base');
      expect(await overlay.readFile('/f', 'utf8')).toBe('Base');
      const after = await overlay.stat('/f');
      expect([after.ino, after.mode & 0o7777]).toEqual([ino, 0o600]);
      expect(readFileSync(join(base, 'f'), 'utf8')).toBe('base');
      await h.truncate(0);
      expect(await overlay.readFile('/f', 'utf8')).toBe('');
      expect(await text(overlay)).toBe('M f /f');
      await overlay.close();
      const origins = new Database(db);
      await origins.connect();
      expect(await origins.prepare('SELECT lower_path FROM fs_origin').all()).toEqual([{ lower_path: '/f' }]);
      await origins.close();
      overlay = undefined;
    });

    it('fails a handle write after its entry was removed or replaced', async () => {
      overlay = await openMerged(db, base);
      const [gone, replaced] = [await overlay.open('/f'), await overlay.open('/d/x')];
      await overlay.unlink('/f');
      expect(await code(gone.pwrite(0, Buffer.from('z')))).toBe('ENOENT');
      await overlay.unlink('/d/x');
      await overlay.writeFile('/d/x', 'new');
      expect(await code(replaced.truncate(0))).toBe('ENOENT');
      expect(await overlay.readFile('/d/x', 'utf8')).toBe('new');
      expect(readFileSync(join(base, 'f'), 'utf8')).toBe('base');
    });

    it('diffs a write into a large base without reading it', async () => {
      mkdirSync(join(base, 'big'));
      for (let i = 0; i < 10000; i++) writeFileSync(join(base, 'big', `f${i}`), '');
      const host = await hostLower(base);
      let names = 0;
      overlay = await openMerged(db, { ...host, readdir: async (p) => (names += (await host.readdir(p)).length, host.readdir(p)) });
      await overlay.writeFile('/big/f1', 'x');
      expect(await text(overlay)).toBe('M d /big; M f /big/f1');
      expect(names).toBeLessThan(100);
    });

    it('creates missing parents, removes trees, and copies files with their mode', async () => {
      chmodSync(join(base, 'f'), 0o600);
      overlay = await openMerged(db, base);
      await overlay.writeFile('/n/a/b', 'x');
      expect(await overlay.readdir('/n/a')).toEqual(['b']);
      await overlay.copyFile('/f', '/n/copy');
      expect([await overlay.readFile('/n/copy', 'utf8'), (await overlay.stat('/n/copy')).mode & 0o7777]).toEqual(['base', 0o600]);
      expect(await code(overlay.rm('/d'))).toBe('EISDIR');
      await overlay.rm('/d', { recursive: true });
      await overlay.rm('/nope', { force: true });
      expect(await code(overlay.rm('/nope'))).toBe('ENOENT');
      expect(await overlay.readdir('/')).toEqual(['e', 'f', 'l', 'n', 'p']);
      expect(await text(overlay)).toBe(
        'D d /d; D d /d/sub; D f /d/sub/y; D f /d/x; A d /n; A d /n/a; A f /n/a/b; A f /n/copy'
      );
    });

    it('links delta files, refuses lower sources, and keeps the data until the last link goes', async () => {
      overlay = await openMerged(db, base);
      expect(await code(overlay.link('/f', '/f2'))).toBe('EXDEV');
      await overlay.writeFile('/n', '1');
      await overlay.link('/n', '/n2');
      expect([(await overlay.stat('/n')).ino, (await overlay.stat('/n')).nlink]).toEqual([(await overlay.stat('/n2')).ino, 2]);
      await overlay.unlink('/n');
      expect(await overlay.readFile('/n2', 'utf8')).toBe('1');
    });

    it('moves a base directory and keeps the result across a reopen', async () => {
      overlay = await openMerged(db, base);
      await overlay.rename('/d', '/m');
      await overlay.symlink('m/x', '/s');
      await overlay.chmod('/e', 0o700);
      const before = await text(overlay);
      await overlay.close();
      overlay = await openMerged(db);
      expect(await text(overlay)).toBe(before);
      expect(await overlay.readFile('/m/sub/y', 'utf8')).toBe('y');
      expect(await overlay.readFile('/s', 'utf8')).toBe('x');
      expect(await code(overlay.stat('/d'))).toBe('ENOENT');
      expect((await overlay.stat('/e')).mode & 0o7777).toBe(0o700);
    });

    it('resolves dentries by name when a path ends in a slash, and keeps them valid after a reopen', async () => {
      overlay = await openMerged(db, base);
      await overlay.rename('/d', '/e/');
      await overlay.mkdir('/n/');
      await overlay.rmdir('/n/');
      await overlay.mkdir('/k');
      await overlay.rename('/k/', '/k2');
      expect(await code(overlay.rmdir('/e/.'))).toBe('EINVAL');
      expect(await code(overlay.rename('/f', '/zz/'))).toBe('ENOTDIR');
      expect(await code(overlay.symlink('t', '/s/'))).toBe('ENOENT');
      expect(await code(overlay.writeFile('/w/', 'x'))).toBe('EISDIR');
      await overlay.close();
      overlay = await openMerged(db);
      expect(await overlay.readdir('/')).toEqual(['e', 'f', 'k2', 'l', 'p']);
      expect(await overlay.readFile('/e/sub/y', 'utf8')).toBe('y');
      await overlay.close();
      overlay = undefined;
      const raw = new Database(db);
      await raw.connect();
      const dangling = `SELECT count(*) AS n FROM fs_dentry d LEFT JOIN fs_inode i ON i.ino = d.ino
        LEFT JOIN fs_inode p ON p.ino = d.parent_ino WHERE i.ino IS NULL OR p.ino IS NULL`;
      expect(await raw.prepare(dangling).get()).toEqual({ n: 0 });
      await raw.close();
    });

    it('serializes concurrent first writes and creates under one lower entry', async () => {
      overlay = await openMerged(db, base);
      const [a, b] = [await overlay.open('/f'), await overlay.open('/f')];
      await Promise.all([a.pwrite(0, Buffer.from('A')), b.pwrite(1, Buffer.from('B'))]);
      expect(await overlay.readFile('/f', 'utf8')).toBe('ABse');
      await Promise.all([overlay.writeFile('/d/a', 'A'), overlay.writeFile('/d/b', 'B')]);
      expect([await overlay.readFile('/d/a', 'utf8'), await overlay.readFile('/d/b', 'utf8')]).toEqual(['A', 'B']);
    });

    it('stores the byte length of a non-ASCII symlink target', async () => {
      symlinkSync('é', join(base, 'u'));
      overlay = await openMerged(db, base);
      await overlay.rename('/u', '/moved');
      await overlay.symlink('é', '/made');
      const sizes = async (o: OverlayFS) => [(await o.lstat('/moved')).size, (await o.lstat('/made')).size];
      expect(await sizes(overlay)).toEqual([2, 2]);
      await overlay.close();
      overlay = await openMerged(db);
      expect(await sizes(overlay)).toEqual([2, 2]);
    });

    it('rejects negative sizes and offsets before copying up', async () => {
      overlay = await openMerged(db, base);
      const h = await overlay.open('/f');
      expect(await code(h.truncate(-1))).toBe('EINVAL');
      expect(await code(h.pwrite(-1, Buffer.from('x')))).toBe('EINVAL');
      expect(await text(overlay)).toBe('');
      await h.pwrite(0, Buffer.from('B'));
      expect(await code(h.truncate(-1))).toBe('EINVAL');
      expect([await overlay.readFile('/f', 'utf8'), (await overlay.stat('/f')).size]).toEqual(['Base', 4]);
    });

    it('does not follow a final directory symlink for a mutation that ends in a slash', async () => {
      symlinkSync('d', join(base, 'aliasd'));
      symlinkSync('e', join(base, 'aliase'));
      overlay = await openMerged(db, base);
      await overlay.symlink('e', '/delta-link');
      const attempts = [
        overlay.rename('/aliasd/', '/moved'), overlay.rename('/d', '/aliase/'), overlay.rmdir('/aliase/'),
        overlay.rmdir('/delta-link/'), overlay.rm('/aliasd/', { recursive: true }), overlay.rename('/delta-link/', '/moved'),
      ];
      expect(await Promise.all(attempts.map(code))).toEqual(Array(6).fill('ENOTDIR'));
      expect(await code(overlay.mkdir('/aliasd/'))).toBe('EEXIST');
      expect(await code(overlay.rm('/d/.', { recursive: true }))).toBe('EINVAL');
      expect((await overlay.lstat('/aliasd/')).isDirectory()).toBe(true);
      expect(await overlay.readdir('/aliasd/')).toEqual(['sub', 'x']);
      await overlay.close();
      overlay = await openMerged(db);
      expect(await overlay.readdir('/')).toEqual(['aliasd', 'aliase', 'd', 'delta-link', 'e', 'f', 'l', 'p']);
      expect(await overlay.readFile('/d/x', 'utf8')).toBe('x');
    });

    it('rejects non-finite sizes, offsets and modes without changing the file', async () => {
      overlay = await openMerged(db, base);
      const h = await overlay.open('/f');
      const rejected = [h.truncate(Infinity), h.truncate(NaN), h.truncate(1.5), h.pwrite(NaN, Buffer.from('x')), h.pread(NaN, 1), h.pread(0, -1), overlay.chmod('/f', NaN)];
      expect(await Promise.all(rejected.map(code))).toEqual(Array(7).fill('EINVAL'));
      expect(await text(overlay)).toBe('');
      await h.pwrite(0, Buffer.from('B'));
      expect(await code(h.truncate(Infinity))).toBe('EINVAL');
      await overlay.close();
      overlay = await openMerged(db);
      expect([await overlay.readFile('/f', 'utf8'), (await overlay.stat('/f')).size]).toEqual(['Base', 4]);
    });

    it('finishes accepted mutations before close releases the lock', async () => {
      let entered!: () => void;
      let release!: () => void;
      const reading = new Promise<void>((r) => (entered = r));
      const blocked = new Promise<void>((r) => (release = r));
      const host = await hostLower(base);
      overlay = await openMerged(db, { ...host, read: async (...a) => (entered(), await blocked, host.read(...a)) });
      const write = (await overlay.open('/f')).pwrite(0, Buffer.from('A'));
      const queued = overlay.writeFile('/after', 'ok');
      await reading;
      const closing = overlay.close();
      expect(await code(overlay.writeFile('/late', 'x'))).toBe('EBADF');
      expect(await code(openMerged(db))).toBe('EBUSY');
      release();
      await Promise.all([write, queued, closing]);
      overlay = await openMerged(db);
      expect([await overlay.readFile('/f', 'utf8'), await overlay.readFile('/after', 'utf8')]).toEqual(['Aase', 'ok']);
      await overlay.close();
      overlay = undefined;
      const raw = new Database(db);
      await raw.connect();
      const orphans = 'SELECT count(*) AS n FROM fs_inode i LEFT JOIN fs_dentry d ON d.ino = i.ino WHERE i.ino != 1 AND d.ino IS NULL';
      expect(await raw.prepare(orphans).get()).toEqual({ n: 0 });
      await raw.close();
    });

    it('keeps one merged inode for observers while a copy-up is in flight', async () => {
      let entered!: () => void;
      let release!: () => void;
      const published = new Promise<void>((r) => (entered = r));
      const blocked = new Promise<void>((r) => (release = r));
      const real = new Database(db);
      await real.connect();
      const slow = new Proxy(real, {
        get(target, key) {
          const value = Reflect.get(target, key);
          if (key !== 'prepare') return typeof value === 'function' ? value.bind(target) : value;
          return (sql: string) => {
            const stmt = target.prepare(sql);
            if (!sql.includes('INSERT INTO fs_origin')) return stmt;
            return new Proxy(stmt, {
              get: (t, k) => (k === 'run' ? async (...a: unknown[]) => ((await t.run(...a)), entered(), blocked) : Reflect.get(t, k).bind(t)),
            });
          };
        },
      });
      overlay = await openOnHandle(slow, await hostLower(base));
      const first = await overlay.open('/f');
      const before = (await first.fstat()).ino;
      const write = first.pwrite(0, Buffer.from('A'));
      await published;
      const [during, second] = [await overlay.stat('/f'), await overlay.open('/f')];
      release();
      await write;
      expect([during.ino, (await overlay.stat('/f')).ino, (await second.fstat()).ino]).toEqual([before, before, before]);
    });

    it('refuses to copy up a lower FIFO and changes nothing', async () => {
      execFileSync('mkfifo', [join(base, 'd', 'fifo')]);
      overlay = await openMerged(db, base);
      expect(await code(overlay.rename('/d', '/m'))).toBe('ENOTSUP');
      expect(await code(overlay.chmod('/p', 0o600))).toBe('ENOTSUP');
      expect(await overlay.readdir('/')).toEqual(['d', 'e', 'f', 'l', 'p']);
      expect(await text(overlay)).toBe('');
    });
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

    it('keeps the reservation while the connection is still closing', async () => {
      const lower = await hostLower(base);
      const real = new Database(db);
      await real.connect();
      const slow = new Proxy(real, {
        get(target, key) {
          if (key === 'close') return async () => (await new Promise((r) => setTimeout(r, 100)), target.close());
          const value = Reflect.get(target, key);
          return typeof value === 'function' ? value.bind(target) : value;
        },
      });
      overlay = await openOnHandle(slow, lower);
      const closing = overlay.close();
      const again = new Database(db);
      await again.connect();
      expect(await code(openOnHandle(again, lower))).toBe('EBUSY');
      await closing;
      const next = await openOnHandle(again, lower);
      expect(await next.readFile('/f', 'utf8')).toBe('base');
      await next.close();
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
