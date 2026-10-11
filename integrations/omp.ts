// Adapted from Herdr src/integration/assets/omp/herdr-agent-state.ts (Apache-2.0).
// Crit changes: launch-only identity, compact socket protocol, root isolation,
// explicit Unknown/Exited, no session discovery or compatibility hooks.
// @ts-nocheck
import { createReporter } from "./reporter.js";

export default function (pi) {
  // OMP marks shells it spawns with OMPCODE=1; a nested OMP is not this pane.
  if (process.env.OMPCODE === "1") return;
  const reporter = createReporter();
  if (!reporter) return;

  let active;
  let blocked = 0;
  let blockedMessage;
  let failure;
  let idleTimer;
  let rootSession;

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

  function publish() {
    if (blocked > 0) return reporter.report("blocked", blockedMessage);
    if (failure) return reporter.report("blocked", failure);
    return reporter.report(active === true ? "working" : active === false ? "idle" : "unknown");
  }

  function initialize(_event, ctx) {
    if (!root(ctx, true)) return;
    cancelIdle();
    blocked = 0;
    blockedMessage = undefined;
    failure = undefined;
    const idle = ctx.isIdle?.();
    active = typeof idle === "boolean" ? !idle : undefined;
    // OMP awaits lifecycle handlers. Deliver before its synchronous UI startup
    // can stall the event loop beyond the socket's bounded handshake timeout.
    return publish();
  }

  pi.on("session_start", initialize);
  pi.on("session_switch", initialize);
  pi.on("agent_start", (_event, ctx) => {
    if (!root(ctx)) return;
    cancelIdle();
    failure = undefined;
    active = true;
    void publish();
  });
  pi.on("tool_approval_requested", (event, ctx) => {
    if (!root(ctx)) return;
    cancelIdle();
    blocked += 1;
    blockedMessage = event.reason || `${event.toolName || "Tool"} approval`;
    void publish();
  });
  pi.on("tool_approval_resolved", (_event, ctx) => {
    if (!root(ctx)) return;
    blocked = Math.max(0, blocked - 1);
    if (!blocked) blockedMessage = undefined;
    void publish();
  });
  pi.on("tool_execution_start", (event, ctx) => {
    if (event.toolName !== "ask" || !root(ctx)) return;
    cancelIdle();
    blocked += 1;
    blockedMessage = event.args?.questions?.find((q) => typeof q?.question === "string")?.question || "Waiting for user input";
    void publish();
  });
  pi.on("tool_execution_end", (event, ctx) => {
    if (event.toolName !== "ask" || !root(ctx)) return;
    blocked = Math.max(0, blocked - 1);
    if (!blocked) blockedMessage = undefined;
    void publish();
  });
  pi.on("agent_end", (event, ctx) => {
    if (!root(ctx) || active !== true || event.willContinue === true) return;
    cancelIdle();
    active = false;
    const assistant = Array.isArray(event.messages)
      ? event.messages.findLast((message) => message?.role === "assistant") : undefined;
    if (assistant?.stopReason === "error") {
      failure = assistant.errorMessage || "Agent error";
      void publish();
      return;
    }
    idleTimer = setTimeout(() => { idleTimer = undefined; void publish(); }, 250);
    idleTimer.unref?.();
  });
  pi.on("session_shutdown", async (_event, ctx) => {
    if (!root(ctx)) return;
    cancelIdle();
    await reporter.report("exited");
  });
}
