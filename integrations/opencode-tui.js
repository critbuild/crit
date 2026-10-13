// Adapted from Herdr src/integration/assets/opencode/herdr-tui-session.js
// (Apache-2.0), specifically its full-TUI lifecycle implementation.
// Crit changes: current OpenCode TUI API only, Solid route subscriptions instead
// of polling, launch-scoped transport, explicit Unknown and exact chat identity.
import { createEffect, createRoot, on, untrack } from "solid-js";
import { createReporter } from "./reporter.js";

export default { id: "crit.opencode.tui", tui };

async function tui(api) {
  const reporter = createReporter();
  if (!reporter) return;
  let disposed = false;
  let context;
  const initial = process.env.CRIT_RESUME_ID;
  let observedSelection = false;
  let selectedBoundary = initial;

  function routeID() {
    const route = api.route.current;
    if (route?.name === "session") return route.params?.sessionID;
    // Plugin routes (the built-in /diff viewer) overlay the current chat; only Home clears it.
    return route?.name === "home" ? undefined : context?.route;
  }
  function current(ctx) {
    return !disposed && context === ctx && routeID() === ctx.route;
  }
  async function read(ctx, request) {
    const result = await request({
      signal: AbortSignal.any([ctx.controller.signal, AbortSignal.timeout(5000)]),
      throwOnError: true,
    });
    if (!current(ctx) || result?.data === undefined) throw new Error("Session data unavailable");
    return result.data;
  }

  // Browsing a descendant retains the originally selected root. A pane attached
  // directly to a child owns only that child's subtree, never its siblings.
  function root(ctx, id) {
    const seen = new Set();
    while (typeof id === "string" && !seen.has(id) && !ctx.deleted.has(id)) {
      seen.add(id);
      if (id === ctx.boundary) return id;
      const session = ctx.sessions.get(id) ?? api.state.session.get(id);
      if (!session) return;
      if (!session.parentID) return id;
      id = session.parentID;
    }
  }
  async function resolveRoot(ctx, id) {
    const seen = new Set();
    while (typeof id === "string" && !seen.has(id) && !ctx.deleted.has(id)) {
      seen.add(id);
      if (id === ctx.boundary) return id;
      let session = ctx.sessions.get(id) ?? api.state.session.get(id);
      if (!session) {
        if (!ctx.lookups.has(id)) {
          const requested = id;
          const lookup = read(ctx, (options) => api.client.session.get({ sessionID: requested }, options));
          ctx.lookups.set(id, lookup);
          lookup.finally(() => ctx.lookups.delete(requested)).catch(() => {});
        }
        session = await ctx.lookups.get(id);
        if (ctx.deleted.has(id)) return;
        session = ctx.sessions.get(id) ?? session;
        if (session.id !== id) throw new Error("Unexpected session identity");
        ctx.sessions.set(id, session);
      }
      if (!session.parentID) return id;
      id = session.parentID;
    }
  }
  function owners(ctx) {
    return new Set([...ctx.statuses.keys(), ...[...ctx.blockers.values()].map((r) => r.sessionID)]);
  }
  function state(ctx) {
    for (const request of ctx.blockers.values()) {
      if (root(ctx, request.sessionID) === ctx.selected) {
        return ["blocked", request.questions?.[0]?.question || request.permission || "Waiting for user input"];
      }
    }
    if (ctx.errors.has(ctx.selected)) return ["blocked", ctx.errors.get(ctx.selected)];
    for (const id of ctx.statuses.keys()) {
      if (root(ctx, id) === ctx.selected) return ["working"];
    }
    if (ctx.hydrated && [...owners(ctx)].every((id) => root(ctx, id) !== undefined)) return ["idle"];
    return ["unknown"];
  }
  function report(ctx) {
    if (!current(ctx) || !ctx.selected) return;
    const selected = ctx.selected;
    const [value, message] = state(ctx);
    void reporter.report(value, message,
      () => current(ctx) && ctx.selected === selected && root(ctx, ctx.route) === selected, selected);
  }
  function reconcile(ctx) {
    if (!current(ctx)) return;
    report(ctx);
    if (ctx.resolving) { ctx.resolveAgain = true; return; }
    ctx.resolving = true;
    void (async () => {
      const ancestor = await resolveRoot(ctx, ctx.route);
      if (!current(ctx) || !ancestor) return;
      const selected = ancestor === ctx.boundary ? ctx.boundary : ctx.route;
      if (selected !== ctx.selected) {
        ctx.boundary = selected;
        ctx.selected = selected;
        selectedBoundary = selected;
      }
      // Identity does not depend on all descendant lifecycle lookups succeeding.
      report(ctx);
      await Promise.all([...owners(ctx)].map((id) => resolveRoot(ctx, id)));
      if (current(ctx)) report(ctx);
    })().catch(() => {}).finally(() => {
      ctx.resolving = false;
      if (ctx.resolveAgain) { ctx.resolveAgain = false; reconcile(ctx); }
    });
  }
  function clearRequests(ctx, matches) {
    for (const [key, request] of ctx.blockers) if (matches(request)) ctx.blockers.delete(key);
  }
  function terminalTool(part) {
    return part?.type === "tool" && ["completed", "error"].includes(part.state?.status);
  }
  function apply(ctx, event) {
    const data = event.properties;
    if (!data) return;
    if (event.type === "message.part.updated") {
      const part = data.part;
      if (terminalTool(part)) {
        clearRequests(ctx, (r) => r.sessionID === part.sessionID &&
          r.tool?.messageID === part.messageID && r.tool?.callID === part.callID);
      }
      return;
    }
    const id = data.sessionID ?? data.info?.id;
    if (typeof id !== "string" || ctx.deleted.has(id)) return;
    switch (event.type) {
      case "session.created":
      case "session.updated":
        if (data.info) ctx.sessions.set(id, data.info);
        break;
      case "session.deleted":
        ctx.deleted.add(id);
        ctx.sessions.delete(id);
        ctx.statuses.delete(id);
        ctx.errors.delete(id);
        clearRequests(ctx, (r) => r.sessionID === id);
        if (ctx.selected === id || ctx.boundary === id || (!ctx.selected && ctx.route === id)) {
          ctx.controller.abort();
          context = undefined;
          selectedBoundary = undefined;
          void reporter.report("unknown", undefined, () => !disposed && context === undefined, null);
        }
        break;
      case "session.status":
      case "session.idle": {
        const status = event.type === "session.idle" ? "idle" : data.status?.type;
        if (status === "busy" || status === "retry") {
          ctx.statuses.set(id, status);
          ctx.errors.delete(id);
        } else if (status === "idle") {
          ctx.statuses.delete(id);
        }
        break;
      }
      case "session.error":
        if (data.error?.name !== "MessageAbortedError") ctx.errors.set(id, data.error?.data?.message || "Agent error");
        break;
      case "permission.asked":
      case "question.asked":
        if (typeof data.id === "string") ctx.blockers.set(`${event.type.split(".")[0]}:${data.id}`, data);
        break;
      case "permission.replied":
      case "question.replied":
      case "question.rejected":
        ctx.blockers.delete(`${event.type.split(".")[0]}:${data.requestID}`);
        break;
    }
  }
  async function hydrate(ctx) {
    if (!current(ctx) || ctx.loading) return;
    ctx.loading = true;
    try {
      const [statuses, permissions, questions] = await Promise.all([
        read(ctx, (options) => api.client.session.status(undefined, options)),
        read(ctx, (options) => api.client.permission.list(undefined, options)),
        read(ctx, (options) => api.client.question.list(undefined, options)),
      ]);
      if (!statuses || typeof statuses !== "object" || Array.isArray(statuses) ||
          !Object.values(statuses).every((s) => ["busy", "retry", "idle"].includes(s?.type)) ||
          !Array.isArray(permissions) || !Array.isArray(questions)) throw new Error("Incomplete session snapshot");
      const messages = new Map();
      const blockers = new Map();
      let validated = true;
      await Promise.all([["permission", permissions], ["question", questions]].flatMap(([kind, requests]) =>
        requests.map(async (request) => {
          if (ctx.deleted.has(request.sessionID)) return;
          if (request.tool) {
            const { messageID, callID } = request.tool;
            const key = `${request.sessionID}:${messageID}`;
            if (!messages.has(key)) messages.set(key, read(ctx, (options) => api.client.session.message({
              sessionID: request.sessionID, messageID,
            }, options)));
            try {
              const message = await messages.get(key);
              if (terminalTool(message.parts?.find((p) => p.type === "tool" && p.callID === callID))) return;
            } catch { validated = false; }
          }
          if (!ctx.deleted.has(request.sessionID)) blockers.set(`${kind}:${request.id}`, request);
        })));
      if (!current(ctx)) return;
      ctx.statuses = new Map(Object.entries(statuses)
        .filter(([id, status]) => !ctx.deleted.has(id) && ["busy", "retry"].includes(status.type)));
      ctx.blockers = blockers;
      // Replay events received during the snapshot, so a late response cannot
      // resurrect a permission that the user has already answered.
      for (const event of ctx.events) apply(ctx, event);
      ctx.hydrated = validated;
      reconcile(ctx);
    } catch {
      // Unknown is deliberate: unavailable lifecycle evidence is not Idle.
    } finally {
      ctx.loading = false;
      if (ctx.hydrated) ctx.events = [];
    }
  }
  function syncSelection(reset = false) {
    if (disposed) return;
    const id = routeID();
    if (!reset && id === context?.route) return;
    const boundary = selectedBoundary;
    context?.controller.abort();
    context = undefined;
    if (typeof id !== "string" || !id) {
      if (observedSelection || !initial) selectedBoundary = undefined;
      // Startup can briefly be Home before --session's route is installed.
      // Once a chat was observed, Home is an authoritative new-chat selection.
      void reporter.report("unknown", undefined,
        () => !disposed && routeID() === id, observedSelection || !initial ? null : undefined);
      return;
    }
    observedSelection = true;
    void reporter.report("unknown");
    const ctx = {
      route: id, boundary, controller: new AbortController(), sessions: new Map(), lookups: new Map(),
      statuses: new Map(), blockers: new Map(), errors: new Map(), deleted: new Set(),
      events: [], hydrated: false, loading: false, resolving: false,
    };
    context = ctx;
    reconcile(ctx);
    void hydrate(ctx);
  }
  const subscriptions = [
    "session.created", "session.updated", "session.deleted", "session.status", "session.idle", "session.error",
    "permission.asked", "permission.replied", "question.asked", "question.replied", "question.rejected",
    "message.part.updated",
  ].map((type) => api.event.on(type, (event) => {
    if (type === "message.part.updated" && !terminalTool(event.properties?.part)) return;
    syncSelection();
    const ctx = context;
    if (!ctx && type === "session.deleted" && selectedBoundary &&
        (event.properties?.sessionID ?? event.properties?.info?.id) === selectedBoundary) {
      selectedBoundary = undefined;
      observedSelection = true;
      void reporter.report("unknown", undefined, () => !disposed && !context, null);
    }
    if (!ctx) return;
    if (ctx.loading || !ctx.hydrated) {
      // Bound reconnect-time deltas; restart a snapshot rather than grow forever.
      if (ctx.events.length >= 4096) { syncSelection(true); return; }
      ctx.events.push(event);
    }
    apply(ctx, event);
    reconcile(ctx);
    if (!ctx.hydrated && !ctx.loading) void hydrate(ctx);
  }));
  for (const type of ["server.connected", "server.instance.disposed", "global.disposed"]) {
    subscriptions.push(api.event.on(type, () => syncSelection(true)));
  }
  // OpenCode's plugin loader shares its own Solid runtime, so this tracks the
  // route getter without a timer or dependency installed into the user's config.
  const disposeRoute = createRoot((dispose) => {
    createEffect(on(routeID, () => untrack(() => syncSelection())));
    return dispose;
  });
  api.lifecycle.onDispose(async () => {
    disposed = true;
    context?.controller.abort();
    disposeRoute();
    for (const unsubscribe of subscriptions) unsubscribe();
    await reporter.report("unknown");
  });
  syncSelection();
}
