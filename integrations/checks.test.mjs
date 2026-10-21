import assert from "node:assert/strict";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { createCheckClient } from "./checks.js";

// Stateful subprocess boundary: requests persist until cancellation/release, and
// manual ownership deliberately survives automatic release. No Docker/model calls.
async function fixture(t, initial = {}) {
  const directory = await mkdtemp(path.join(tmpdir(), "crit-check-hooks-"));
  const cleanup = [];
  let client;
  t.after(async () => {
    try {
      for (const dispose of cleanup) await dispose();
      await client?.finish(directory);
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  });
  const state = path.join(directory, "state.json");
  const bin = path.join(directory, "crit executable; not a shell");
  await writeFile(state, JSON.stringify({ releases: 0, ...initial }));
  await writeFile(bin, `#!${process.execPath}
const fs = require("node:fs");
const file = process.env.FIXTURE_STATE;
const read = () => JSON.parse(fs.readFileSync(file, "utf8"));
const save = value => fs.writeFileSync(file, JSON.stringify(value));
const args = process.argv.slice(2);
if (args[0] !== "check") process.exit(10);
const state = read();
if (args[1] === "acquire") {
  state.pid = process.pid;
  state.repo = args[args.indexOf("--repo") + 1];
  state.owner = [process.env.CRIT_WINDOW_PID, process.env.CRIT_SESSION, process.env.CRIT_AGENT_PID];
  if (state.wait) {
    state.queued = true; save(state);
    process.on("SIGTERM", () => { const s = read(); s.queued = false; s.cancelled = true; save(s); process.exit(0); });
    process.stderr.write("Waiting for exclusive check turn\\n");
    setInterval(() => {}, 60000);
  } else {
    state.active = true; save(state);
    process.stdout.write(JSON.stringify({ phase: "active", endpoint: "http://127.0.0.1:4321" }));
  }
} else if (args[1] === "release") {
  if (!args.includes("--automatic")) process.exit(11);
  state.releases++; state.active = false; state.queued = false; save(state);
  process.stdout.write(JSON.stringify(state));
} else if (args[1] === "status") {
  if (state.fail) { process.stderr.write("bad health: " + "x".repeat(80000)); process.exitCode = 2; }
  else process.stdout.write(JSON.stringify(state));
} else process.exit(12);
`);
  await chmod(bin, 0o700);
  const env = { ...process.env, CRIT_ENV: "1", CRIT_BIN: bin, CRIT_WINDOW_PID: "1234",
    CRIT_SESSION: "18446744073709551615", CRIT_AGENT_PID: String(process.pid), FIXTURE_STATE: state };
  client = createCheckClient(env);
  return { directory, env, client, cleanup, state: async () => JSON.parse(await readFile(state, "utf8")) };
}

function ready() {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, progress: resolve };
}

test("queued native acquisition abort kills its client and removes pending ownership", async (t) => {
  const f = await fixture(t, { wait: true, manual: true });
  const controller = new AbortController();
  const waiting = ready();
  const acquisition = f.client.execute("acquire", { cwd: f.directory, signal: controller.signal, progress: waiting.progress });
  const rejected = assert.rejects(acquisition, /cancelled by user/);
  await waiting.promise;
  const pid = (await f.state()).pid;
  controller.abort(new Error("cancelled by user"));
  await rejected;
  const state = await f.state();
  assert.equal(state.cancelled, true);
  assert.equal(state.queued, false);
  assert.equal(state.manual, true);
  assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
});

test("finish drains a queued acquisition before a subsequent turn can acquire", async (t) => {
  const f = await fixture(t, { wait: true });
  const waiting = ready();
  const acquisition = f.client.execute("acquire", { cwd: f.directory, progress: waiting.progress });
  const rejected = assert.rejects(acquisition, /Agent turn ended/);
  await waiting.promise;
  await f.client.finish(f.directory);
  await rejected;
  const state = await f.state();
  assert.equal(state.cancelled, true);
  assert.equal(state.active, false);
  assert.equal(state.queued, false);
  assert.ok(state.releases >= 1);
});

