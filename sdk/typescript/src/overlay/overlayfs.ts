import type { DatabasePromise } from '@tursodatabase/database-common';
import { createFsError, type FsErrorCode, type FsSyscall } from '../errors.js';
import { AgentFS, AgentFSFile } from '../filesystem/agentfs.js';
import {
  S_IFMT,
  S_IFREG,
  S_IFDIR,
  S_IFLNK,
  createStats,
  type DirEntry,
  type FileHandle,
  type FileSystem,
  type FilesystemStats,
  type Stats,
  type StatsData,
} from '../filesystem/interface.js';

/** Base entry stats. `mode` carries the file type bits; times are in seconds. */
export interface LowerStats {
  mode: number;
  uid: number;
  gid: number;
  size: number;
  nlink: number;
  atime: number;
  mtime: number;
  ctime: number;
}

/**
 * Read-only view of the base layer. Every `path` is a spec Path (`/`, `/d/x`).
 * OverlayFS calls a method on P only after `lstat` returned a directory for each
 * proper ancestor of P, so an adapter resolves P with one lookup and never meets a symlink.
 * An error other than "not found" propagates unchanged.
 */
export interface LowerAdapter {
  /** Written to `base_path`. */
  readonly locator: string;
  /** Written to `base_identity`; non-empty. */
  identity(): Promise<string>;
  /** `null` when P does not exist. */
  lstat(path: string): Promise<LowerStats | null>;
  readdir(path: string): Promise<string[]>;
  readlink(path: string): Promise<string>;
  /** Shorter than `length` at end of file. */
  read(path: string, offset: number, length: number): Promise<Uint8Array>;
}

export interface OverlayInfo {
  path: string;
  version: number;
  basePath: string;
  baseIdentity: string;
}

export interface Change {
  change: 'A' | 'M' | 'D';
  type: 'f' | 'd' | 'l' | '?';
  path: string;
}

/** A merged entry. `ino` is the instance-scoped inode number. */
interface Entry {
  ino: number;
  deltaIno?: number;
  lowerPath?: string;
}

/** `lower` is the base entry at `path` when the base resolves it and no whiteout covers it. */
interface Node {
  entry: Entry;
  path: string;
  stats: Stats;
  lower?: LowerStats;
}

const MAX_LINKS = 40;
const COLUMNS = 'i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime';
const TYPES: Record<number, Change['type']> = { [S_IFREG]: 'f', [S_IFDIR]: 'd', [S_IFLNK]: 'l' };

const fail = (code: FsErrorCode, syscall: FsSyscall, path?: string): never => {
  throw createFsError({ code, syscall, path });
};
const unsupported = (syscall: FsSyscall) => async (..._args: any[]): Promise<never> => fail('ENOSYS', syscall);
const join = (dir: string, name: string) => `${dir === '/' ? '' : dir}/${name}`;
const split = (path: string) => path.split('/').filter((c) => c && c !== '.').reverse();
const isDir = (s?: { mode: number } | null) => !!s && (s.mode & S_IFMT) === S_IFDIR;
const typeOf = (mode: number) => TYPES[mode & S_IFMT] ?? '?';

/**
 * An unclosed statement keeps the database file locked after `db.close()`. This wrapper closes
 * each statement after every `get`, `all` or `run`, so closing the connection releases the lock.
 */
const closing = (db: DatabasePromise): DatabasePromise =>
  new Proxy(db, {
    get(target, key) {
      const value = Reflect.get(target, key);
      if (key !== 'prepare') return typeof value === 'function' ? value.bind(target) : value;
      return (sql: string) =>
        new Proxy({}, {
          get: (_, method: string) => async (...args: unknown[]) => {
            const stmt = target.prepare(sql) as any;
            try {
              return await stmt[method](...args);
            } finally {
              stmt.close();
            }
          },
        });
    },
  });

/** Storage key to its open overlay, or `null` while a management call holds the reservation. */
const registry = new Map<unknown, OverlayFS | null>();

