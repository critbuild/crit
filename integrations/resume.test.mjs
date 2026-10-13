// Hook behavior regressions using API-shaped lifecycle fixtures, not live model calls.
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";

let nextFixture = 0;
async function hook(name, initial) {
  const key = `critResumeFixture${nextFixture++}`;
  const fixture = { reports: [], effects: [] };
  globalThis[key] = fixture;
  const reporter = `export function createReporter() { return { report(state, message, current = () => true, identity) {
    if (current()) globalThis[${JSON.stringify(key)}].reports.push({state, message, identity});
    return Promise.resolve();
  } }; }`;
  const solid = `export function createEffect(fn) { globalThis[${JSON.stringify(key)}].effects.push(fn); fn(); }
    export function createRoot(fn) { return fn(() => {}); }
    export function on(_source, fn) { return fn; }
    export function untrack(fn) { return fn(); }`;
  let source = await readFile(new URL(name, import.meta.url), "utf8");
  source = source.replace('"./reporter.js"', JSON.stringify(`data:text/javascript,${encodeURIComponent(reporter)}`));
  source = source.replace('"solid-js"', JSON.stringify(`data:text/javascript,${encodeURIComponent(solid)}`));
  source = source.replaceAll("process.env.CRIT_RESUME_ID", initial === undefined ? "undefined" : JSON.stringify(initial));
  source += `\n// ${key}\n`;
  fixture.module = await import(`data:text/javascript,${encodeURIComponent(source)}`);
  return fixture;
}

// Drain finite promise work in the fixtures; there are no polling timers.
async function settle() {
  for (let i = 0; i < 20; i++) await Promise.resolve();
}

test("OMP same-directory switches, lazy durability and child isolation", async () => {
  const directory = await mkdtemp(path.join(tmpdir(), "crit-resume-"));
  const fixture = await hook("omp.ts");
  const handlers = new Map();
  const previousOwner = process.env.OMPCODE;
  delete process.env.OMPCODE;
  fixture.module.default({ on: (name, handler) => handlers.set(name, handler) });
  let id = "first";
  let file = path.join(directory, "first.jsonl");
  const ctx = { hasUI: true, isIdle: () => true,
    sessionManager: { getSessionId: () => id, getSessionFile: () => file } };
  try {
    await writeFile(file, "durable-first");
    await handlers.get("session_start")({}, ctx);
    assert.equal(fixture.reports.at(-1).identity, file);
    id = "second";
    file = path.join(directory, "second.jsonl");
    await handlers.get("session_switch")({ reason: "new" }, ctx);
    assert.equal(fixture.reports.at(-1).identity, null, "new unsaved chat clears old identity");
    const count = fixture.reports.length;
    await handlers.get("session_start")({}, { ...ctx, hasUI: false });
    await handlers.get("agent_start")({}, { ...ctx, sessionManager: { getSessionId: () => "child" } });
    assert.equal(fixture.reports.length, count, "subagents cannot steal selection");
    await writeFile(file, "durable-second");
    await handlers.get("agent_end")({ willContinue: true }, ctx);
    assert.equal(fixture.reports.at(-1).identity, file, "first durable chat identity is refreshed");
    await rm(file);
    await handlers.get("agent_end")({ willContinue: true }, ctx);
    assert.equal(fixture.reports.at(-1).identity, undefined, "transient disappearance does not erase durable chat");
    id = "branched";
    file = path.join(directory, "branched.jsonl");
    await writeFile(file, "durable-branched");
    await handlers.get("session_branch")({ reason: "btw" }, ctx);
    assert.equal(fixture.reports.at(-1).identity, file, "a branch selects the new session file");
    await handlers.get("agent_start")({}, ctx);
    assert.equal(fixture.reports.at(-1).state, "working", "the branched session keeps reporting");
    await handlers.get("session_shutdown")({}, ctx);
  } finally {
    if (previousOwner === undefined) delete process.env.OMPCODE;
    else process.env.OMPCODE = previousOwner;
    await rm(directory, { recursive: true, force: true });
  }
});

