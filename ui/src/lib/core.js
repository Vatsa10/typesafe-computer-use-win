// A thin wrapper over the shell's bridge (`window.core`, see preload.js). It speaks only the
// methods listed in protocol.md. Failures are surfaced to whoever registered with onError rather
// than swallowed, and the caller gets `null` so it can simply bail.

const listeners = new Set();
const errorHandlers = new Set();

// The bridge registers a fresh IPC listener on every `on` call, so subscribe once and fan out.
window.core.on((message) => {
  for (const listener of listeners) {
    try {
      listener(message);
    } catch (error) {
      console.error("listener failed", error);
    }
  }
});

export function on(listener) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function onError(handler) {
  errorHandlers.add(handler);
  return () => errorHandlers.delete(handler);
}

// `quiet` skips the error handlers: for the background state poll, where one missed answer is not
// worth a toast.
export async function call(method, params, { quiet = false } = {}) {
  let reply;
  try {
    reply = await window.core.call(method, params || {});
  } catch (error) {
    reply = { ok: false, error: String((error && error.message) || error) };
  }
  if (reply && reply.ok) return reply.result;
  const message = (reply && reply.error) || "the core did not answer";
  if (!quiet) for (const handler of errorHandlers) handler(method, message);
  return null;
}

// Errors the user can do nothing about deserve plainer words than the core's.
export function describeError(method, message) {
  if (/runner not wired/i.test(message)) {
    return { title: "Runs are not available yet", body: "This build of the core can't start, pause or abort runs. Settings and history still work." };
  }
  if (/core is not running|did not answer/i.test(message)) {
    return { title: "The core is not responding", body: message };
  }
  return { title: `${method} failed`, body: message };
}