/** @internal */
export function claim(key: unknown): void {
  if (registry.has(key)) fail('EBUSY', 'open');
  registry.set(key, null);
}

/** @internal */
export function release(key: unknown): void {
  registry.delete(key);
}

/** A writable AgentFS delta over a read-only base. */
export class OverlayFS implements FileSystem {
  private whiteouts = new Set<string>();
  private byDelta = new Map<number, Entry>([[1, { ino: 1, deltaIno: 1 }]]);
  private byLower = new Map<string, Entry>();
  private nextIno = 2;
  private rootLower?: LowerStats;
  private closed = false;

  /** @internal Use `openMerged`. */
  constructor(
    private db: DatabasePromise,
    private delta: AgentFS,
    private lower: LowerAdapter,
    private B: BufferConstructor,
    private key: unknown
  ) {}

  /** @internal */
  async load(): Promise<void> {
    const rows = (await this.db.prepare('SELECT path FROM fs_whiteout').all()) as { path: string }[];
    this.whiteouts = new Set(rows.map((r) => r.path));
    this.rootLower = (await this.lower.lstat('/')) ?? undefined;
  }

  /** Releases the writer lock. A second call does nothing. */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    release(this.key);
    await this.db.close();
  }

  private cmp(a: string, b: string): number {
    return this.B.compare(this.B.from(a), this.B.from(b));
  }

  private async stats(e: Entry): Promise<Stats> {
    if (e.deltaIno === undefined) {
      const l = await this.lower.lstat(e.lowerPath!);
      return l ? createStats({ ino: e.ino, ...l }) : fail('ENOENT', 'stat', e.lowerPath);
    }
    const row = (await this.db.prepare(`SELECT ${COLUMNS} FROM fs_inode i WHERE i.ino = ?`).get(e.deltaIno)) as
      | StatsData
      | undefined;
    return row ? createStats({ ...row, ino: e.ino }) : fail('ENOENT', 'stat');
  }

  private async pread(e: Entry, offset: number, size: number): Promise<Buffer> {
    if (e.deltaIno !== undefined) {
      return new AgentFSFile(this.db, this.B, e.deltaIno, this.delta.getChunkSize()).pread(offset, size);
    }
    return this.B.from(await this.lower.read(e.lowerPath!, offset, size));
  }

  private deltaNode(path: string, row: StatsData, lower?: LowerStats): Node {
    const deltaIno = Number(row.ino);
    let entry = this.byDelta.get(deltaIno);
    if (!entry) this.byDelta.set(deltaIno, (entry = { ino: this.nextIno++, deltaIno }));
    return { entry, path, lower, stats: createStats({ ...row, ino: entry.ino }) };
  }

  private lowerNode(path: string, lower: LowerStats): Node {
    let entry = this.byLower.get(path);
    if (!entry) this.byLower.set(path, (entry = { ino: this.nextIno++, lowerPath: path }));
    return { entry, path, lower, stats: createStats({ ...lower, ino: entry.ino }) };
  }

  /** The merged child of `dir`, by the spec's lookup precedence. */
  private async child(dir: Node, name: string): Promise<Node | null> {
    const path = join(dir.path, name);
    // `dir.lower` is set only when no whiteout covers `dir`, so a whiteout at `path` is the only one to check.
    const lower =
      isDir(dir.lower) && !this.whiteouts.has(path) ? ((await this.lower.lstat(path)) ?? undefined) : undefined;
    if (dir.entry.deltaIno !== undefined) {
      const row = (await this.db
        .prepare(
          `SELECT ${COLUMNS} FROM fs_dentry d JOIN fs_inode i ON i.ino = d.ino WHERE d.parent_ino = ? AND d.name = ?`
        )
        .get(dir.entry.deltaIno, name)) as StatsData | undefined;
      if (row) return this.deltaNode(path, row, lower);
    }
    return lower ? this.lowerNode(path, lower) : null;
  }

  private async target(node: Node): Promise<string> {
    if (node.entry.deltaIno === undefined) return this.lower.readlink(node.path);
    const row = (await this.db.prepare('SELECT target FROM fs_symlink WHERE ino = ?').get(node.entry.deltaIno)) as
      | { target: string }
      | undefined;
    return row?.target ?? fail('ENOENT', 'open', node.path);
  }

  /**
   * Resolves `path` one component at a time. Symlinks at intermediate components are always
   * followed, the last one only if `followLast`. `node` is null when the last component is missing.
   */
  private async walk(path: string, followLast: boolean, syscall: FsSyscall) {
    const stack = [await this.root()];
    const parts = split(path);
    let node: Node | null = stack[0];
    let name = '';
    let links = 0;
    while (parts.length) {
      name = parts.pop()!;
      if (name === '..') {
        if (stack.length > 1) stack.pop();
        node = stack[stack.length - 1];
        continue;
      }
      node = await this.child(stack[stack.length - 1], name);
      if (!node && parts.length) fail('ENOENT', syscall, path);
      if (!node) break;
      if (node.stats.isSymbolicLink() && (parts.length || followLast)) {
        if (++links > MAX_LINKS) fail('ELOOP', syscall, path);
        const target = await this.target(node);
        if (target.startsWith('/')) stack.length = 1;
        parts.push(...split(target));
        node = stack[stack.length - 1];
      } else if (parts.length) {
        if (!isDir(node.stats)) fail('ENOTDIR', syscall, path);
        stack.push(node);
      }
    }
    return { parent: stack[stack.length - 1], name, node };
  }

  private async root(): Promise<Node> {
    const entry = this.byDelta.get(1)!;
    return { entry, path: '/', lower: this.rootLower, stats: await this.stats(entry) };
  }

  private async find(path: string, followLast: boolean, syscall: FsSyscall): Promise<Node> {
    return (await this.walk(path, followLast, syscall)).node ?? fail('ENOENT', syscall, path);
  }

  private async file(path: string): Promise<Node> {
    const node = await this.find(path, true, 'open');
    if (isDir(node.stats)) fail('EISDIR', 'open', path);
    if (!node.stats.isFile()) fail('ENOTSUP', 'open', path);
    return node;
  }

  private async dir(path: string): Promise<Node> {
    const node = await this.find(path, true, 'scandir');
    return isDir(node.stats) ? node : fail('ENOTDIR', 'scandir', path);
  }

  private async names(dir: Node): Promise<string[]> {
    const names = new Set<string>();
    if (dir.entry.deltaIno !== undefined) {
      const rows = await this.db.prepare('SELECT name FROM fs_dentry WHERE parent_ino = ?').all(dir.entry.deltaIno);
      for (const r of rows as { name: string }[]) names.add(r.name);
    }
    if (isDir(dir.lower)) {
      for (const n of await this.lower.readdir(dir.path)) {
        if (!this.whiteouts.has(join(dir.path, n))) names.add(n);
      }
    }
    return [...names].sort((a, b) => this.cmp(a, b));
  }

  async stat(path: string): Promise<Stats> {
    return (await this.find(path, true, 'stat')).stats;
  }

  async lstat(path: string): Promise<Stats> {
    return (await this.find(path, false, 'stat')).stats;
  }

  async readFile(path: string): Promise<Buffer>;
  async readFile(path: string, encoding: BufferEncoding): Promise<string>;
  async readFile(path: string, options: { encoding: BufferEncoding }): Promise<string>;
  async readFile(
    path: string,
    options?: BufferEncoding | { encoding?: BufferEncoding }
  ): Promise<Buffer | string> {
    const encoding = typeof options === 'string' ? options : options?.encoding;
    const node = await this.file(path);
    const data = await this.pread(node.entry, 0, node.stats.size);
    return encoding ? data.toString(encoding) : data;
  }

  async readdir(path: string): Promise<string[]> {
    return this.names(await this.dir(path));
  }

  async readdirPlus(path: string): Promise<DirEntry[]> {
    const dir = await this.dir(path);
    const entries: DirEntry[] = [];
    for (const name of await this.names(dir)) {
      const node = await this.child(dir, name);
      if (node) entries.push({ name, stats: node.stats });
    }
    return entries;
  }

  async readlink(path: string): Promise<string> {
    const node = await this.find(path, false, 'open');
    return node.stats.isSymbolicLink() ? this.target(node) : fail('EINVAL', 'open', path);
  }

  async access(path: string): Promise<void> {
    await this.find(path, true, 'access');
  }

  statfs(): Promise<FilesystemStats> {
    return this.delta.statfs();
  }

  async open(path: string): Promise<FileHandle> {
    const { entry } = await this.file(path);
    return {
      pread: (offset, size) => this.pread(entry, offset, size),
      fstat: () => this.stats(entry),
      fsync: async () => {},
      pwrite: unsupported('open'),
      truncate: unsupported('open'),
    };
  }

  writeFile: FileSystem['writeFile'] = unsupported('open');
  mkdir: FileSystem['mkdir'] = unsupported('mkdir');
  rmdir: FileSystem['rmdir'] = unsupported('rmdir');
  unlink: FileSystem['unlink'] = unsupported('unlink');
  rm: FileSystem['rm'] = unsupported('rm');
  rename: FileSystem['rename'] = unsupported('rename');
  copyFile: FileSystem['copyFile'] = unsupported('copyfile');
  symlink: FileSystem['symlink'] = unsupported('open');
  chmod: (path: string, mode: number) => Promise<void> = unsupported('chmod');
  link: (existingPath: string, newPath: string) => Promise<void> = unsupported('link');

  /**
   * Compares the merged tree with the base tree without reading the whole base: it walks the
   * delta, then the base subtrees that a whiteout or a non-directory delta entry hides.
   */
  async diff(): Promise<Change[]> {
    const changes = new Map<string, Change>();
    const inDelta = new Set<string>();
    const hidden = new Set(this.whiteouts);
    const memo = new Map<string, Promise<LowerStats | null>>();
    const base = (path: string): Promise<LowerStats | null> => {
      if (!memo.has(path)) {
        const parent = path === '/' ? Promise.resolve({ mode: S_IFDIR }) : base(path.slice(0, path.lastIndexOf('/')) || '/');
        memo.set(path, parent.then((p) => (isDir(p) ? this.lower.lstat(path) : null)));
      }
      return memo.get(path)!;
    };
    const rows = this.db.prepare(
      'SELECT d.name, i.ino, i.mode FROM fs_dentry d JOIN fs_inode i ON i.ino = d.ino WHERE d.parent_ino = ?'
    );
    const queue: [number, string][] = [[1, '/']];
    for (let item = queue.pop(); item; item = queue.pop()) {
      for (const r of (await rows.all(item[0])) as { name: string; ino: number; mode: number }[]) {
        const path = join(item[1], r.name);
        const inBase = await base(path);
        inDelta.add(path);
        changes.set(path, { change: inBase ? 'M' : 'A', type: typeOf(r.mode), path });
        if (isDir(r)) queue.push([Number(r.ino), path]);
        else if (isDir(inBase)) hidden.add(path);
      }
    }
    const removed = async (path: string): Promise<void> => {
      const b = await base(path);
      if (!b) return;
      if (!inDelta.has(path)) changes.set(path, { change: 'D', type: typeOf(b.mode), path });
      if (isDir(b)) for (const n of await this.lower.readdir(path)) await removed(join(path, n));
    };
    for (const path of hidden) await removed(path);
    return [...changes.values()].sort((a, b) => this.cmp(a.path, b.path));
  }
}

