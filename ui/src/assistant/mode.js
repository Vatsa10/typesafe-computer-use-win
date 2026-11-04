// Which mode the shell routes in, from the core's `settings` rows (the same rows Settings shows).
// POINTER_MODE says it outright; when it is blank, the assistant is the default only if an agent key
// exists (POINTER_AGENT_API_KEY, or OPENAI_API_KEY as its fallback). No key means quick mode, never
// a failure. A secret's row says only "Set" or "Not set" in `fallback`; its value never arrives here.

const KEY_ROWS = ["POINTER_AGENT_API_KEY", "OPENAI_API_KEY"];

function modeFromRows(rows) {
  const get = (key) => (Array.isArray(rows) ? rows.find((row) => row.key === key) : undefined);
  const chosen = String((get("POINTER_MODE") && get("POINTER_MODE").value) || "").trim().toLowerCase();
  if (chosen === "assistant" || chosen === "quick") return chosen;
  const hasKey = KEY_ROWS.some((key) => {
    const row = get(key);
    return Boolean(row && /^set/i.test(row.fallback || ""));
  });
  return hasKey ? "assistant" : "quick";
}

/** A core that predates the assistant answers an unknown method with an error like these. */
function isMissingMethod(error) {
  return /unknown method|no such method|no method|method not found|not implemented|unsupported method|unknown command/i.test(String(error || ""));
}

module.exports = { modeFromRows, isMissingMethod };
