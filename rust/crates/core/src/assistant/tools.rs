//! The tools the model can call.
//!
//! A tool's result is always a string for the model; a failure starts with `error:` so the model
//! can recover. Which tools exist depends on [`Kind`]: a background task gets no screen, speech,
//! UI or camera tool at all, both in the list it is shown and in what `dispatch` will run.

use std::io::Read;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::tasks::Tasks;
use super::{safety, store, Memory, Observation, Services};
use crate::writer::shape_or_box;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Foreground,
    Task,
}

/// What one turn has gathered so far.
#[derive(Default)]
pub struct TurnCtx {
    /// The latest observe this turn. Marks refer to its item numbers.
    pub observation: Option<Observation>,
    /// The previous successful `interact`, to catch an identical repeat.
    pub last_interact: Option<Value>,
    pub interacted: bool,
    /// Camera frames waiting to go to the model with the next request, in memory only.
    pub pending_images: Vec<String>,
}

pub struct ToolEnv<'a> {
    pub svc: &'a Arc<Services>,
    pub kind: Kind,
    pub label: Option<&'a str>,
    pub cancel: &'a api::chat::Cancel,
    pub tasks: &'a Arc<Tasks>,
    pub memory: &'a Mutex<Memory>,
    pub ctx: &'a mut TurnCtx,
}

const FOREGROUND: [&str; 13] = [
    "interact",
    "observe",
    "pointer_run",
    "read_file",
    "list_dir",
    "write_file",
    "run_shell",
    "camera_look",
    "start_task",
    "list_tasks",
    "stop_task",
    "resume_task",
    "search_history",
];

const TASK: [&str; 5] = [
    "read_file",
    "list_dir",
    "write_file",
    "run_shell",
    "search_history",
];

pub fn names(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Foreground => &FOREGROUND,
        Kind::Task => &TASK,
    }
}

fn func(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({ "type": "function", "function": {
        "name": name, "description": description,
        "parameters": { "type": "object", "properties": properties, "required": required },
    }})
}

fn schema_of(name: &str) -> Value {
    match name {
        "interact" => func(
            "interact",
            "The only way to reach the user. Speak a short line, show a short text and mark items on the \
             screen, all together in one call. Give at least one of speech, text, marks. Marks refer to \
             item numbers from your latest observe.",
            json!({
                "speech": {"type": "string", "description": "What to say aloud: one or two plain sentences."},
                "text": {"type": "string", "description": "Text to show on screen. Defaults to the speech."},
                "marks": {"type": "array", "maxItems": 8, "items": {"type": "object", "properties": {
                    "item": {"type": "integer", "description": "An item number from the latest observe."},
                    "shape": {"type": "string", "enum": ["box", "circle", "arrow", "underline", "note", "step"]},
                    "label": {"type": "string"},
                }, "required": ["item"]}},
                "hold_seconds": {"type": "number", "description": "How long marks stay, 1 to 30. Default 6."},
            }),
            &[],
        ),
        "observe" => func(
            "observe",
            "Look at the screen under the mouse: numbered items (text and controls), open windows, \
             displays and where the mouse is. Call it before pointing at anything.",
            json!({}),
            &[],
        ),
        "pointer_run" => func(
            "pointer_run",
            "Operate the computer toward one goal with Pointer's own look-and-act loop, and wait for \
             it. In Dry mode it only shows what it would press. Returns the outcome and its answer.",
            json!({"goal": {"type": "string", "description": "One clear goal in plain English."}}),
            &["goal"],
        ),
        "read_file" => func(
            "read_file",
            "Read a text file inside the user's profile folder. Secrets (.env files, browser stores, SSH keys) are refused.",
            json!({"path": {"type": "string"}, "max_bytes": {"type": "integer"}}),
            &["path"],
        ),
        "list_dir" => func(
            "list_dir",
            "List a folder inside the user's profile folder.",
            json!({"path": {"type": "string"}}),
            &["path"],
        ),
        "write_file" => func(
            "write_file",
            "Write a file inside the user's profile folder. The user is asked first and may refuse.",
            json!({"path": {"type": "string"}, "content": {"type": "string"},
                   "append": {"type": "boolean"}}),
            &["path", "content"],
        ),
        "run_shell" => func(
            "run_shell",
            "Run one PowerShell command. The user is asked first and may refuse. Output is returned.",
            json!({"command": {"type": "string"}, "cwd": {"type": "string"}}),
            &["command"],
        ),
        "camera_look" => func(
            "camera_look",
            "Look through the user's camera for a moment. Only when the user asks, and only when the \
             camera is enabled. The frames are not saved.",
            json!({"seconds": {"type": "number"}}),
            &[],
        ),
        "start_task" => func(
            "start_task",
            "Start a background task (at most three at once) for a longer job that needs no screen. \
             It reports back on its own.",
            json!({"title": {"type": "string"}, "instructions": {"type": "string"}}),
            &["title", "instructions"],
        ),
        "list_tasks" => func("list_tasks", "List background tasks and their status.", json!({}), &[]),
        "stop_task" => func(
            "stop_task",
            "Stop a background task.",
            json!({"id": {"type": "string"}}),
            &["id"],
        ),
        "resume_task" => func(
            "resume_task",
            "Resume a task that was interrupted or stopped. Only when the user asks.",
            json!({"id": {"type": "string"}}),
            &["id"],
        ),
        "search_history" => func(
            "search_history",
            "Search saved conversations for words.",
            json!({"query": {"type": "string"}}),
            &["query"],
        ),
        _ => json!({}),
    }
}

