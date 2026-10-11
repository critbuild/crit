// Transport/attribution regressions only; these are not agent lifecycle smoke tests.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import net from "node:net";
import test from "node:test";
import { createReporter } from "./reporter.js";

const keys = ["CRIT_ENV", "CRIT_SESSION", "CRIT_SOCKET", "CRIT_AGENT_PID"];
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
    const reporter = createReporter();
    assert.ok(reporter);
    await reporter.report("working", undefined, () => false);
    await reporter.report("blocked", "Root permission");
    await reporter.report("blocked", "Root permission");
    assert.equal(frames.length, 1);
    assert.match(frames[0], /^\{"session":18446744073709551615,"state":"blocked"/);
    assert.equal(JSON.parse(frames[0]).message, "Root permission");
  } finally {
    await new Promise((resolve) => listener.close(resolve));
    await rm(directory, { recursive: true, force: true });
  }
}));
