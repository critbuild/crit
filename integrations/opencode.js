// Adapted from Herdr src/integration/assets/opencode/herdr-agent-state.js
// (Apache-2.0). Crit uses this only for local `run`/Mini; the full TUI owns its
// lifecycle through opencode-tui.js, including attached servers and selection.
import { createReporter } from "./reporter.js";

function ownsLocalLifecycle() {
  const args = process.argv.slice(2);
  const separator = args.indexOf("--");
  if (separator !== -1) args.splice(separator);
  if (args.some((arg) => arg === "--attach" || arg.startsWith("--attach="))) return false;
  while (args[0] === "--print-logs" || args[0] === "--log-level" || args[0]?.startsWith("--log-level=")) {
    args.splice(0, args[0] === "--log-level" ? 2 : 1);
  }
  return args[0] === "run" || (!['serve', 'web', 'attach'].includes(args[0]) && args.includes("--mini"));
}

let claimed = false;
// OpenCode's TUI copies process.env into its server Worker before the TUI plugin
// claims the pane, so tool, `!` shell and PTY children would inherit an empty
// owner. Pin it to this process so a nested `opencode run` or `omp` cannot claim.
const pinOwner = {
  "shell.env": async (_input, output) => {
    if (process.env.CRIT_ENV === "1") output.env.CRIT_AGENT_PID = String(process.pid);
  },
  // Crit's note on where scratch goes: its sandbox, not a RAM-backed /tmp. OMP takes it as
  // --append-system-prompt; OpenCode has no such flag, so every request's system prompt gets it.
  "experimental.chat.system.transform": async (_input, output) => {
    const note = process.env.CRIT_SANDBOX_NOTE;
    if (note && !output.system.includes(note)) output.system.push(note);
  },
};
async function server() {
  if (claimed || !ownsLocalLifecycle()) return pinOwner;
  const reporter = createReporter();
  if (!reporter) return pinOwner;
  claimed = true;
  let selected = process.env.CRIT_RESUME_ID || undefined;
  let state = "unknown";
  let error;
  const parents = new Map();
  const blockers = new Map();
  let epoch = 0;

  function root(id) {
    const seen = new Set();
    while (typeof id === "string" && !seen.has(id)) {
      if (id === selected) return id;
      seen.add(id);
      id = parents.get(id);
    }
  }
  function publish() {
    const generation = epoch;
    const request = selected && [...blockers.values()].find((request) => root(request.sessionID) === selected);
    return reporter.report(request ? "blocked" : state,
      request ? request.questions?.[0]?.question || request.permission || "Waiting for user input" : error,
      () => generation === epoch, selected ?? null);
  }
  await publish();
  return {
    ...pinOwner,
    "chat.message": async ({ sessionID }) => {
      if (!sessionID || parents.has(sessionID)) return;
      if (selected !== sessionID) { epoch += 1; blockers.clear(); }
      selected = sessionID;
      state = "working";
      error = undefined;
      await publish();
    },
    event: async ({ event }) => {
      const data = event?.properties ?? {};
      if (data.info?.id && data.info.parentID) parents.set(data.info.id, data.info.parentID);
      const id = data.sessionID ?? data.info?.id;
      if (event.type === "session.created" && data.info?.id && !data.info.parentID) {
        epoch += 1;
        selected = data.info.id;
        blockers.clear();
        state = "idle";
        error = undefined;
        await publish();
        return;
      }
      if (!selected || root(id) !== selected) return;
      switch (event.type) {
        case "permission.asked":
        case "question.asked":
          if (typeof data.id === "string") blockers.set(`${event.type.split(".")[0]}:${data.id}`, data);
          break;
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
          blockers.delete(`${event.type.split(".")[0]}:${data.requestID}`);
          break;
        case "session.status":
          if (id !== selected) return;
          if (data.status?.type === "busy" || data.status?.type === "retry") {
            state = "working";
            error = undefined;
          } else if (data.status?.type === "idle") {
            // OpenCode publishes idle right after session.error; keep the error until the next turn.
            if (error) return;
            state = "idle";
          } else return;
          break;
        case "session.idle":
          if (id !== selected || error) return;
          state = "idle";
          break;
        case "session.error":
          if (id !== selected || data.error?.name === "MessageAbortedError") return;
          state = "blocked";
          error = data.error?.data?.message || "Agent error";
          break;
        case "session.deleted":
          for (const [key, request] of blockers) if (request.sessionID === id) blockers.delete(key);
          parents.delete(id);
          if (id === selected) {
            epoch += 1;
            selected = undefined;
            state = "unknown";
            error = undefined;
          }
          break;
        default:
          return;
      }
      await publish();
    },
  };
}

export default { id: "crit.opencode.server", server };