pub fn schemas(kind: Kind) -> Vec<Value> {
    names(kind).iter().map(|n| schema_of(n)).collect()
}

pub fn dispatch(env: &mut ToolEnv, name: &str, args: &Value) -> String {
    if !names(env.kind).contains(&name) {
        return format!("error: there is no tool {name} here");
    }
    let done = match name {
        "interact" => interact(env, args),
        "observe" => observe(env),
        "pointer_run" => pointer_run(env, args),
        "read_file" => read_file(env, args),
        "list_dir" => list_dir(env, args),
        "write_file" => write_file(env, args),
        "run_shell" => run_shell(env, args),
        "camera_look" => camera_look(env, args),
        "start_task" => start_task(env, args),
        "list_tasks" => Ok(Value::Array(env.tasks.list()).to_string()),
        "stop_task" => stop_task(env, args),
        "resume_task" => resume_task(env, args),
        "search_history" => search_history(env, args),
        _ => Err(format!("there is no tool {name}")),
    };
    match done {
        Ok(text) => text,
        Err(e) => format!("error: {e}"),
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn need_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    match arg_str(args, key)? {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(format!("{key} is required")),
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}\n[truncated]")
}

// ---------------------------------------------------------------- interact

/// One mark, checked and resolved.
struct Mark {
    rect: (f64, f64, f64, f64),
    shape: &'static str,
    label: String,
    item: u64,
}

struct Plan {
    speech: String,
    text: String,
    marks: Vec<Mark>,
    hold: f64,
}

/// Validate the whole call before anything happens. Any bad reference rejects all of it.
fn plan(args: &Value, observation: Option<&Observation>) -> Result<Plan, String> {
    let speech = arg_str(args, "speech")?.unwrap_or("").to_string();
    let text = arg_str(args, "text")?.unwrap_or("").to_string();
    let raw_marks: Vec<Value> = match args.get("marks") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err("marks must be an array".into()),
    };
    if speech.is_empty() && text.is_empty() && raw_marks.is_empty() {
        return Err("interact needs speech, text or marks".into());
    }
    if speech.chars().count() > 600 {
        return Err("speech is too long: keep it to a sentence or two".into());
    }
    if text.chars().count() > 2000 {
        return Err("text is too long".into());
    }
    if raw_marks.len() > 8 {
        return Err("at most 8 marks".into());
    }
    let hold = match args.get("hold_seconds") {
        None | Some(Value::Null) => 6.0,
        Some(v) => v
            .as_f64()
            .ok_or("hold_seconds must be a number")?
            .clamp(1.0, 30.0),
    };
    let mut marks = Vec::new();
    for (i, m) in raw_marks.iter().enumerate() {
        let item = m
            .get("item")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("mark {i}: item must be a whole number"))?;
        let obs =
            observation.ok_or("no observation yet this turn: call observe before marking items")?;
        let rect = obs
            .rect(item as usize)
            .ok_or_else(|| format!("mark {i}: item {item} is not in the latest observe"))?;
        let label = match m.get("label") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) if s.chars().count() <= 80 => s.trim().to_string(),
            Some(Value::String(_)) => return Err(format!("mark {i}: label is too long")),
            Some(_) => return Err(format!("mark {i}: label must be a string")),
        };
        let shape = match m.get("shape") {
            None | Some(Value::Null) => "box",
            Some(Value::String(s)) => shape_or_box(s),
            Some(_) => return Err(format!("mark {i}: shape must be a string")),
        };
        marks.push(Mark {
            rect,
            shape,
            label,
            item,
        });
    }
    Ok(Plan {
        speech,
        text,
        marks,
        hold,
    })
}

fn fingerprint(p: &Plan) -> Value {
    json!({
        "speech": p.speech, "text": p.text, "hold": p.hold,
        "marks": p.marks.iter().map(|m| json!([m.item, m.shape, m.label])).collect::<Vec<_>>(),
    })
}

