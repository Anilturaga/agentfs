import { Database } from '@tursodatabase/database';
import { lstat, open, readdir, readlink, realpath, stat, unlink } from 'fs/promises';
import { createFsError } from '../errors.js';
import { claim, initOverlay, mount, release, type LowerAdapter, type OverlayFS, type OverlayInfo } from './overlayfs.js';

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
    release(key);
    await db.close().catch(() => {});
    throw lockError(e);
  }
}
