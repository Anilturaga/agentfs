import type { DatabasePromise } from '@tursodatabase/database-common';
import { createFsError, type FsErrorCode, type FsSyscall } from '../errors.js';
import { AgentFS, AgentFSFile } from '../filesystem/agentfs.js';
import {
  S_IFMT, S_IFREG, S_IFDIR, S_IFLNK, DEFAULT_FILE_MODE, DEFAULT_DIR_MODE, createStats,
  type DirEntry, type FileHandle, type FileSystem, type FilesystemStats, type Stats, type StatsData,
} from '../filesystem/interface.js';

/** Base entry stats. `mode` carries the file type bits; times are in seconds. */
export interface LowerStats {
  mode: number; uid: number; gid: number; size: number; nlink: number;
  atime: number; mtime: number; ctime: number;
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

export interface OverlayInfo { path: string; version: number; basePath: string; baseIdentity: string }

export interface Change { change: 'A' | 'M' | 'D'; type: 'f' | 'd' | 'l' | '?'; path: string }

/** A merged entry. `ino` is the instance-scoped inode number. */
interface Entry { ino: number; deltaIno?: number; lowerPath?: string }

/** `lower` is the base entry at `path` when the base resolves it and no whiteout covers it. */
interface Node { entry: Entry; path: string; stats: Stats; lower?: LowerStats }

const MAX_LINKS = 40;
const COLUMNS = 'i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime';
const TYPES: Record<number, Change['type']> = { [S_IFREG]: 'f', [S_IFDIR]: 'd', [S_IFLNK]: 'l' };

const fail = (code: FsErrorCode, syscall: FsSyscall, path?: string): never => {
  throw createFsError({ code, syscall, path });
};
const join = (dir: string, name: string) => `${dir === '/' ? '' : dir}/${name}`;
const split = (path: string) => path.split('/').filter((c, i, all) => c || (i > 0 && i === all.length - 1)).reverse();
const isDir = (s?: { mode: number } | null) => !!s && (s.mode & S_IFMT) === S_IFDIR;
const typeOf = (mode: number) => TYPES[mode & S_IFMT] ?? '?';
const copyable = (mode: number) => (mode & S_IFMT) in TYPES;
const parentOf = (path: string) => path.slice(0, path.lastIndexOf('/')) || '/';
const now = () => Math.floor(Date.now() / 1000);
const natural = (n: number) => Number.isSafeInteger(n) && n >= 0;

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
            try { return await stmt[method](...args); } finally { stmt.close(); }
          },
        });
    },
  });

/** Storage key to its open overlay, or `null` while a management call holds the reservation. */
const registry = new Map<unknown, OverlayFS | null>();