test("OpenCode TUI root boundaries, route changes, reconnect and disposal", async () => {
  const fixture = await hook("opencode-tui.js");
  const events = new Map();
  let dispose;
  const sessions = new Map([
    ["root-a", { id: "root-a" }], ["child-a", { id: "child-a", parentID: "root-a" }],
    ["root-b", { id: "root-b" }], ["child-b", { id: "child-b", parentID: "root-a" }],
  ]);
  const api = {
    route: { current: { name: "session", params: { sessionID: "root-a" } } },
    state: { session: { get: (id) => sessions.get(id) } },
    client: {
      session: { get: async ({ sessionID }) => ({ data: sessions.get(sessionID) }), status: async () => ({ data: {} }) },
      permission: { list: async () => ({ data: [] }) }, question: { list: async () => ({ data: [] }) },
    },
    event: { on: (name, fn) => { events.set(name, fn); return () => {}; } },
    lifecycle: { onDispose: (fn) => { dispose = fn; } },
  };
  await fixture.module.default.tui(api);
  await settle();
  assert.equal(fixture.reports.findLast((r) => r.identity)?.identity, "root-a");
  api.route.current = { name: "session", params: { sessionID: "child-a" } };
  fixture.effects.forEach((fn) => fn());
  await settle();
  assert.equal(fixture.reports.findLast((r) => r.identity)?.identity, "root-a", "browsing child retains root");
  events.get("server.connected")();
  await settle();
  assert.equal(fixture.reports.findLast((r) => r.identity)?.identity, "root-a", "reconnect preserves root boundary");
  api.route.current = { name: "session", params: { sessionID: "root-b" } };
  fixture.effects.forEach((fn) => fn());
  await settle();
  assert.equal(fixture.reports.findLast((r) => r.identity)?.identity, "root-b");
  events.get("session.deleted")({ type: "session.deleted", properties: { info: { id: "root-b" } } });
  assert.equal(fixture.reports.at(-1).identity, null, "removed selected chat is cleared");
  await dispose();
  assert.equal(fixture.reports.at(-1).identity, undefined, "disposal does not erase a valid selection");
});

test("OpenCode TUI ignores stale async selection and isolates direct child attachment", async () => {
  const fixture = await hook("opencode-tui.js");
  let resolveOld;
  const events = new Map();
  const sessions = new Map([["child", { id: "child", parentID: "root" }], ["root", { id: "root" }]]);
  const api = {
    route: { current: { name: "session", params: { sessionID: "old" } } },
    state: { session: { get: (id) => sessions.get(id) } },
    client: {
      session: {
        get: ({ sessionID }) => sessionID === "old" ? new Promise((resolve) => { resolveOld = resolve; })
          : Promise.resolve({ data: sessions.get(sessionID) }),
        status: async () => ({ data: { sibling: { type: "busy" } } }),
      },
      permission: { list: async () => ({ data: [] }) }, question: { list: async () => ({ data: [] }) },
    },
    event: { on: (name, fn) => { events.set(name, fn); return () => {}; } },
    lifecycle: { onDispose() {} },
  };
  await fixture.module.default.tui(api);
  api.route.current = { name: "session", params: { sessionID: "child" } };
  fixture.effects.forEach((fn) => fn());
  await settle();
  resolveOld({ data: { id: "old" } });
  await settle();
  assert.equal(fixture.reports.findLast((r) => r.identity)?.identity, "child", "direct child owns only its subtree");
  assert.ok(!fixture.reports.some((r) => r.state === "working"), "sibling activity cannot mark a direct child working");
  assert.ok(!fixture.reports.some((r) => r.identity === "old"), "stale lookup cannot replace selection");
  api.route.current = { name: "home" };
  fixture.effects.forEach((fn) => fn());
  assert.equal(fixture.reports.at(-1).identity, null, "Home explicitly clears chat");
});

