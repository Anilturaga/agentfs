// Runs one case of conformance/overlay/cases.json: `node overlay-conformance-worker.mjs <index>`.
// Exit codes: 0 every step matched, 1 a step result did not match, 2 harness error.
import { Database } from '@tursodatabase/database';
import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = fileURLToPath(new URL('.', import.meta.url));
const sdk = await import('../dist/index_node.js');
const doc = JSON.parse(readFileSync(join(here, '../../../conformance/overlay/cases.json'), 'utf8'));
const FIXTURE = join(here, '../../rust/tests/fixtures/legacy_rust_overlay.db');

const root = mkdtempSync(join(tmpdir(), 'overlay-case-'));
for (const d of ['base', 'other', 'dir']) mkdirSync(join(root, d));
const at = (p) => resolve(root, p);
const host = (p) => join(root, 'base', p);

const field = (s, name = 'type') =>
  ({
    mode: () => (s.mode & 0o7777).toString(8),
    size: () => String(s.size),
    ino: () => String(s.ino),
    type: () => (s.isFile() ? 'f' : s.isDirectory() ? 'd' : s.isSymbolicLink() ? 'l' : '?'),
  })[name]();

async function sql(db, statement) {
  const conn = new Database(db);
  await conn.connect();
  try {
    const rows = await conn.prepare(statement).all();
    return rows.map((r) => Object.values(r).map((v) => (v === null ? 'NULL' : String(v))).join('|')).join('; ');
  } finally {
    await conn.close();
  }
}

let fs = null;
let handle = null;
let current = 'db';

const ops = {
  readdir: async (p) => (await fs.readdir(p)).join(' '),
  stat: async (p, f) => field(await fs.stat(p), f),
  lstat: async (p, f) => field(await fs.lstat(p), f),
  plus: async (dir, name, f) => {
    const entry = (await fs.readdirPlus(dir)).find((e) => e.name === name);
    if (!entry) throw Object.assign(new Error('missing'), { code: 'ENOENT' });
    return field(entry.stats, f);
  },
  read: (p) => fs.readFile(p, 'utf8'),
  write: async (p, ...s) => void (await fs.writeFile(p, s.join(' '))),
  readlink: (p) => fs.readlink(p),
  mkdir: (p) => fs.mkdir(p),
  rmdir: (p) => fs.rmdir(p),
  unlink: (p) => fs.unlink(p),
  chmod: (p, m) => fs.chmod(p, parseInt(m, 8)),
  symlink: (t, p) => fs.symlink(t, p),
  link: (s, d) => fs.link(s, d),
  rename: (s, d) => fs.rename(s, d),
  open: async (p) => void (handle = await fs.open(p)),
  hread: async () => (await handle.pread(0, (await handle.fstat()).size)).toString(),
  diff: async () => (await fs.diff()).map((c) => `${c.change} ${c.type} ${c.path}`).join('; '),
  openMerged: async (db, base) => {
    fs = await sdk.openMerged(at(db), base && at(base));
    current = db;
  },
  close: async () => {
    const closing = fs;
    fs = handle = null;
    await closing.close();
  },
  reopen: async () => {
    await ops.close();
    await ops.openMerged(current);
  },
  createOverlay: async (db, base) => void (await sdk.createOverlay(at(db), at(base))),
  deleteOverlay: (db) => sdk.deleteOverlay(at(db)),
  rebindOverlay: (db, base) => sdk.rebindOverlay(at(db), at(base)),
  listOverlays: async (dir, base) => (await sdk.listOverlays(at(dir), base && at(base))).map((o) => o.path.replace(`${root}/`, '')).join(' '),
  plainDb: async (db) => void (await (await sdk.AgentFS.open({ path: at(db) })).close()),
  legacyRust: async (db, base) => {
    copyFileSync(FIXTURE, at(db));
    await sql(at(db), `UPDATE fs_overlay_config SET value = '${at(base)}' WHERE key = 'base_path'`);
  },
  sql: (db, ...s) => sql(at(db), s.join(' ')),
  hostRead: (p) => readFileSync(host(p), 'utf8'),
  hostWrite: (p, ...s) => writeFileSync(host(p), s.join(' ')),
  hostRm: (p) => rmSync(host(p)),
  hostMkdir: (p) => mkdirSync(host(p)),
};

async function run(step) {
  const [cmd, want] = step.split('=>').map((s) => s.trim());
  const bound = /^(\w+) = (.*)$/.exec(cmd);
  const words = (bound ? bound[2] : cmd).split(/\s+/);
  let out;
  let ok = true;
  try {
    if (typeof ops[words[0]] !== 'function') throw Object.assign(new Error('ENOSYS'), { code: 'ENOSYS' });
    out = String((await ops[words[0]](...words.slice(1))) ?? '');
  } catch (e) {
    ok = false;
    out = e.code ?? e.message;
  }
  return { out, ok, want: step.includes('=>') ? want : undefined, name: bound?.[1] };
}

const satisfied = ({ want, ok, out }) => {
  if (want === undefined) return ok;
  return want.startsWith('!') ? ok && out !== want.slice(1) : out === want;
};

const vars = {};
let code = 0;
try {
  execFileSync('sh', ['-ec', doc.base], { cwd: join(root, 'base') });
  await ops.createOverlay('db', 'base');
  await ops.openMerged('db', 'base');
  for (const step of doc.cases[Number(process.argv[2])].steps) {
    const subst = step.replace(/\$(\w+)/g, (m, k) => vars[k] ?? m);
    const result = await run(subst);
    if (!satisfied(result)) {
      console.error(`\`${step}\` gave \`${result.out}\``);
      code = 1;
      break;
    }
    if (result.name) vars[result.name] = result.out;
  }
} catch (e) {
  console.error(e);
  code = 2;
}
process.exit(code);
