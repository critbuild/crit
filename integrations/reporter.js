// Crit's launch-scoped transport. No subprocesses, screen inspection, or polling.
import net from "node:net";

export function createReporter() {
  const socketPath = process.env.CRIT_SOCKET;
  const session = process.env.CRIT_SESSION;
  const owner = process.env.CRIT_AGENT_PID;
  if (process.env.CRIT_ENV !== "1" || !socketPath || !/^(0|[1-9][0-9]*)$/.test(session ?? "") ||
      BigInt(session) > 18446744073709551615n || (owner && owner !== String(process.pid))) return;
  // Children inherit this marker. Another process must never claim this pane.
  process.env.CRIT_AGENT_PID = String(process.pid);
  let pending;
  let draining;
  let previous;
  let resume = process.env.CRIT_RESUME_ID
    ? { status: "selected", identity: process.env.CRIT_RESUME_ID } : { status: "unknown" };

  function send(next) {
    return new Promise((resolve) => {
      let done = false;
      let timer;
      const finish = (delivered) => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        client.destroy();
        resolve(delivered);
      };
      const client = net.createConnection(socketPath);
      client.on("connect", () => {
        if (!next.current()) return finish(false);
        // Keep Rust's entire u64 range intact rather than rounding through Number.
        client.write(`{"session":${session},"state":${JSON.stringify(next.state)},"message":${JSON.stringify(next.message)},"resume":${JSON.stringify(next.resume)}}\n`);
      });
      client.on("data", (data) => finish(data.includes(10)));
      client.on("error", () => finish(false));
      client.on("end", () => finish(false));
      client.on("close", () => finish(false));
      timer = setTimeout(() => finish(false), 500);
      timer.unref?.();
      client.unref();
    });
  }

  async function drain() {
    let failures = 0;
    while (pending) {
      const next = pending;
      pending = undefined;
      if (!next.current()) continue;
      next.resume = next.identity === undefined ? resume
        : typeof next.identity === "string" && next.identity
          ? { status: "selected", identity: next.identity } : { status: "clear" };
      if (previous?.state === next.state && previous?.message === next.message &&
          previous?.resume.status === next.resume.status && previous?.resume.identity === next.resume.identity) continue;
      if (await send(next)) {
        previous = next;
        resume = next.resume;
        failures = 0;
      } else {
        previous = undefined;
        // A host stall can outlast Crit's handshake deadline; retry the latest state briefly.
        if (!pending && next.current() && ++failures <= 3) {
          pending = next;
          await new Promise((resolve) => setTimeout(resolve, 100 * failures).unref?.());
        }
      }
    }
  }

  // Undefined means unavailable evidence, null means an explicit no-chat selection.
  // Keep the guard third for existing asynchronous lifecycle callers.
  function report(state, message, current = () => true, identity) {
    if (!current()) return Promise.resolve();
    // Crit's strict JSON parser rejects lone surrogates, so never cut a pair in half.
    let text = typeof message === "string" ? message.toWellFormed() : null;
    if (text?.length > 512) text = text.slice(0, /[\uD800-\uDBFF]/.test(text[511]) ? 511 : 512);
    pending = { state, message: text, current, identity };
    if (!draining) {
      draining = drain().finally(() => {
        draining = undefined;
        if (pending) {
          const next = pending;
          void report(next.state, next.message, next.current, next.identity);
        }
      });
    }
    return draining;
  }
  return { report };
}
