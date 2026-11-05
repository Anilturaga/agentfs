import { Database } from '@tursodatabase/database';
import { lstat, open, readdir, readlink, realpath, rm, stat, unlink } from 'fs/promises';
import { join } from 'path';
import { createFsError } from '../errors.js';
import { claim, fail, holder, initOverlay, mount, readConfig, rebind, release, type LowerAdapter, type OverlayFS, type OverlayInfo } from './overlayfs.js';

const secs = (ms: number) => Math.floor(ms / 1000);

/** Serves the host directory `dir` as a lower layer. Fails with `ENOENT` if it does not exist. */
export async function hostLower(dir: string): Promise<LowerAdapter> {
  const locator = await realpath(dir);
  return {
    locator,
    async identity() {
      const s = await stat(locator, { bigint: true });
      return `host:${s.dev}:${s.ino}`;
    },
    async lstat(path) {
      try {
        const s = await lstat(locator + path);
        const { mode, uid, gid, size, nlink } = s;
        return { mode, uid, gid, size, nlink, atime: secs(s.atimeMs), mtime: secs(s.mtimeMs), ctime: secs(s.ctimeMs) };
      } catch (e: any) {
        if (e.code === 'ENOENT' || e.code === 'ENOTDIR') return null;
        throw e;
      }
    },
    readdir: (path) => readdir(locator + path),
    readlink: (path) => readlink(locator + path),
    async read(path, offset, length) {
      const file = await open(locator + path, 'r');
      try {
        const buf = new Uint8Array(length);
        const { bytesRead } = await file.read(buf, 0, length, offset);
        return buf.subarray(0, bytesRead);
      } finally {
        await file.close();
      }
    },
  };
}

const lowerOf = async (base: string | LowerAdapter) => (typeof base === 'string' ? hostLower(base) : base);

/** Every path, symlink and hard link to one file gets one key. */
async function fileKey(path: string): Promise<string> {
  const s = await stat(path, { bigint: true });
  if (s.nlink > 1n && holder(`file:${s.dev}:${s.ino}`) === undefined) fail('ENOTSUP', 'open', path, 'hard-linked overlay database is not supported');
  return `file:${s.dev}:${s.ino}`;
}

const lockError = (e: unknown) =>
  e instanceof Error && e.message.includes('Locking error') ? createFsError({ code: 'EBUSY', syscall: 'open' }) : e;

/** Creates the database file `dbPath` with a v1 overlay over `base`. */
export async function createOverlay(dbPath: string, base: string | LowerAdapter): Promise<OverlayInfo> {
  await (await open(dbPath, 'wx')).close();
  const key = await fileKey(dbPath);
  claim(key);
  const db = new Database(dbPath);
  try {
    const lower = await lowerOf(base);
    await db.connect();
    const { info } = await initOverlay(db, lower, Buffer);
    await db.close();
    return { path: dbPath, ...info };
  } catch (e) {
    await db.close().catch(() => {});
    await unlink(dbPath);
    throw lockError(e);
  } finally {
    release(key);
  }
}

/**
 * Opens the overlay in `dbPath` as the only writer. Without `base`, it serves the host
 * directory recorded in the database.
 */
export async function openMerged(dbPath: string, base?: string | LowerAdapter): Promise<OverlayFS> {
  const key = await fileKey(dbPath);
  claim(key);
  const db = new Database(dbPath);
  try {
    await db.connect();
    return await mount(db, key, Buffer, base === undefined ? undefined : await lowerOf(base), hostLower);
  } catch (e) {
    await db.close().catch(() => {});
    release(key);
    throw lockError(e);
  }
}

/** Runs `fn` on a connection to the existing `dbPath` while holding its writer lock. */
async function locked(dbPath: string, fn: (db: Database) => Promise<void>): Promise<void> {
  const key = await fileKey(dbPath);
  claim(key);
  const db = new Database(dbPath);
  try {
    await db.connect();
    await fn(db);
  } catch (e) {
    throw lockError(e);
  } finally {
    await db.close().catch(() => {});
    release(key);
  }
}

/** Deletes the overlay database `dbPath` and its engine sidecar files, the main file last. */
export const deleteOverlay = (dbPath: string) =>
  locked(dbPath, async (db) => {
    if (!(await readConfig(db))) fail('EINVAL', 'open', dbPath);
    await db.close();
    for (const suffix of ['-wal', '-shm', '-log', '-wal-revert', '-info', '-changes', '']) await rm(dbPath + suffix, { force: true });
  });

/** Points the v1 overlay in `dbPath` at `base`. Only this call rebinds; `openMerged` never does. */
export const rebindOverlay = (dbPath: string, base: string | LowerAdapter) =>
  locked(dbPath, async (db) => rebind(db, await lowerOf(base)));

/** The overlays directly in `dir`, sorted by path; with `base`, only those over it. Skips a file that cannot be read. */
export async function listOverlays(dir: string, base?: string | LowerAdapter): Promise<OverlayInfo[]> {
  const lower = base === undefined ? undefined : await lowerOf(base);
  const identity = await lower?.identity();
  const found: OverlayInfo[] = [];
  for (const entry of (await readdir(dir, { withFileTypes: true })).sort((a, b) => (a.name < b.name ? -1 : 1))) {
    if (!entry.isFile() || !entry.name.endsWith('.db')) continue;
    const path = join(dir, entry.name);
    const config = await configAt(path).catch(() => undefined);
    if (!config) continue;
    const info = { path, version: Number(config.get('version') ?? 0), basePath: config.get('base_path') ?? '', baseIdentity: config.get('base_identity') ?? '' };
    if (!lower || (info.baseIdentity ? info.baseIdentity === identity : info.basePath === lower.locator)) found.push(info);
  }
  return found;
}

/** Reads through the instance that holds `path`, else through a read-only connection. */
async function configAt(path: string) {
  const held = holder(await fileKey(path));
  if (held !== undefined) return held?.config();
  const db = new Database(path, { readonly: true });
  try {
    await db.connect();
    return await readConfig(db);
  } finally {
    await db.close().catch(() => {});
  }
}
