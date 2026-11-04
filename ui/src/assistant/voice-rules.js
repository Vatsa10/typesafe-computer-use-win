// The rules for turning raw recognizer text into something worth sending to the assistant. Pure
// functions, no timers and no I/O, so the conversation controller and its self-test share them.
//
// Three problems live here:
//   1. Pointer's own voice comes back through the microphone and is transcribed as if the user said it.
//   2. Fillers ("um", "uh") are noise.
//   3. Short acknowledgements ("okay") said while Pointer talks are listening noises, not requests.
//
// The one rule that outranks the others: a word that changes the meaning of what was said is never
// thrown away. "no", "not", "instead" and their kin protect a line from being called an echo.

const FILLERS = new Set(["um", "umm", "uh", "uhh", "uhm", "er", "erm", "ah", "hmm", "hm", "mm", "mmm"]);
const BACKCHANNEL = new Set(["ok", "okay", "yeah", "yep", "mhm", "uhhuh"]);
const PROTECTED = new Set([
  "no", "not", "nope", "dont", "doesnt", "didnt", "never", "instead", "stop", "wait", "cancel",
  "without", "rather", "actually", "but", "except", "undo", "wrong", "other",
]);

/** Lower-case letters only, so "Don't," and "dont" compare equal. */
function norm(word) {
  return String(word).toLowerCase().replace(/[^a-z0-9]/g, "");
}

function wordsOf(text) {
  return String(text || "")
    .split(/\s+/)
    .map((raw) => ({ raw, key: norm(raw) }))
    .filter((w) => w.key);
}

/** Drop fillers and collapse spacing. "um open uh settings" becomes "open settings". */
function clean(text) {
  return wordsOf(text)
    .filter((w) => !FILLERS.has(w.key))
    .map((w) => w.raw)
    .join(" ")
    .trim();
}

function countWords(text) {
  return wordsOf(clean(text)).length;
}

function hasProtected(text) {
  return wordsOf(text).some((w) => PROTECTED.has(w.key));
}

/** Every word is an acknowledgement ("okay", "okay okay", "yeah"). */
function isBackchannel(text) {
  const words = wordsOf(clean(text));
  return words.length > 0 && words.every((w) => BACKCHANNEL.has(w.key));
}

/**
 * Is `heard` Pointer's own voice coming back through the microphone? True only when every word (or,
 * for four words or more, at least 85% of them) appears in `spoken`, and none of them is a word that
 * changes meaning. The line is kept whole or dropped whole: a user's sentence is never trimmed down
 * to the part that is not an echo, because the part that is left may say the opposite.
 */
function isEcho(heard, spoken) {
  const words = wordsOf(clean(heard));
  if (!words.length || !spoken) return false;
  if (words.some((w) => PROTECTED.has(w.key))) return false;
  const said = new Set(wordsOf(spoken).map((w) => w.key));
  const inside = words.filter((w) => said.has(w.key)).length;
  return words.length >= 4 ? inside / words.length >= 0.85 : inside === words.length;
}

module.exports = { clean, countWords, hasProtected, isBackchannel, isEcho, wordsOf, FILLERS, BACKCHANNEL, PROTECTED };