/** @internal Writes the AgentFS schema and a v1 overlay over `base` in one transaction. */
export async function initOverlay(raw: DatabasePromise, base: LowerAdapter, B: BufferConstructor) {
  const db = closing(raw);
  const config = { base_path: base.locator, base_identity: await base.identity(), version: '1' };
  const delta = await AgentFS.fromDatabase(db, B);
  await db.exec('BEGIN');
  try {
    await db.exec(`
      CREATE TABLE IF NOT EXISTS fs_overlay_config (key TEXT PRIMARY KEY, value TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS fs_whiteout (path TEXT PRIMARY KEY, created_at INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS fs_origin (delta_ino INTEGER PRIMARY KEY, lower_path TEXT)
    `);
    const put = db.prepare('INSERT INTO fs_overlay_config (key, value) VALUES (?, ?)');
    for (const [key, value] of Object.entries(config)) await put.run(key, value);
    await db.exec('COMMIT');
  } catch (e) {
    await db.exec('ROLLBACK');
    throw e;
  }
  const info = { version: 1, basePath: config.base_path, baseIdentity: config.base_identity };
  return { delta, info };
}

/**
 * @internal Classifies the database before any write, initializes it if needed, and opens it.
 * The caller has claimed `key` and owns `db`. Without `base`, `host` opens the recorded `base_path`.
 * Migration is not implemented: it fails with `ENOSYS`.
 */