fn interact(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let plan = plan(args, env.ctx.observation.as_ref())?;
    let key = fingerprint(&plan);
    if env.ctx.last_interact.as_ref() == Some(&key) {
        return Ok("duplicate: identical to your previous interact; nothing was done again".into());
    }
    env.ctx.last_interact = Some(key);
    env.ctx.interacted = true;
    let svc = env.svc;
    // Together: the marks, the text, then the voice.
    if !plan.marks.is_empty() {
        svc.emit("point", json!({ "clear": true }));
        let n = plan.marks.len();
        for (i, m) in plan.marks.iter().enumerate() {
            let (x, y, w, h) = m.rect;
            let mut p = json!({
                "x": x, "y": y, "w": w, "h": h, "label": m.label,
                "tone": if m.shape == "note" { "note" } else { "point" },
                "shape": m.shape, "hold": plan.hold,
            });
            if m.shape == "step" {
                p["step"] = json!(i + 1);
                p["of"] = json!(n);
            }
            svc.emit("point", p);
        }
    }
    let shown = if plan.text.is_empty() {
        &plan.speech
    } else {
        &plan.text
    };
    if !shown.is_empty() {
        svc.emit("reply", json!({ "text": shown }));
    }
    let mut spoken = false;
    if !plan.speech.is_empty() {
        env.memory
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last_spoken = plan.speech.clone();
        svc.assistant_state("speaking", "");
        svc.emit("speech", json!({ "action": "start", "text": plan.speech }));
        spoken = svc.voice.speak(&plan.speech, env.cancel);
        svc.emit("speech", json!({ "action": "end", "text": plan.speech }));
    }
    Ok(format!(
        "ok: shown{}{}",
        if plan.marks.is_empty() {
            String::new()
        } else {
            format!(", {} mark(s) drawn", plan.marks.len())
        },
        match (plan.speech.is_empty(), spoken) {
            (true, _) => "",
            (false, true) => ", spoken",
            (false, false) => ", not spoken (the voice is off or unavailable)",
        }
    ))
}

// ---------------------------------------------------------------- the screen and the run

fn observe(env: &mut ToolEnv) -> Result<String, String> {
    let obs = env.svc.senses.observe()?;
    let text = clip(&obs.summary.to_string(), 14_000);
    env.ctx.observation = Some(obs);
    Ok(text)
}

fn pointer_run(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let goal = need_str(args, "goal")?;
    let act = (env.svc.act_mode)();
    if act {
        env.svc.assistant_state(
            "control",
            "controlling the computer: press Esc twice to stop",
        );
    }
    let result = env.svc.hands.run_goal(goal, act, env.cancel);
    // The screen is not what the last observe saw any more.
    if act {
        env.ctx.observation = None;
    }
    let r = result?;
    Ok(
        json!({ "outcome": r.outcome, "answer": r.answer, "acted": act,
               "seconds": (r.seconds * 10.0).round() / 10.0 })
        .to_string(),
    )
}

// ---------------------------------------------------------------- files and the shell

fn read_file(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let path = safety::resolve(need_str(args, "path")?, &env.svc.profile)?;
    let cap = args
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(20_000)
        .clamp(1, 200_000) as usize;
    let file = std::fs::File::open(&path).map_err(|e| format!("cannot open it: {e}"))?;
    if file.metadata().map(|m| m.is_dir()).unwrap_or(false) {
        return Err("that is a folder: use list_dir".into());
    }
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read it: {e}"))?;
    if bytes.iter().take(2048).any(|b| *b == 0) {
        return Err("that looks like a binary file".into());
    }
    let truncated = bytes.len() > cap;
    bytes.truncate(cap);
    let mut text = String::from_utf8_lossy(&bytes).to_string();
    if truncated {
        text.push_str("\n[truncated]");
    }
    Ok(text)
}

fn list_dir(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let path = safety::resolve(need_str(args, "path")?, &env.svc.profile)?;
    let mut rows = Vec::new();
    let entries = std::fs::read_dir(&path).map_err(|e| format!("cannot list it: {e}"))?;
    for entry in entries.flatten() {
        let p = entry.path();
        if safety::forbidden(&p).is_some() {
            continue; // never even named
        }
        let meta = entry.metadata().ok();
        rows.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "dir": meta.as_ref().is_some_and(|m| m.is_dir()),
            "bytes": meta.as_ref().map(|m| m.len()),
        }));
    }
    rows.sort_by_key(|r| r["name"].as_str().unwrap_or("").to_lowercase());
    let total = rows.len();
    rows.truncate(200);
    Ok(json!({ "entries": rows, "total": total }).to_string())
}

fn waited(d: std::time::Duration) -> String {
    if d.as_secs() >= 1 {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", d.as_millis())
    }
}

