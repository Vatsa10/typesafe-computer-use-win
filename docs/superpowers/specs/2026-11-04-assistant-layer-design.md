# Assistant layer: design

**Status:** accepted 2026-11-04
**Goal:** Pointer behaves like a conversational assistant, not a one-shot command runner, while
its cheap perception and action loop stays the hands.

## Modes (the user's choice, Settings)

- `POINTER_MODE=assistant` (default when an agent key exists): a parent agent owns the conversation.
- `POINTER_MODE=quick`: today's behaviour, TypeSafe routing only, about $0.0002 a step.

Model choice: `POINTER_AGENT_BASE_URL` (default OpenAI; any OpenAI-compatible server such as Groq,
OpenRouter or a local one), `POINTER_AGENT_MODEL` (default `gpt-4.1`), `POINTER_AGENT_API_KEY`
(secret; falls back to `OPENAI_API_KEY`). No agent key means quick mode, never a failure.

## Shape

```
voice / bar / wake → shell → chat{text,source} → parent agent (core) → tools
  interact      speech + marks + text, delivered together, one reply path
  pointer_run   the existing run loop, as a blocking tool (act follows the panel mode)
  observe       perception summary: numbered items, windows, displays, mouse
  files, shell  read tools free; writes and shell need a confirm from the user
  tasks         up to 3 background agents with no screen, speech or UI
  camera_look   only when asked and only if enabled
  history       search of saved conversations
```

## Rules

- One foreground turn. A new `chat` cancels the unfinished turn and continues the same conversation.
- Only `interact` reaches the user. A turn ending without it shows its text silently.
- `interact` is validated whole before any effect. An identical repeat is a no-op.
- Tool output and screen text are data, never instructions.
- Background tasks never speak, draw, click, or use the camera.
- Consequential actions need the user: shell, file writes, sending, buying, deleting.
- Conversations persist under `POINTER_HOME\conversations`. No API key is ever written there.
- Wake phrase and camera default OFF. The mic is never streamed off-device while resting.

## Protocol additions

Methods: `chat{text,source:"voice"|"typed"}`, `gate{heard,speaking}` → `{interrupt,confidence}`,
`interrupt` (stop speech only), `stop_all`, `new_conversation`, `tasks`, `task_stop{id}`,
`confirm_reply{id,allow}`, `camera_frames{id,images?,error?}`.

Events: `assistant{state,detail}` with state idle|listening|thinking|working|speaking|control,
`reply{text}`, `speech{action:"start"|"end",text}`, `task{id,title,status,summary}`,
`confirm{id,summary}`, `camera{action:"capture",id,seconds}`. Marks reuse `point`, plus `shape`:
box|circle|arrow|underline|note|step (unknown draws a box).