test("OpenCode Mini/local switches and child events cannot replace resume identity", async () => {
  const previousArgs = process.argv;
  process.argv = ["node", "opencode", "--mini"];
  try {
    const fixture = await hook("opencode.js", "restored");
    const handlers = await fixture.module.default.server();
    assert.equal(fixture.reports.at(-1).identity, "restored");
    await handlers.event({ event: { type: "session.created", properties: { info: { id: "child", parentID: "restored" } } } });
    await handlers["chat.message"]({ sessionID: "child" });
    assert.equal(fixture.reports.at(-1).identity, "restored");
    await handlers.event({ event: { type: "session.created", properties: { info: { id: "new-root" } } } });
    assert.equal(fixture.reports.at(-1).identity, "new-root");
    await handlers.event({ event: { type: "session.deleted", properties: { info: { id: "new-root" } } } });
    assert.equal(fixture.reports.at(-1).identity, null);
  } finally {
    process.argv = previousArgs;
  }
});

test("restored OpenCode launch identity survives startup Home until selection resolves", async () => {
  const fixture = await hook("opencode-tui.js", "root");
  const api = {
    route: { current: { name: "home" } },
    state: { session: { get: (id) => ({ id, ...(id === "child" ? { parentID: "root" } : {}) }) } },
    client: {
      session: { status: async () => ({ data: {} }) },
      permission: { list: async () => ({ data: [] }) },
      question: { list: async () => ({ data: [] }) },
    },
    event: { on: () => () => {} },
    lifecycle: { onDispose() {} },
  };
  await fixture.module.default.tui(api);
  assert.ok(!fixture.reports.some((r) => r.identity === null), "transient startup Home must preserve restore target");
  api.route.current = { name: "session", params: { sessionID: "child" } };
  fixture.effects.forEach((fn) => fn());
  await settle();
  assert.equal(fixture.reports.findLast((r) => r.identity)?.identity, "root");
});

test("removed restored chat is cleared before authoritative selection is available", async () => {
  const fixture = await hook("opencode-tui.js", "removed");
  const events = new Map();
  const api = {
    route: { current: { name: "home" } },
    event: { on: (name, fn) => { events.set(name, fn); return () => {}; } },
    lifecycle: { onDispose() {} },
  };
  await fixture.module.default.tui(api);
  events.get("session.deleted")({ type: "session.deleted", properties: { info: { id: "removed" } } });
  assert.equal(fixture.reports.at(-1).identity, null);
});

// API-shaped OpenCode TUI host; `events` collects the plugin's event subscriptions.
function tuiApi(events, route, sessions) {
  return {
    route: { current: route },
    state: { session: { get: (id) => sessions.get(id) } },
    client: {
      session: { get: async ({ sessionID }) => ({ data: sessions.get(sessionID) }), status: async () => ({ data: {} }) },
      permission: { list: async () => ({ data: [] }) }, question: { list: async () => ({ data: [] }) },
    },
    event: { on: (name, fn) => { events.set(name, fn); return () => {}; } },
    lifecycle: { onDispose() {} },
  };
}

test("plugin routes keep the selected root", async () => {
  const fixture = await hook("opencode-tui.js");
  const events = new Map();
  const sessions = new Map([["root", { id: "root" }], ["child", { id: "child", parentID: "root" }]]);
  const api = tuiApi(events, { name: "session", params: { sessionID: "root" } }, sessions);
  const child = { name: "session", params: { sessionID: "child" } };
  const go = async (route) => {
    api.route.current = route;
    fixture.effects.forEach((fn) => fn());
    await settle();
  };
  await fixture.module.default.tui(api);
  await settle();
  await go(child);
  // OpenCode routes plugin pages, such as the built-in /diff viewer, as { name: <plugin route id>, params }.
  await go({ name: "diff", params: { mode: "git", sessionID: "child", returnRoute: child } });
  assert.ok(!fixture.reports.some((r) => r.identity === null), "a plugin route is not Home");
  events.get("session.status")({ type: "session.status", properties: { sessionID: "child", status: { type: "busy" } } });
  assert.equal(fixture.reports.at(-1).state, "working", "live state still flows under a plugin route");
  await go(child);
  assert.ok(!fixture.reports.some((r) => r.identity === null), "returning is not Home either");
  assert.equal(fixture.reports.findLast((r) => r.identity)?.identity, "root", "returning keeps the root boundary");
});

