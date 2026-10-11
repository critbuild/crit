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
        client.write(`{"session":${session},"state":${JSON.stringify(next.state)},"message":${JSON.stringify(next.message)}}\n`);
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
    while (pending) {
      const next = pending;
      pending = undefined;
      if (!next.current()) continue;
      if (previous?.state === next.state && previous?.message === next.message) continue;
      previous = await send(next) ? next : undefined;
    }
  }

  function report(state, message, current = () => true) {
    pending = { state, message: typeof message === "string" ? message.slice(0, 512) : null, current };
    if (!draining) {
      draining = drain().finally(() => {
        draining = undefined;
        if (pending) {
          const next = pending;
          void report(next.state, next.message, next.current);
        }
      });
    }
    return draining;
  }
  return { report };
}
