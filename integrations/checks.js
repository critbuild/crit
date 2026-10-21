// Launch-scoped native-tool client. The coordinator owns FIFO, liveness and cleanup.
import { spawn } from "node:child_process";
import { isAbsolute } from "node:path";

export const CHECK_INSTRUCTIONS = `Shared checking: keep coding worktrees independent. Before shared Compose, service or browser checks, acquire crit_check (action acquire); it waits FIFO and returns the owner's service endpoints. Use those endpoints, and run host check processes with "$CRIT_BIN" check exec -- PROGRAM ARGS. Release crit_check in finally, including failures/aborts. Never start an independent Docker/Compose stack. Coding alone needs no check turn. Manual/UI-pinned turns are not released by agent lifecycle cleanup.`;
export const CHECK_DESCRIPTION = "Acquire the project's exclusive shared checking environment (FIFO, cancellable), release this agent's automatic turn, or inspect status and service endpoints. Actions: acquire, release, status.";
const LIMIT = 32 * 1024;

export function createCheckClient(env = process.env) {
  if (env.CRIT_ENV !== "1" || !isAbsolute(env.CRIT_BIN || "") || !env.CRIT_WINDOW_PID || !env.CRIT_SESSION ||
      env.CRIT_AGENT_PID !== String(process.pid)) return;
  // Snapshot launch identity; unrelated nested harnesses cannot retarget this client.
  const ownerEnv = { ...env };
  const pending = new Set();
  let finishing;

  function run(action, { cwd, signal, progress } = {}) {
    if (signal?.aborted) return Promise.reject(signal.reason ?? new Error("Check cancelled"));
    const args = ["check", action];
    if (action === "release") args.push("--automatic");
    if (cwd) args.push("--repo", cwd);
    return new Promise((resolve, reject) => {
      let output = "";
      let errors = "";
      let killTimer;
      const child = spawn(ownerEnv.CRIT_BIN, args, { env: ownerEnv, stdio: ["ignore", "pipe", "pipe"] });
      const abort = () => {
        child.kill("SIGTERM");
        killTimer = setTimeout(() => child.kill("SIGKILL"), 1000);
        killTimer.unref?.();
      };
      signal?.addEventListener("abort", abort, { once: true });
      if (signal?.aborted) abort();
      child.stdout.on("data", (data) => { output = (output + data.toString()).slice(-LIMIT); });
      child.stderr.on("data", (data) => {
        const text = data.toString();
        errors = (errors + text).slice(-LIMIT);
        progress?.(text.slice(-2048));
      });
      const clean = () => { clearTimeout(killTimer); signal?.removeEventListener("abort", abort); };
      child.on("error", (error) => { clean(); reject(error); });
      child.on("close", (code, killed) => {
        clean();
        if (signal?.aborted) reject(signal.reason ?? new Error("Check cancelled"));
        else if (code !== 0) reject(new Error(errors.trim() || `crit check ${action} exited ${code ?? killed}`));
        else resolve(output.trim() || errors.trim() || `Check ${action} complete`);
      });
    });
  }

  async function execute(action, options = {}) {
    if (!["acquire", "release", "status"].includes(action)) throw new Error("Unknown crit_check action");
    if (action === "release") return finish(options.cwd);
    if (finishing) await finishing;
    if (action !== "acquire") return run(action, options);
    const controller = new AbortController();
    const abort = () => controller.abort(options.signal.reason);
    options.signal?.addEventListener("abort", abort, { once: true });
    if (options.signal?.aborted) abort();
    const entry = { controller, done: undefined };
    pending.add(entry);
    entry.done = run(action, { ...options, signal: controller.signal }).catch(async (error) => {
      // A grant may race client cancellation; release only automatic ownership.
      await run("release", { cwd: options.cwd });
      throw error;
    });
    try {
      return await entry.done;
    } finally {
      pending.delete(entry);
      options.signal?.removeEventListener("abort", abort);
    }
  }

  function finish(cwd) {
    if (finishing) return finishing;
    for (const entry of pending) entry.controller.abort(new Error("Agent turn ended"));
    finishing = (async () => {
      await Promise.allSettled([...pending].map((entry) => entry.done));
      return run("release", { cwd });
    })().finally(() => { finishing = undefined; });
    return finishing;
  }
  return { execute, finish };
}
