// Transport/attribution regressions only; these are not agent lifecycle smoke tests.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import net from "node:net";
import test from "node:test";
import { createReporter } from "./reporter.js";

const keys = ["CRIT_ENV", "CRIT_SESSION", "CRIT_SOCKET", "CRIT_AGENT_PID", "CRIT_RESUME_ID"];
async function isolated(run) {
  const before = Object.fromEntries(keys.map((key) => [key, process.env[key]]));
  try { await run(); }
  finally {
    for (const key of keys) {
      if (before[key] === undefined) delete process.env[key];
      else process.env[key] = before[key];
    }
  }
}

test("only a launch-scoped root process can claim a pane", () => isolated(async () => {
  process.env.CRIT_ENV = "1";
  process.env.CRIT_SESSION = "5";
  process.env.CRIT_SOCKET = "/unused/crit.sock";
  process.env.CRIT_AGENT_PID = String(process.pid + 1);
  assert.equal(createReporter(), undefined);
  process.env.CRIT_AGENT_PID = "";
  delete process.env.CRIT_ENV;
  assert.equal(createReporter(), undefined);
  process.env.CRIT_ENV = "1";
  process.env.CRIT_SESSION = "18446744073709551616";
  assert.equal(createReporter(), undefined);
}));

test("stale selection work is discarded and full u64 identity survives transport", () => isolated(async () => {
  const directory = await mkdtemp(path.join(tmpdir(), "crit-attribution-"));
  const socket = path.join(directory, "agent.sock");
  const frames = [];
  const listener = net.createServer((stream) => {
    let frame = "";
    stream.on("data", (data) => {
      frame += data.toString();
      if (!frame.endsWith("\n")) return;
      frames.push(frame);
      stream.end("\n");
    });
  });
  try {
    await new Promise((resolve, reject) => {
      listener.once("error", reject);
      listener.listen(socket, resolve);
    });
    process.env.CRIT_ENV = "1";
    process.env.CRIT_SESSION = "18446744073709551615";
    process.env.CRIT_SOCKET = socket;
    process.env.CRIT_AGENT_PID = "";
    process.env.CRIT_RESUME_ID = "chat-restored";
    const reporter = createReporter();
    assert.ok(reporter);
    await reporter.report("working", undefined, () => false);
    await reporter.report("blocked", "Root permission");
    await reporter.report("blocked", "Root permission");
    assert.equal(frames.length, 1);
    assert.match(frames[0], /^\{"session":18446744073709551615,"state":"blocked"/);
    assert.equal(JSON.parse(frames[0]).message, "Root permission");
    assert.deepEqual(JSON.parse(frames[0]).resume, { status: "selected", identity: "chat-restored" });
    await reporter.report("blocked", "Root permission", () => true, "chat-second");
    assert.equal(frames.length, 2, "identity-only change must not be deduplicated");
    assert.equal(JSON.parse(frames[1]).resume.identity, "chat-second");
    await reporter.report("blocked", "Root permission", () => false, "chat-stale");
    let stillCurrent = true;
    const stale = reporter.report("working", undefined, () => stillCurrent, "chat-stale-on-connect");
    stillCurrent = false;
    await stale;
    await reporter.report("unknown");
    assert.equal(JSON.parse(frames.at(-1)).resume.identity, "chat-second", "stale work cannot replace identity");
    await reporter.report("unknown", undefined, () => true, null);
    assert.deepEqual(JSON.parse(frames.at(-1)).resume, { status: "clear" });
    await reporter.report("exited");
    assert.deepEqual(JSON.parse(frames.at(-1)).resume, { status: "clear" }, "teardown cannot resurrect a chat");
  } finally {
    await new Promise((resolve) => listener.close(resolve));
    await rm(directory, { recursive: true, force: true });
  }
}));