export function claim(key: unknown): void {
  if (registry.has(key)) fail('EBUSY', 'open');
  registry.set(key, null);
}
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
  private tail: Promise<unknown> = Promise.resolve();

  /** @internal Use `openMerged`. */
  constructor(
    private db: DatabasePromise,
    private delta: AgentFS,
    private lower: LowerAdapter,
    private B: BufferConstructor,
    private key: unknown
  ) {}

  async load(): Promise<void> {
    const rows: { path: string }[] = await this.db.prepare('SELECT path FROM fs_whiteout').all();
    this.whiteouts = new Set(rows.map((r) => r.path));
    this.rootLower = (await this.lower.lstat('/')) ?? undefined;
  }

  /** Finishes the accepted mutations, then releases the writer lock. A second call does nothing. */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    await this.tail;
    await this.db.close().finally(() => release(this.key));
  }

  private cmp(a: string, b: string): number {
    return this.B.compare(this.B.from(a), this.B.from(b));
  }

  private async stats(e: Entry): Promise<Stats> {
    if (e.deltaIno === undefined) {
      const l = await this.lower.lstat(e.lowerPath!);
      return l ? createStats({ ...l, ino: e.ino }) : fail('ENOENT', 'stat', e.lowerPath);
    }
    const row: StatsData | undefined = await this.db
      .prepare(`SELECT ${COLUMNS} FROM fs_inode i WHERE i.ino = ?`)
      .get(e.deltaIno);
    return row ? createStats({ ...row, ino: e.ino }) : fail('ENOENT', 'stat');
  }

  private async pread(e: Entry, offset: number, size: number): Promise<Buffer> {
    if (e.deltaIno !== undefined) {
      return new AgentFSFile(this.db, this.B, e.deltaIno, this.delta.getChunkSize()).pread(offset, size);
    }
    return this.B.from(await this.lower.read(e.lowerPath!, offset, size));
  }

  private async child(dir: Node, name: string): Promise<Node | null> {
    const path = join(dir.path, name);
    // `dir.lower` is set only when no whiteout covers `dir`, so a whiteout at `path` is the only one to check.
    const lower =
      isDir(dir.lower) && !this.whiteouts.has(path) ? ((await this.lower.lstat(path)) ?? undefined) : undefined;
    if (dir.entry.deltaIno !== undefined) {
      const sql = `SELECT ${COLUMNS} FROM fs_dentry d JOIN fs_inode i ON i.ino = d.ino WHERE d.parent_ino = ? AND d.name = ?`;
      const row: StatsData | undefined = await this.db.prepare(sql).get(dir.entry.deltaIno, name);
      if (row) {
        const deltaIno = Number(row.ino);
        let entry = this.byDelta.get(deltaIno);
        if (!entry) this.byDelta.set(deltaIno, (entry = { ino: this.nextIno++, deltaIno }));
        return { entry, path, lower, stats: createStats({ ...row, ino: entry.ino }) };
      }
    }
    if (!lower) return null;
    let entry = this.byLower.get(path);
    if (!entry) this.byLower.set(path, (entry = { ino: this.nextIno++, lowerPath: path }));
    return { entry, path, lower, stats: createStats({ ...lower, ino: entry.ino }) };
  }

  private async target(node: Node): Promise<string> {
    if (node.entry.deltaIno === undefined) return this.lower.readlink(node.path);
    const row: { target: string } | undefined = await this.db
      .prepare('SELECT target FROM fs_symlink WHERE ino = ?')
      .get(node.entry.deltaIno);
    return row?.target ?? fail('ENOENT', 'open', node.path);
  }

  /**
   * Resolves `path` one component at a time. Symlinks at intermediate components are always
   * followed, the last one only if `followLast`. `node` is null when the last component is missing.
   * `parent` and `name` locate the dentry of `node`; `name` is empty after a final `.` or `..`,
   * `dirOnly` is set when the path ends in `/`, `.` or `..`, and `viaLink` when a no-follow walk crossed
   * a final symlink only because of that suffix (POSIX mutations then fail with `ENOTDIR` or `EEXIST`).
   */
  private async walk(path: string, followLast: boolean, syscall: FsSyscall) {
    const root = this.byDelta.get(1)!;
    const stack: Node[] = [{ entry: root, path: '/', lower: this.rootLower, stats: await this.stats(root) }];
    const parts = split(path);
    let node: Node | null = stack[0];
    let [parent, name, dirOnly, viaLink, links] = [stack[0], '', false, false, 0];
    while (parts.length) {
      const part = parts.pop()!;
      if (part === '' || part === '.' || part === '..') {
        if (part === '..' && stack.length > 1) stack.pop();
        node = stack[stack.length - 1];
        if (part) name = '';
        dirOnly = true;
        continue;
      }
      [parent, name, dirOnly] = [stack[stack.length - 1], part, false];
      node = await this.child(parent, name);
      if (!node && parts.some((p) => p !== '')) fail('ENOENT', syscall, path);
      if (!node) {
        dirOnly = parts.length > 0;
        break;
      }
      if (node.stats.isSymbolicLink() && (parts.length || followLast)) {
        if (++links > MAX_LINKS) fail('ELOOP', syscall, path);
        if (!followLast && parts.every((p) => p === '' || p === '.')) viaLink = true;
        const target = await this.target(node);
        if (target.startsWith('/')) stack.length = 1;
        parts.push(...split(target));
        node = stack[stack.length - 1];
      } else if (parts.length) {
        if (!isDir(node.stats)) fail('ENOTDIR', syscall, path);
        stack.push(node);
      }
    }
    return { parent, name, node, dirOnly, viaLink };
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
      for (const r of rows) names.add(r.name);
    }
    if (isDir(dir.lower)) {
      for (const n of await this.lower.readdir(dir.path)) if (!this.whiteouts.has(join(dir.path, n))) names.add(n);
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
  async readFile(path: string, options?: BufferEncoding | { encoding?: BufferEncoding }): Promise<Buffer | string> {
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
    const file = async (withContent = true) =>
      new AgentFSFile(this.db, this.B, await this.writable(entry, withContent), this.delta.getChunkSize());
    return {
      pread: async (offset, size) => (natural(offset) && natural(size) ? this.pread(entry, offset, size) : fail('EINVAL', 'open')),
      fstat: () => this.stats(entry),
      fsync: async () => {},
      pwrite: (offset, data) =>
        this.exclusive(async () => (!natural(offset) ? fail('EINVAL', 'open') : (await file()).pwrite(offset, data))),
      truncate: (size) =>
        this.exclusive(async () => (!natural(size) ? fail('EINVAL', 'open') : (await file(size > 0)).truncate(size))),
    };
  }

  /** Copies a lower-backed entry and its lower-backed ancestors, top-down, into the delta. */
  private async copyUp(node: Node, withContent = true): Promise<number> {
    if (node.entry.deltaIno !== undefined) return node.entry.deltaIno;
    const l = node.lower!;
    if (!copyable(l.mode)) fail('ENOTSUP', 'open', node.path);
    const parentIno = await this.copyUp(await this.find(parentOf(node.path), true, 'open'));
    const kind = l.mode & S_IFMT;
    const target = kind === S_IFLNK ? await this.lower.readlink(node.path) : '';
    const data = kind === S_IFREG && withContent && l.size > 0 ? this.B.from(await this.lower.read(node.path, 0, l.size)) : null;
    const ino = await this.delta.createInode(l.mode, l.uid, l.gid);
    if (kind === S_IFLNK) await this.writeLink(ino, target);
    if (data) await new AgentFSFile(this.db, this.B, ino, this.delta.getChunkSize()).pwrite(0, data);
    this.byDelta.set(ino, node.entry);
    await this.delta.createDentry(parentIno, node.path.slice(node.path.lastIndexOf('/') + 1), ino);
    await this.db.prepare('INSERT INTO fs_origin (delta_ino, lower_path) VALUES (?, ?)').run(ino, node.path);
    this.byLower.delete(node.path);
    node.entry.deltaIno = ino;
    return ino;
  }

  /** Inserts a whiteout if the base resolves `node` and no whiteout covers it. */
  private async hide(node: Node | null): Promise<void> {
    if (!node?.lower) return;
    await this.db.prepare('INSERT INTO fs_whiteout (path, created_at) VALUES (?, ?)').run(node.path, now());
    this.whiteouts.add(node.path);
  }

  private async writeLink(ino: number, target: string): Promise<void> {
    await this.db.prepare('INSERT INTO fs_symlink (ino, target) VALUES (?, ?)').run(ino, target);
    await this.db.prepare('UPDATE fs_inode SET size = ? WHERE ino = ?').run(this.B.byteLength(target), ino);
  }

  private async add(parent: Node, name: string, mode: number): Promise<number> {
    const parentIno = await this.copyUp(parent);
    const ino = await this.delta.createInode(mode);
    await this.delta.createDentry(parentIno, name, ino);
    return ino;
  }

  /** Resolves an existing entry that is not the root, without following a final symlink. */
  private async locate(path: string, syscall: FsSyscall) {
    const { parent, name, node, viaLink } = await this.walk(path, false, syscall);
    if (viaLink) fail('ENOTDIR', syscall, path);
    const found = node ?? fail('ENOENT', syscall, path);
    if (found.path === '/') fail('EPERM', syscall, path);
    if (!name) fail('EINVAL', syscall, path);
    return { parent, name, node: found };
  }

  private async remove({ parent, name, node }: { parent: Node; name: string; node: Node }): Promise<void> {
    if (node.entry.deltaIno !== undefined) {
      await this.delta.removeDentryAndMaybeInode(parent.entry.deltaIno!, name, node.entry.deltaIno);
    }
    await this.hide(node);
  }

  /** The delta inode of a handle's entry. A lower-backed entry is copied up unless it was removed or replaced. */
  private async writable(entry: Entry, withContent: boolean): Promise<number> {
    if (entry.deltaIno !== undefined) return entry.deltaIno;
    const { node } = await this.walk(entry.lowerPath!, false, 'open');
    return node?.entry === entry ? this.copyUp(node, withContent) : fail('ENOENT', 'open', entry.lowerPath);
  }

  /** Runs mutations one at a time, so concurrent first copy-ups and writes cannot interleave. */
  private async exclusive<T>(op: () => Promise<T>): Promise<T> {
    if (this.closed) fail('EBADF', 'open');
    const run = this.tail.then(op);
    this.tail = run.catch(() => {});
    return run;
  }

  private locked<A extends unknown[]>(op: (...args: A) => Promise<void>) {
    return (...args: A) => this.exclusive(() => op.apply(this, args));
  }

  private async subtree(node: Node): Promise<Node[]> {
    const nodes = [node];
    if (isDir(node.stats)) {
      for (const name of await this.names(node)) nodes.push(...(await this.subtree((await this.child(node, name))!)));
    }
    return nodes;
  }

  private async doWriteFile(path: string, data: string | Buffer, options?: BufferEncoding | { encoding?: BufferEncoding }) {
    const content = typeof data === 'string' ? this.B.from(data, typeof options === 'string' ? options : options?.encoding) : data;
    const dirs = path.split('/').filter(Boolean).slice(0, -1);
    for (let i = 1; i <= dirs.length; i++) {
      const dir = '/' + dirs.slice(0, i).join('/');
      if (!(await this.walk(dir, true, 'open')).node) await this.doMkdir(dir);
    }
    const { parent, name, node, dirOnly } = await this.walk(path, true, 'open');
    if (dirOnly || (node && isDir(node.stats))) fail('EISDIR', 'open', path);
    const ino = node ? await this.copyUp(node, false) : await this.add(parent, name, DEFAULT_FILE_MODE);
    const file = new AgentFSFile(this.db, this.B, ino, this.delta.getChunkSize());
    await file.truncate(0);
    await file.pwrite(0, content);
  }

  private async doMkdir(path: string): Promise<void> {
    const { parent, name, node, viaLink } = await this.walk(path, false, 'mkdir');
    if (node || viaLink) fail('EEXIST', 'mkdir', path);
    await this.add(parent, name, DEFAULT_DIR_MODE);
  }

  private async doSymlink(target: string, path: string): Promise<void> {
    const { parent, name, node, dirOnly, viaLink } = await this.walk(path, false, 'open');
    if (node || viaLink) fail('EEXIST', 'open', path);
    if (dirOnly) fail('ENOENT', 'open', path);
    await this.writeLink(await this.add(parent, name, S_IFLNK | 0o777), target);
  }

  private async doUnlink(path: string): Promise<void> {
    const found = await this.locate(path, 'unlink');
    if (isDir(found.node.stats)) fail('EISDIR', 'unlink', path);
    await this.remove(found);
  }

  private async doRmdir(path: string): Promise<void> {
    const found = await this.locate(path, 'rmdir');
    if (!isDir(found.node.stats)) fail('ENOTDIR', 'rmdir', path);
    if ((await this.names(found.node)).length) fail('ENOTEMPTY', 'rmdir', path);
    await this.remove(found);
  }

  private async doRm(path: string, options?: { force?: boolean; recursive?: boolean }): Promise<void> {
    const found = await this.locate(path, 'rm').catch((e) => (options?.force && e.code === 'ENOENT' ? null : Promise.reject(e)));
    if (!found) return;
    const { node } = found;
    if (!isDir(node.stats)) return this.doUnlink(path);
    if (!options?.recursive) fail('EISDIR', 'rm', path);
    for (const name of await this.names(node)) await this.doRm(join(node.path, name), { recursive: true });
    await this.doRmdir(node.path);
  }

  private async doChmod(path: string, mode: number): Promise<void> {
    if (!Number.isInteger(mode)) fail('EINVAL', 'chmod', path);
    const ino = await this.copyUp(await this.find(path, true, 'chmod'));
    await this.db.prepare('UPDATE fs_inode SET mode = (mode & ?) | ?, ctime = ? WHERE ino = ?').run(S_IFMT, mode & 0o7777, now(), ino);
  }

  private async doLink(existingPath: string, newPath: string): Promise<void> {
    const { node } = await this.locate(existingPath, 'link');
    if (isDir(node.stats)) fail('EPERM', 'link', existingPath);
    const ino = node.entry.deltaIno ?? fail('EXDEV', 'link', existingPath);
    const { parent, name, node: existing, dirOnly, viaLink } = await this.walk(newPath, false, 'link');
    if (existing || viaLink) fail('EEXIST', 'link', newPath);
    if (dirOnly) fail('ENOENT', 'link', newPath);
    await this.delta.createDentry(await this.copyUp(parent), name, ino);
  }

  private async doRename(oldPath: string, newPath: string): Promise<void> {
    const src = await this.locate(oldPath, 'rename');
    const dst = await this.walk(newPath, false, 'rename');
    if (dst.viaLink) fail('ENOTDIR', 'rename', newPath);
    const dstPath = dst.node?.path ?? join(dst.parent.path, dst.name);
    const srcDelta = src.node.entry.deltaIno;
    if (src.node.path === dstPath || (srcDelta !== undefined && srcDelta === dst.node?.entry.deltaIno)) return;
    const srcIsDir = isDir(src.node.stats);
    if (srcIsDir && dstPath.startsWith(src.node.path + '/')) fail('EINVAL', 'rename', newPath);
    if (!dst.name) fail('EINVAL', 'rename', newPath);
    if (dst.node) {
      if (isDir(dst.node.stats) !== srcIsDir) fail(srcIsDir ? 'ENOTDIR' : 'EISDIR', 'rename', newPath);
      if (srcIsDir && (await this.names(dst.node)).length) fail('ENOTEMPTY', 'rename', newPath);
    } else if (dst.dirOnly && !srcIsDir) fail('ENOTDIR', 'rename', newPath);
    const tree = await this.subtree(src.node);
    for (const n of tree) if (n.lower && n.entry.deltaIno === undefined && !copyable(n.lower.mode)) fail('ENOTSUP', 'rename', n.path);
    for (const n of tree) await this.copyUp(n);
    const dstParentIno = await this.copyUp(dst.parent);
    const dstDelta = dst.node?.entry.deltaIno;
    if (dstDelta !== undefined) await this.delta.removeDentryAndMaybeInode(dstParentIno, dst.name, dstDelta);
    const move = 'UPDATE fs_dentry SET parent_ino = ?, name = ? WHERE parent_ino = ? AND name = ?';
    await this.db.prepare(move).run(dstParentIno, dst.name, src.parent.entry.deltaIno, src.name);
    await this.hide(src.node);
    await this.hide(dst.node);
  }

  private async doCopyFile(src: string, dest: string): Promise<void> {
    await this.doWriteFile(dest, await this.readFile(src));
    await this.doChmod(dest, (await this.stat(src)).mode);
  }

  writeFile = this.locked(this.doWriteFile);
  mkdir = this.locked(this.doMkdir);
  symlink = this.locked(this.doSymlink);
  unlink = this.locked(this.doUnlink);
  rmdir = this.locked(this.doRmdir);
  rm = this.locked(this.doRm);
  chmod = this.locked(this.doChmod);
  link = this.locked(this.doLink);
  rename = this.locked(this.doRename);
  copyFile = this.locked(this.doCopyFile);

  /**
   * Compares the merged tree with the base tree without reading the whole base: it walks the
   * delta, then the base subtrees that a whiteout or a non-directory delta entry hides.
   */
  async diff(): Promise<Change[]> {
    const changes = new Map<string, Change>();
    const hidden = new Set(this.whiteouts);
    const memo = new Map<string, Promise<LowerStats | null>>();
    const base = (path: string): Promise<LowerStats | null> => {
      if (!memo.has(path)) {
        const parent = path === '/' ? Promise.resolve({ mode: S_IFDIR }) : base(parentOf(path));
        memo.set(path, parent.then((p) => (isDir(p) ? this.lower.lstat(path) : null)));
      }
      return memo.get(path)!;
    };
    const children = 'SELECT d.name, i.ino, i.mode FROM fs_dentry d JOIN fs_inode i ON i.ino = d.ino WHERE d.parent_ino = ?';
    const queue: [number, string][] = [[1, '/']];
    for (let item = queue.pop(); item; item = queue.pop()) {
      for (const r of await this.db.prepare(children).all(item[0])) {
        const path = join(item[1], r.name);
        const inBase = await base(path);
        changes.set(path, { change: inBase ? 'M' : 'A', type: typeOf(r.mode), path });
        if (isDir(r)) queue.push([Number(r.ino), path]);
        else if (isDir(inBase)) hidden.add(path);
      }
    }
    const removed = async (path: string): Promise<void> => {
      const b = await base(path);
      if (!b) return;
      if (!changes.has(path)) changes.set(path, { change: 'D', type: typeOf(b.mode), path });
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
  return { delta, info: { version: 1, basePath: config.base_path, baseIdentity: config.base_identity } };
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
  if (has) for (const r of await db.prepare('SELECT key, value FROM fs_overlay_config').all()) config.set(r.key, r.value);
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
    const tables = await closing(db).prepare("SELECT 1 FROM sqlite_master WHERE type = 'table'").all();
    if (tables.length > 0) fail('EEXIST', 'open', db.name);
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
    await db.close().finally(() => release(key));
    throw e;
  }
}