/// Ask the user. Denied by default: no reply in the wait, a cancel or an error all mean no.
fn confirm(env: &ToolEnv, summary: &str) -> Result<(), String> {
    let svc = env.svc;
    let summary = match env.label {
        Some(l) => format!("[background task: {l}] {summary}"),
        None => summary.to_string(),
    };
    let id = svc.confirms.open("confirm");
    svc.emit("confirm", json!({ "id": id, "summary": summary }));
    match svc.confirms.wait(&id, svc.limits.confirm_wait, env.cancel) {
        Some(true) => Ok(()),
        Some(false) => Err("denied: the user said no".into()),
        None if env.cancel.is_cancelled() => Err("cancelled".into()),
        None => Err(format!(
            "denied: the user did not answer within {}",
            waited(svc.limits.confirm_wait)
        )),
    }
}

fn write_file(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let path = safety::resolve(need_str(args, "path")?, &env.svc.profile)?;
    let content = match args.get("content") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err("content is required".into()),
    };
    if content.len() > 500_000 {
        return Err("content is too large".into());
    }
    let append = args.get("append").and_then(Value::as_bool).unwrap_or(false);
    let what = if append { "append to" } else { "write" };
    confirm(
        env,
        &format!("{what} file {} ({} bytes)", path.display(), content.len()),
    )?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create the folder: {e}"))?;
    }
    if append {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open it: {e}"))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("cannot write it: {e}"))?;
    } else {
        std::fs::write(&path, content.as_bytes()).map_err(|e| format!("cannot write it: {e}"))?;
    }
    Ok(format!(
        "ok: wrote {} bytes to {}",
        content.len(),
        path.display()
    ))
}

fn run_shell(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let command = need_str(args, "command")?;
    if let Some(why) = safety::command_mentions_forbidden(command) {
        return Err(format!("refused: {why}"));
    }
    let cwd = match arg_str(args, "cwd")? {
        Some(c) if !c.is_empty() => Some(safety::resolve(c, &env.svc.profile)?),
        _ => None,
    };
    confirm(env, &format!("run PowerShell: {command}"))?;
    let out = env.svc.shell.run(
        command,
        cwd.as_deref().or(Some(env.svc.profile.as_path())),
        env.svc.limits.shell_timeout,
        env.cancel,
    )?;
    let code = out
        .code
        .map_or("no exit code".to_string(), |c| format!("exit code {c}"));
    Ok(format!("{code}\n{}", clip(&out.output, 6000)))
}

fn search_history(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let query = need_str(args, "query")?;
    let hits = store::search(&env.svc.conversations_dir(), query, 6);
    Ok(json!({ "matches": hits }).to_string())
}

// ---------------------------------------------------------------- the camera

fn camera_look(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let svc = env.svc;
    if !(svc.camera_enabled)() {
        return Err("the camera is off: the user can turn it on in Settings".into());
    }
    let seconds = args
        .get("seconds")
        .and_then(Value::as_f64)
        .unwrap_or(2.0)
        .clamp(1.0, 5.0);
    let id = svc.cameras.open("cam");
    svc.emit(
        "camera",
        json!({ "action": "capture", "id": id, "seconds": seconds }),
    );
    let reply = svc
        .cameras
        .wait(&id, svc.limits.camera_wait, env.cancel)
        .ok_or_else(|| {
            if env.cancel.is_cancelled() {
                "cancelled".to_string()
            } else {
                format!(
                    "the camera did not answer within {}",
                    waited(svc.limits.camera_wait)
                )
            }
        })?;
    let images = reply.map_err(|e| format!("the camera could not capture: {e}"))?;
    let mut kept = Vec::new();
    for img in images.into_iter().take(4) {
        let url = if img.starts_with("data:image/") {
            img
        } else {
            format!("data:image/jpeg;base64,{img}")
        };
        if url.len() <= 6_000_000 {
            kept.push(url);
        }
    }
    if kept.is_empty() {
        return Err("no frames were captured".into());
    }
    let n = kept.len();
    env.ctx.pending_images.extend(kept);
    Ok(format!("{n} frame(s) captured and attached below"))
}

// ---------------------------------------------------------------- tasks

fn start_task(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let title = need_str(args, "title")?;
    let instructions = need_str(args, "instructions")?;
    let id = env.tasks.start(title, instructions)?;
    Ok(format!("started task {id}"))
}

fn stop_task(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let id = need_str(args, "id")?;
    if env.tasks.stop(id)? {
        Ok(format!("task {id} is stopping"))
    } else {
        Ok(format!("task {id} was not running"))
    }
}

fn resume_task(env: &mut ToolEnv, args: &Value) -> Result<String, String> {
    let id = need_str(args, "id")?;
    env.tasks.resume(id)?;
    Ok(format!("task {id} resumed"))
}
