// Adapted from Herdr src/integration/assets/omp/herdr-agent-state.ts (Apache-2.0).
// Crit changes: launch-only identity, compact socket protocol, root isolation,
// explicit Unknown/Exited, no session discovery or compatibility hooks.
// @ts-nocheck
import { createReporter } from "./reporter.js";
import { createCheckClient, CHECK_INSTRUCTIONS, CHECK_DESCRIPTION } from "./checks.js";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

export default function (pi) {
  // OMP marks shells it spawns with OMPCODE=1; a nested OMP is not this pane.
  if (process.env.OMPCODE === "1") return;
  const reporter = createReporter();
  if (!reporter) return;
  const checks = createCheckClient();
  if (checks) {
    pi.registerTool({
      name: "crit_check",
      label: "Crit check",
      loadMode: "essential",
      description: CHECK_DESCRIPTION,
      parameters: pi.zod.object({ action: pi.zod.enum(["acquire", "release", "status"]) }),
      async execute(_id, { action }, signal, onUpdate, ctx) {
        if (!root(ctx)) throw new Error("crit_check belongs to the pane's root session");
        const text = await checks.execute(action, {
          cwd: ctx.cwd, signal,
          progress: (text) => onUpdate?.({ content: [{ type: "text", text }], details: {} }),
        });
        return { content: [{ type: "text", text }], details: {} };
      },
    });
    pi.on("before_agent_start", (event, ctx) => {
      if (!root(ctx)) return;
      return { systemPrompt: [...event.systemPrompt, CHECK_INSTRUCTIONS] };
    });
  }

  let active;
  let blocked = 0;
  let blockedMessage;
  let failure;
  let idleTimer;
  let rootSession;
  let epoch = 0;
  let durableSession;

  function root(ctx, select = false) {
    if (ctx?.hasUI !== true) return false;
    const id = ctx.sessionManager?.getSessionId?.();
    if (select || rootSession === undefined) rootSession = id;
    return id === rootSession;
  }

  function cancelIdle() {
    clearTimeout(idleTimer);
    idleTimer = undefined;
  }

  function publish(ctx) {
    const generation = epoch;
    const current = () => generation === epoch && root(ctx);
    // Session paths are allocated before OMP's lazy writer materializes them.
    // A new unsaved chat must clear the previous chat, not resume it later.
    const file = ctx.sessionManager?.getSessionFile?.();
    let identity;
    if (typeof ctx.sessionManager?.getSessionFile === "function") {
      if (typeof file === "string" && file && existsSync(file)) {
        identity = resolve(file);
        durableSession = rootSession;
      } else if (!file || durableSession !== rootSession) {
        identity = null;
      }
      // A previously durable chat can be briefly unavailable while relocated.
      // Unknown evidence preserves it; only a new selection clears old history.
    }
    if (blocked > 0) return reporter.report("blocked", blockedMessage, current, identity);
    if (failure) return reporter.report("blocked", failure, current, identity);
    return reporter.report(active === true ? "working" : active === false ? "idle" : "unknown", undefined, current, identity);
  }

  async function initialize(_event, ctx) {
    if (!root(ctx, true)) return;
    await checks?.finish(ctx.cwd);
    epoch += 1;
    cancelIdle();
    blocked = 0;
    blockedMessage = undefined;
    failure = undefined;
    const idle = ctx.isIdle?.();
    active = typeof idle === "boolean" ? !idle : undefined;
    // OMP awaits lifecycle handlers. Deliver before its synchronous UI startup
    // can stall the event loop beyond the socket's bounded handshake timeout.
    return publish(ctx);
  }

  pi.on("session_start", initialize);
  pi.on("session_switch", initialize);
  pi.on("session_branch", initialize);
  pi.on("agent_start", (_event, ctx) => {
    if (!root(ctx)) return;
    cancelIdle();
    failure = undefined;
    active = true;
    void publish(ctx);
  });
  pi.on("tool_approval_requested", (event, ctx) => {
    if (!root(ctx)) return;
    cancelIdle();
    blocked += 1;
    blockedMessage = event.reason || `${event.toolName || "Tool"} approval`;
    void publish(ctx);
  });
  pi.on("tool_approval_resolved", (_event, ctx) => {
    if (!root(ctx)) return;
    blocked = Math.max(0, blocked - 1);
    if (!blocked) blockedMessage = undefined;
    void publish(ctx);
  });
  pi.on("tool_execution_start", (event, ctx) => {
    if (event.toolName !== "ask" || !root(ctx)) return;
    cancelIdle();
    blocked += 1;
    blockedMessage = event.args?.questions?.find((q) => typeof q?.question === "string")?.question || "Waiting for user input";
    void publish(ctx);
  });
  pi.on("tool_execution_end", (event, ctx) => {
    if (event.toolName !== "ask" || !root(ctx)) return;
    blocked = Math.max(0, blocked - 1);
    if (!blocked) blockedMessage = undefined;
    void publish(ctx);
  });
  pi.on("agent_end", async (event, ctx) => {
    if (!root(ctx)) return;
    if (event.willContinue !== true) await checks?.finish(ctx.cwd);
    if (active !== true || event.willContinue === true) return publish(ctx);
    cancelIdle();
    active = false;
    const assistant = Array.isArray(event.messages)
      ? event.messages.findLast((message) => message?.role === "assistant") : undefined;
    if (assistant?.stopReason === "error") {
      failure = assistant.errorMessage || "Agent error";
      void publish(ctx);
      return;
    }
    idleTimer = setTimeout(() => { idleTimer = undefined; void publish(ctx); }, 250);
    idleTimer.unref?.();
  });
  pi.on("session_shutdown", async (_event, ctx) => {
    if (!root(ctx)) return;
    cancelIdle();
    await checks?.finish(ctx.cwd);
    // Preserve a valid identity during teardown, but capture a draft/session
    // that became durable since the last normal lifecycle event.
    const file = ctx.sessionManager?.getSessionFile?.();
    await reporter.report("exited", undefined, () => true,
      typeof file === "string" && file && existsSync(file) ? resolve(file) : undefined);
  });
}