test("exact argument boundaries and owner identity survive acquisition, manual turn survives finish", async (t) => {
  const f = await fixture(t, { manual: true });
  const repo = path.join(f.directory, "checkout with spaces; $(not-a-shell)");
  const view = JSON.parse(await f.client.execute("acquire", { cwd: repo }));
  assert.equal(view.endpoint, "http://127.0.0.1:4321");
  let state = await f.state();
  assert.equal(state.repo, repo);
  assert.deepEqual(state.owner, [f.env.CRIT_WINDOW_PID, f.env.CRIT_SESSION, String(process.pid)]);
  await f.client.finish(f.directory);
  state = await f.state();
  assert.equal(state.active, false);
  assert.equal(state.manual, true);
});

test("already-aborted acquisition creates no queued client; errors and progress stay bounded", async (t) => {
  const f = await fixture(t, { fail: true });
  await assert.rejects(f.client.execute("acquire", { signal: AbortSignal.abort(new Error("already aborted")) }), /already aborted/);
  assert.equal((await f.state()).pid, undefined);
  const chunks = [];
  await assert.rejects(f.client.execute("status", { progress: (text) => chunks.push(text) }), (error) => error.message.length <= 32768);
  assert.ok(chunks.length > 0);
  assert.ok(chunks.every((chunk) => chunk.length <= 2048));
  assert.equal(createCheckClient({ ...f.env, CRIT_AGENT_PID: String(process.pid + 1) }), undefined);
});

async function loadHook(name) {
  let source = await readFile(new URL(name, import.meta.url), "utf8");
  const reporter = `export const createReporter = () => ({ report: async () => {} });`;
  source = source.replace('"./reporter.js"', JSON.stringify(`data:text/javascript,${encodeURIComponent(reporter)}`));
  source = source.replace('"./checks.js"', JSON.stringify(new URL("./checks.js", import.meta.url).href));
  // API-shaped registration fixture only; subprocess/ownership behavior is real.
  const sdk = `export const tool = Object.assign(x => x, { schema: { enum: values => ({ values }) } });`;
  source = source.replace('import.meta.resolve("@opencode-ai/plugin/tool",\n      pathToFileURL(join(config, "package.json")).href)', JSON.stringify(`data:text/javascript,${encodeURIComponent(sdk)}`));
  return import(`data:text/javascript,${encodeURIComponent(source)}#${Math.random()}`);
}

async function launchEnv(t, env) {
  const keys = [...new Set([...Object.keys(env), "OMPCODE"])];
  const before = Object.fromEntries(keys.map((key) => [key, process.env[key]]));
  Object.assign(process.env, env);
  delete process.env.OMPCODE;
  t.after(() => {
    for (const key of keys) {
      if (before[key] === undefined) delete process.env[key];
      else process.env[key] = before[key];
    }
  });
}

test("OMP continuation retains automatic turn; final/abort and shutdown drain it", async (t) => {
  const f = await fixture(t, { manual: true });
  await launchEnv(t, f.env);
  const hook = await loadHook("omp.ts");
  const handlers = new Map();
  let tool;
  hook.default({ on: (name, fn) => handlers.set(name, fn), registerTool: (value) => { tool = value; },
    zod: { object: (value) => value, enum: (value) => value } });
  const ctx = { cwd: f.directory, hasUI: true, isIdle: () => true,
    sessionManager: { getSessionId: () => "root", getSessionFile: () => undefined } };
  await handlers.get("session_start")({}, ctx);
  f.cleanup.push(() => handlers.get("session_shutdown")({}, ctx));
  const prompt = handlers.get("before_agent_start")({ systemPrompt: ["user instruction"] }, ctx);
  assert.equal(prompt.systemPrompt[0], "user instruction");
  await handlers.get("agent_start")({}, ctx);
  await tool.execute("call", { action: "acquire" }, undefined, undefined, ctx);
  await handlers.get("agent_end")({ willContinue: true }, ctx);
  assert.equal((await f.state()).active, true);
  await handlers.get("agent_end")({ messages: [{ role: "assistant", stopReason: "aborted" }] }, ctx);
  assert.equal((await f.state()).active, false);
  assert.equal((await f.state()).manual, true);
  await tool.execute("call", { action: "acquire" }, undefined, undefined, ctx);
  await handlers.get("session_shutdown")({}, ctx);
  assert.equal((await f.state()).active, false);
});