test("OpenCode errors survive the idle published after them", async () => {
  const error = { name: "APIError", data: { message: "Quota exceeded" } };
  const stillBlocked = (fixture, since) => {
    const reports = fixture.reports.slice(since);
    assert.ok(reports.length > 0 && reports.every((r) => r.state === "blocked" && r.message === "Quota exceeded"),
      `idle replaced the error: ${JSON.stringify(reports)}`);
  };

  const tui = await hook("opencode-tui.js");
  const events = new Map();
  await tui.module.default.tui(tuiApi(events, { name: "session", params: { sessionID: "root" } },
    new Map([["root", { id: "root" }]])));
  await settle();
  const fire = (type, properties) => events.get(type)({ type, properties: { sessionID: "root", ...properties } });
  fire("session.status", { status: { type: "busy" } });
  const tuiSince = tui.reports.length;
  // OpenCode publishes session.error, then idle, as the failed turn ends.
  fire("session.error", { error });
  fire("session.status", { status: { type: "idle" } });
  fire("session.idle", {});
  stillBlocked(tui, tuiSince);
  fire("session.status", { status: { type: "busy" } });
  assert.equal(tui.reports.at(-1).state, "working");
  assert.equal(tui.reports.at(-1).message, undefined, "the next turn clears the error");

  const previousArgs = process.argv;
  process.argv = ["node", "opencode", "run", "hello"];
  try {
    const server = await hook("opencode.js");
    const handlers = await server.module.default.server();
    const send = (type, properties) => handlers.event({ event: { type, properties: { sessionID: "root", ...properties } } });
    await handlers.event({ event: { type: "session.created", properties: { info: { id: "root" } } } });
    await send("session.status", { status: { type: "busy" } });
    const serverSince = server.reports.length;
    await send("session.error", { error });
    await send("session.status", { status: { type: "idle" } });
    await send("session.idle", {});
    stillBlocked(server, serverSince);
    await send("session.status", { status: { type: "busy" } });
    assert.equal(server.reports.at(-1).state, "working");
    assert.equal(server.reports.at(-1).message, undefined, "the next turn clears the error");
  } finally {
    process.argv = previousArgs;
  }
});

test("OpenCode pins the pane owner for child shells", async () => {
  const keys = ["CRIT_ENV", "CRIT_SESSION", "CRIT_SOCKET", "CRIT_AGENT_PID"];
  const saved = Object.fromEntries(keys.map((key) => [key, process.env[key]]));
  const previousArgs = process.argv;
  const shellEnv = async (hooks) => {
    const output = { env: {} };
    await hooks["shell.env"]({ cwd: "/" }, output);
    return output.env;
  };
  try {
    process.env.CRIT_ENV = "1";
    // The TUI server worker snapshots process.env before the TUI plugin claims the pane.
    for (const argv of [["node", "opencode"], ["node", "opencode", "run", "hello"]]) {
      process.argv = argv;
      const fixture = await hook("opencode.js");
      assert.deepEqual(await shellEnv(await fixture.module.default.server()),
        { CRIT_AGENT_PID: String(process.pid) }, argv.join(" "));
    }
    // A nested run cannot claim the pane (no reporter), but its children must not either.
    Object.assign(process.env, {
      CRIT_SESSION: "5", CRIT_SOCKET: "/unused/crit.sock", CRIT_AGENT_PID: String(process.pid + 1),
    });
    const nested = (await import("./opencode.js")).default;
    assert.deepEqual(await shellEnv(await nested.server()), { CRIT_AGENT_PID: String(process.pid) });
    delete process.env.CRIT_ENV;
    assert.deepEqual(await shellEnv(await nested.server()), {}, "outside Crit nothing is injected");
  } finally {
    process.argv = previousArgs;
    for (const key of keys) {
      if (saved[key] === undefined) delete process.env[key];
      else process.env[key] = saved[key];
    }
  }
});