export async function mount(
  raw: DatabasePromise,
  key: unknown,
  B: BufferConstructor,
  base?: LowerAdapter,
  host?: (dir: string) => Promise<LowerAdapter>
): Promise<OverlayFS> {
  const db = closing(raw);
  const has = (await db.prepare("SELECT 1 FROM sqlite_master WHERE name = 'fs_overlay_config'").all()).length > 0;
  const config = new Map<string, string>();
  if (has) {
    const rows = (await db.prepare('SELECT key, value FROM fs_overlay_config').all()) as { key: string; value: string }[];
    for (const r of rows) config.set(r.key, r.value);
  }
  const version = config.get('version');
  if (version !== undefined && version !== '1') fail('ENOTSUP', 'open');
  if (version === undefined) {
    if (has) fail('ENOSYS', 'open');
    if (!base) fail('EINVAL', 'open');
    const legacy = "SELECT 1 FROM pragma_table_info('fs_whiteout') WHERE name = 'parent_path'";
    if ((await db.prepare(legacy).all()).length > 0) fail('ENOSYS', 'open');
  }
  const basePath = config.get('base_path');
  const lower = base ?? (host && basePath ? await host(basePath) : fail('EINVAL', 'open'));
  if (version === '1' && (await lower.identity()) !== config.get('base_identity')) fail('ESTALE', 'open');
  const delta = version === '1' ? await AgentFS.fromDatabase(db, B) : (await initOverlay(raw, lower, B)).delta;
  const overlay = new OverlayFS(db, delta, lower, B, key);
  await overlay.load();
  registry.set(key, overlay);
  return overlay;
}

const handleKey = (db: DatabasePromise) => (db.memory ? db : `name:${db.name}`);

/**
 * Initializes a v1 overlay in a database that the caller opened. Fails with `EEXIST` if the
 * database has a table. The call never closes `db`.
 */
export async function createOverlay(db: DatabasePromise, base: LowerAdapter, B = Buffer): Promise<OverlayInfo> {
  const key = handleKey(db);
  claim(key);
  try {
    if ((await closing(db).prepare("SELECT 1 FROM sqlite_master WHERE type = 'table'").all()).length > 0) {
      fail('EEXIST', 'open', db.name);
    }
    return { path: db.name, ...(await initOverlay(db, base, B)).info };
  } finally {
    release(key);
  }
}

/**
 * Opens the overlay in a database that the caller opened. The returned overlay owns `db`:
 * `close()` closes it, and so does a failed open that got past the writer-lock check.
 */
export async function openMerged(db: DatabasePromise, base: LowerAdapter, B = Buffer): Promise<OverlayFS> {
  const key = handleKey(db);
  claim(key);
  try {
    return await mount(db, key, B, base);
  } catch (e) {
    release(key);
    await db.close();
    throw e;
  }
}