test("OpenCode root idle/error, cancellation, selection switch and disposal release automatic ownership", async (t) => {
  const f = await fixture(t, { manual: true });
  await launchEnv(t, f.env);
  const hook = await loadHook("opencode.js");
  const handlers = await hook.default.server({ directory: f.directory,
    client: { session: { get: async ({ path: { id } }) => id === "unknown" ? { error: "not found" } : ({ data: { id, ...(id === "child" ? { parentID: "root" } : {}) } }) } } });
  const ctx = { sessionID: "root", directory: f.directory, abort: new AbortController().signal, metadata() {} };
  f.cleanup.push(() => handlers.dispose());
  const acquire = () => handlers.tool.crit_check.execute({ action: "acquire" }, ctx);
  const event = (type, data = {}) => handlers.event({ event: { type, properties: { sessionID: "root", ...data } } });
  await handlers["chat.message"]({ sessionID: "root" });
  await acquire();
  await handlers["chat.message"]({ sessionID: "child" });
  await event("session.idle", { sessionID: "child" });
  assert.equal((await f.state()).active, true, "child activity cannot release the root turn");
  await assert.rejects(handlers.tool.crit_check.execute({ action: "acquire" }, { ...ctx, sessionID: "child" }), /root session/);
  await assert.rejects(handlers.tool.crit_check.execute({ action: "status" }, { ...ctx, sessionID: "unknown" }), /root session/);
  await event("session.status", { status: { type: "idle" } });
  assert.equal((await f.state()).active, false);
  await acquire();
  await event("session.error", { error: { name: "MessageAbortedError" } });
  assert.equal((await f.state()).active, false);
  await acquire();
  await handlers["chat.message"]({ sessionID: "other" });
  assert.equal((await f.state()).active, false);
  await acquire();
  await handlers.dispose();
  assert.equal((await f.state()).active, false);
  assert.equal((await f.state()).manual, true);
});

test("OpenCode abort signal interrupts a native tool queued on the CLI", async (t) => {
  const f = await fixture(t, { wait: true });
  await launchEnv(t, f.env);
  const hook = await loadHook("opencode.js");
  const handlers = await hook.default.server({ directory: f.directory,
    client: { session: { get: async ({ path: { id } }) => ({ data: { id } }) } } });
  f.cleanup.push(() => handlers.dispose());
  const controller = new AbortController();
  const waiting = ready();
  const acquisition = handlers.tool.crit_check.execute({ action: "acquire" }, {
    sessionID: "root", directory: f.directory, abort: controller.signal,
    metadata: waiting.progress,
  });
  const rejected = assert.rejects(acquisition, /native abort/);
  await waiting.promise;
  controller.abort(new Error("native abort"));
  await rejected;
  assert.equal((await f.state()).cancelled, true);
  assert.equal((await f.state()).queued, false);
  await handlers.dispose();
});

test("OMP session shutdown cancels an outstanding queued native tool", async (t) => {
  const f = await fixture(t, { wait: true });
  await launchEnv(t, f.env);
  const hook = await loadHook("omp.ts");
  const handlers = new Map();
  let tool;
  hook.default({ on: (name, fn) => handlers.set(name, fn), registerTool: (value) => { tool = value; },
    zod: { object: (value) => value, enum: (value) => value } });
  const ctx = { cwd: f.directory, hasUI: true, isIdle: () => true,
    sessionManager: { getSessionId: () => "root", getSessionFile: () => undefined } };
  await handlers.get("session_start")({}, ctx);
  f.cleanup.push(() => handlers.get("session_shutdown")({}, ctx));
  const waiting = ready();
  const acquisition = tool.execute("queued", { action: "acquire" }, undefined, waiting.progress, ctx);
  const rejected = assert.rejects(acquisition, /Agent turn ended/);
  await waiting.promise;
  await handlers.get("session_shutdown")({}, ctx);
  await rejected;
  assert.equal((await f.state()).cancelled, true);
  assert.equal((await f.state()).queued, false);
});
