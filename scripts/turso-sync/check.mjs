#!/usr/bin/env node
// Checks Temper's Turso Sync copy end to end with a real `temper serve`.
//
//   node scripts/turso-sync/check.mjs kill   # confirmed writes survive a killed server and a lost disk
//   node scripts/turso-sync/check.mjs time   # time per write and per burst, with and without the copy
//   node scripts/turso-sync/check.mjs readonly  # a start without the remote serves reads, refuses writes, recovers
//
// Env:
//   TEMPER_BIN        temper binary (default: target/release/temper, then target/debug/temper)
//   TURSODB_BIN       tursodb 0.7.2, used as the remote when SYNC_URL is not set (`tursodb --sync-server`)
//   SYNC_URL          a real remote (libsql://... on Turso Cloud) instead of the local sync server
//   SYNC_TOKEN_FILE   file holding that remote's token (read, never printed)
//   SECONDS_BEFORE_KILL  how long writers run before the kill (default 6)
//   KEEP_LOGS=1       keep the servers' logs after a passing run
//   REMOTE_DELAY_MS   one-way delay added in front of the local sync server, to stand in for a remote
//                     at that distance (Tensorlake to Turso aws-us-east-2 is about 15 ms one way)
//
// kill: writers send writes with 32 in flight and log every write answered 2xx. The server is killed
// with SIGKILL mid-burst and its whole data folder deleted. A new server on an empty folder restores
// from the remote, and every logged write must be there. The background push is set to once an hour
// for this run, so only the push each answer waits for can save the writes.
import { spawn } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import crypto from "node:crypto";

const here = path.dirname(new URL(import.meta.url).pathname);
const repo = path.resolve(here, "../..");
const temperBin = process.env.TEMPER_BIN || [`${repo}/target/release/temper`, `${repo}/target/debug/temper`].find((p) => fs.existsSync(p));
const work = fs.mkdtempSync(path.join(os.tmpdir(), "temper-turso-sync-"));
const apiKey = crypto.randomBytes(24).toString("hex");
const vaultKey = crypto.randomBytes(32).toString("base64");
const run = crypto.randomBytes(3).toString("hex");
const children = [];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const out = (o) => console.log(JSON.stringify(o));
const quantile = (xs, q) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length ? +s[Math.min(s.length - 1, Math.floor(q * s.length))].toFixed(1) : null;
};
const summary = (xs) => ({ count: xs.length, median_ms: quantile(xs, 0.5), slowest_5pct_ms: quantile(xs, 0.95), max_ms: quantile(xs, 1) });

function freePort() {
  return new Promise((resolve) => {
    const srv = net.createServer().listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

function start(cmd, args, env, logName) {
  const log = fs.openSync(path.join(work, logName), "a");
  const child = spawn(cmd, args, { env: { ...process.env, ...env }, stdio: ["ignore", log, log] });
  children.push(child);
  return child;
}

async function remote() {
  if (process.env.SYNC_URL) {
    return { url: process.env.SYNC_URL, token: fs.readFileSync(process.env.SYNC_TOKEN_FILE, "utf8").trim() };
  }
  const port = await freePort();
  start(process.env.TURSODB_BIN || "tursodb", [path.join(work, "remote.db"), "--sync-server", `127.0.0.1:${port}`], {}, "tursodb.log");
  for (let i = 0; i < 200; i++) {
    if (await fetch(`http://127.0.0.1:${port}/`).then(() => true, () => false)) break;
    await sleep(50);
  }
  const delay = Number(process.env.REMOTE_DELAY_MS || 0);
  if (!delay) return { url: `http://127.0.0.1:${port}`, token: "local" };
  const proxyPort = await freePort();
  delayProxy(proxyPort, port, delay);
  return { url: `http://127.0.0.1:${proxyPort}`, token: "local" };
}

// Forwards to `upstream` while `on.value`; otherwise closes new connections at once.
function switchProxy(listen, upstream, on) {
  net.createServer((client) => {
    if (!on.value) return client.destroy();
    const up = net.connect(upstream.port, upstream.host);
    client.pipe(up).pipe(client);
    for (const socket of [client, up]) socket.on("error", () => { client.destroy(); up.destroy(); });
  }).listen(listen, "127.0.0.1");
}

// Every chunk waits `delay` ms in each direction, in order, so a request and its answer cost twice that.
function delayProxy(listen, upstream, delay) {
  net.createServer((client) => {
    const up = net.connect(upstream, "127.0.0.1");
    client.setNoDelay(true);
    up.setNoDelay(true);
    // An end travels behind the data before it, so a closing answer is not cut short.
    const pass = (from, to) => {
      from.on("data", (chunk) => setTimeout(() => to.write(chunk), delay));
      from.on("end", () => setTimeout(() => to.end(), delay));
    };
    pass(client, up);
    pass(up, client);
    const fail = () => { client.destroy(); up.destroy(); };
    for (const socket of [client, up]) socket.on("error", fail);
  }).listen(listen, "127.0.0.1");
}

async function temper(name, sync, extraEnv = {}) {
  const home = path.join(work, name);
  fs.mkdirSync(home, { recursive: true });
  const port = await freePort();
  const env = {
    HOME: home,
    TURSO_URL: `file:${home}/app.db`,
    TEMPER_API_KEY: apiKey,
    TEMPER_VAULT_KEY: vaultKey,
    TEMPER_OS_APPS_DIR: path.join(here, "app"),
    ...(sync ? { TURSO_SYNC_URL: sync.url, TURSO_SYNC_AUTH_TOKEN: sync.token } : {}),
    ...extraEnv,
  };
  const t0 = Date.now();
  const child = start(temperBin, ["serve", "--port", String(port), "--tenant", "default", "--storage", "turso", "--no-observe", "--app", "appdbprobe"], env, `${name}.log`);
  const base = `http://127.0.0.1:${port}`;
  for (let i = 0; ; i++) {
    if (child.exitCode !== null) throw new Error(`${name} exited ${child.exitCode}; see ${work}/${name}.log`);
    const res = await get(base, "/tdata/ProbeNotes?$top=1");
    if (res.status === 200) break;
    if (i > 2400) throw new Error(`${name} did not answer; see ${work}/${name}.log`);
    await sleep(50);
  }
  return { child, base, home, startup_ms: Date.now() - t0 };
}

const headers = { Authorization: `Bearer ${apiKey}`, "X-Tenant-Id": "default" };
const BODY = "x".repeat(1500);

async function write(base, id, i) {
  const t = performance.now();
  try {
    const res = await fetch(`${base}/tdata/ProbeNotes('${id}')/Appdb.Write`, {
      method: "POST",
      headers: { ...headers, "Content-Type": "application/json" },
      body: JSON.stringify({ title: `${id} ${i}`, body: BODY }),
      signal: AbortSignal.timeout(60_000),
    });
    const text = await res.text();
    return { ok: res.ok, status: res.status, ms: performance.now() - t, text: res.ok ? "" : text.slice(0, 200) };
  } catch (error) {
    return { ok: false, status: 0, ms: performance.now() - t, text: String(error).slice(0, 200) };
  }
}

async function get(base, route) {
  const t = performance.now();
  try {
    const res = await fetch(`${base}${route}`, { headers, signal: AbortSignal.timeout(60_000) });
    return { status: res.status, text: await res.text(), ms: performance.now() - t };
  } catch (error) {
    return { status: 0, text: String(error), ms: performance.now() - t };
  }
}

// Key reads one after another until `until()` is true.
async function readLoop(base, ids, until) {
  const ms = [];
  for (let i = 0; !until(); i++) {
    const r = await get(base, `/tdata/ProbeNotes('${ids[i % ids.length]}')`);
    if (r.status === 200) ms.push(r.ms);
  }
  return ms;
}

async function killCheck() {
  const sync = await remote();
  const a = await temper("a", sync, { TEMPER_TURSO_SYNC_INTERVAL_MS: "3600000" });
  const acked = new Map();
  let stop = false;
  let failed = 0;
  const writers = Array.from({ length: 32 }, (_, w) => (async () => {
    for (let j = 0; !stop; j++) {
      const id = `${run}-${w}-${j % 4}`;
      const r = await write(a.base, id, j);
      if (r.ok) acked.set(id, (acked.get(id) || 0) + 1);
      else failed++;
    }
  })());
  await sleep(Number(process.env.SECONDS_BEFORE_KILL || 6) * 1000);
  a.child.kill("SIGKILL");
  const killedAt = [...acked.values()].reduce((x, y) => x + y, 0);
  stop = true;
  await Promise.all(writers);
  const ackedTotal = [...acked.values()].reduce((x, y) => x + y, 0);
  fs.rmSync(a.home, { recursive: true, force: true });

  const b = await temper("b", sync);
  let lost = 0;
  let extra = 0;
  const missing = [];
  for (const [id, n] of acked) {
    const res = await get(b.base, `/tdata/ProbeNotes('${id}')`);
    let have = 0;
    try {
      const body = JSON.parse(res.text);
      have = Number(body.counters?.n ?? body.fields?.n ?? body.n ?? 0);
    } catch {
      have = 0;
    }
    if (have < n) {
      lost += n - have;
      missing.push(`${id}: confirmed ${n}, restored ${have}`);
    }
    extra += Math.max(0, have - n);
  }
  out({ check: "kill", remote: process.env.SYNC_URL ? "turso cloud" : "local sync server", confirmed: ackedTotal, confirmed_before_kill: killedAt, failed_after_kill: failed, records: acked.size, lost, applied_but_not_confirmed: extra, restore_startup_ms: b.startup_ms, missing: missing.slice(0, 8) });
  return lost === 0 && ackedTotal > 0;
}

async function timeCheck() {
  const sync = await remote();
  const results = {};
  for (const [label, copy] of [["local file only", null], ["local file + Turso Sync", sync]]) {
    const t = await temper(label === "local file only" ? "plain" : "synced", copy);
    const single = [];
    const errors = new Map();
    const note = (r) => { if (!r.ok) errors.set(`${r.status} ${r.text}`, (errors.get(`${r.status} ${r.text}`) || 0) + 1); };
    for (let i = 0; i < 100; i++) {
      const r = await write(t.base, `${run}-s-${i % 10}`, i);
      note(r);
      if (r.ok) single.push(r.ms);
    }
    const readIds = Array.from({ length: 10 }, (_, i) => `${run}-s-${i}`);
    let readsDone = false;
    const quiet = await readLoop(t.base, readIds, ((n) => () => n-- <= 0)(200));
    const burst = [];
    const wall = [];
    const during = [];
    for (let round = 0; round < 3; round++) {
      const t0 = performance.now();
      readsDone = false;
      const reader = readLoop(t.base, readIds, () => readsDone);
      const rs = await Promise.all(Array.from({ length: 100 }, (_, i) => write(t.base, `${run}-b${round}-${i}`, 0)));
      wall.push(+(performance.now() - t0).toFixed(0));
      readsDone = true;
      during.push(...(await reader));
      for (const r of rs) { note(r); if (r.ok) burst.push(r.ms); }
    }
    const failed = 100 - single.length + 300 - burst.length;
    results[label] = {
      startup_ms: t.startup_ms,
      one_at_a_time: summary(single),
      hundred_at_once: { ...summary(burst), wall_ms: wall },
      key_read_alone: summary(quiet),
      key_read_during_hundred_writes: summary(during),
      failed_writes: failed,
      errors: [...errors].slice(0, 4).map(([text, n]) => `${n}x ${text}`),
    };
    t.child.kill("SIGTERM");
    await sleep(500);
  }
  out({ check: "time", remote: process.env.SYNC_URL ? "turso cloud" : "local sync server", ...results });
  return Object.values(results).every((r) => r.failed_writes === 0);
}

async function readonlyCheck() {
  // The switch sits in front of the local sync server (or a real remote's host).
  const sync = await remote();
  const target = new URL(sync.url.replace(/^libsql:/, "https:"));
  if (target.protocol !== "http:") throw new Error("readonly runs against the local sync server");
  const on = { value: true };
  const port = await freePort();
  switchProxy(port, { host: target.hostname, port: Number(target.port) }, on);
  const through = { url: `http://127.0.0.1:${port}`, token: sync.token };

  const a = await temper("a", through);
  for (let i = 0; i < 5; i++) if (!(await write(a.base, `${run}-ro`, i)).ok) throw new Error("setup write failed");
  a.child.kill("SIGTERM");
  await new Promise((r) => a.child.once("exit", r));

  on.value = false;
  const b = await temper("a", through);
  const read = await get(b.base, `/tdata/ProbeNotes('${run}-ro')`);
  const paused = await write(b.base, `${run}-ro`, 5);
  on.value = true;
  const t0 = Date.now();
  let resumed = null;
  for (let i = 0; i < 100 && !resumed?.ok; i++) {
    resumed = await write(b.base, `${run}-ro`, 6);
    if (!resumed.ok) await sleep(200);
  }
  b.child.kill("SIGKILL");
  fs.rmSync(b.home, { recursive: true, force: true });
  const c = await temper("c", sync);
  const restored = JSON.parse((await get(c.base, `/tdata/ProbeNotes('${run}-ro')`)).text);
  const n = Number(restored.counters?.n ?? restored.fields?.n ?? restored.n ?? 0);
  const result = {
    check: "readonly",
    read_while_down: read.status,
    write_while_down: `${paused.status} ${paused.text.slice(0, 80)}`,
    write_after_remote_back_ms: resumed?.ok ? Date.now() - t0 : null,
    confirmed: 6,
    restored_n: n,
  };
  out(result);
  return read.status === 200 && paused.status === 503 && resumed?.ok && n >= 6;
}

let ok = false;
try {
  if (!temperBin) throw new Error("no temper binary: build it or set TEMPER_BIN");
  const mode = process.argv[2];
  if (mode === "kill") ok = await killCheck();
  else if (mode === "time") ok = await timeCheck();
  else if (mode === "readonly") ok = await readonlyCheck();
  else throw new Error("usage: check.mjs kill|time|readonly");
} catch (error) {
  console.error(String(error));
} finally {
  for (const child of children) if (child.exitCode === null) child.kill("SIGKILL");
  if (ok && !process.env.KEEP_LOGS) fs.rmSync(work, { recursive: true, force: true });
  else console.error(`logs kept in ${work}`);
}
process.exit(ok ? 0 : 1);
