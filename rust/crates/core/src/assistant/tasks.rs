//! Background tasks: up to three agents working on their own, each with its own conversation.
//!
//! A task gets only the file and shell tools (the shell and writes still ask the user). It has no
//! screen, no speech, no camera and no UI, so it can never speak, draw or click. Its progress
//! reaches the shell only as `task` events. The registry is saved to `tasks.json`; a task that was
//! running when the app closed comes back as `interrupted` and is resumed only when asked.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::chat::Cancel;
use serde_json::{json, Value};

use super::agent::{self, LoopCfg, LoopEnd};
use super::tools::{Kind, TurnCtx};
use super::{store, Memory, Services};

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRecord {
    pub id: String,
    pub title: String,
    pub instructions: String,
    /// running, done, failed, stopped or interrupted.
    pub status: String,
    pub summary: String,
}

impl TaskRecord {
    fn to_json(&self) -> Value {
        json!({ "id": self.id, "title": self.title, "status": self.status, "summary": self.summary })
    }
}

pub struct Tasks {
    svc: Arc<Services>,
    records: Mutex<Vec<TaskRecord>>,
    cancels: Mutex<HashMap<String, Cancel>>,
    next: AtomicU64,
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

impl Tasks {
    /// Read the registry. Anything recorded as running did not finish: it is `interrupted` now.
    pub fn load(svc: Arc<Services>) -> Arc<Self> {
        let path = svc.home.join("tasks.json");
        let mut records: Vec<TaskRecord> = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|r| {
                let s = |k: &str| r.get(k).and_then(Value::as_str).map(str::to_string);
                Some(TaskRecord {
                    id: s("id")?,
                    title: s("title")?,
                    instructions: s("instructions").unwrap_or_default(),
                    status: s("status")?,
                    summary: s("summary").unwrap_or_default(),
                })
            })
            .collect();
        let mut changed = false;
        for r in &mut records {
            if r.status == "running" {
                r.status = "interrupted".into();
                r.summary = "the app closed while this was running".into();
                changed = true;
            }
        }
        let next = records
            .iter()
            .filter_map(|r| r.id.trim_start_matches('t').parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let tasks = Arc::new(Self {
            svc,
            records: Mutex::new(records),
            cancels: Mutex::new(HashMap::new()),
            next: AtomicU64::new(next),
        });
        if changed {
            tasks.save();
        }
        tasks
    }

    fn records(&self) -> std::sync::MutexGuard<'_, Vec<TaskRecord>> {
        self.records.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self) {
        let records = self.records();
        self.write_registry(&records);
    }

    /// Write the registry while the caller holds the records lock. A status is only visible to
    /// other threads together with the file that says so, and writers never share the temp file.
    fn write_registry(&self, records: &[TaskRecord]) {
        let rows: Vec<Value> = records
            .iter()
            .map(|r| {
                json!({ "id": r.id, "title": r.title, "instructions": r.instructions,
                        "status": r.status, "summary": r.summary })
            })
            .collect();
        let _ = std::fs::create_dir_all(&self.svc.home);
        let path = self.svc.home.join("tasks.json");
        let tmp = self.svc.home.join("tasks.json.tmp");
        if std::fs::write(&tmp, Value::Array(rows).to_string()).is_ok()
            && std::fs::rename(&tmp, &path).is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub fn list(&self) -> Vec<Value> {
        self.records().iter().map(TaskRecord::to_json).collect()
    }

    pub fn get(&self, id: &str) -> Option<TaskRecord> {
        self.records().iter().find(|r| r.id == id).cloned()
    }

    pub fn running_count(&self) -> usize {
        self.records()
            .iter()
            .filter(|r| r.status == "running")
            .count()
    }

    fn announce(&self, r: &TaskRecord) {
        self.svc.emit("task", r.to_json());
    }

    /// Start a task. Refused when the limit of running tasks is reached.
    pub fn start(self: &Arc<Self>, title: &str, instructions: &str) -> Result<String, String> {
        let (title, instructions) = (clip(title.trim(), 80), instructions.trim().to_string());
        if title.is_empty() || instructions.is_empty() {
            return Err("a task needs a title and instructions".into());
        }
        let record = {
            let mut records = self.records();
            let running = records.iter().filter(|r| r.status == "running").count();
            if running >= self.svc.limits.max_tasks {
                return Err(format!(
                    "{} background tasks are already running: stop one first",
                    self.svc.limits.max_tasks
                ));
            }
            let record = TaskRecord {
                id: format!("t{}", self.next.fetch_add(1, Ordering::SeqCst)),
                title,
                instructions: instructions.clone(),
                status: "running".into(),
                summary: String::new(),
            };
            records.push(record.clone());
            record
        };
        self.save();
        self.announce(&record);
        let messages = vec![json!({ "role": "user", "content": instructions })];
        self.spawn(&record, messages);
        Ok(record.id)
    }

    /// Resume a task that is not running and not done. Only ever called on the user's request.
    pub fn resume(self: &Arc<Self>, id: &str) -> Result<(), String> {
        let record = {
            let mut records = self.records();
            let running = records.iter().filter(|r| r.status == "running").count();
            let r = records
                .iter_mut()
                .find(|r| r.id == id)
                .ok_or_else(|| format!("there is no task {id}"))?;
            match r.status.as_str() {
                "running" => return Err(format!("task {id} is already running")),
                "done" => return Err(format!("task {id} is already done")),
                _ => {}
            }
            if running >= self.svc.limits.max_tasks {
                return Err(format!(
                    "{} background tasks are already running: stop one first",
                    self.svc.limits.max_tasks
                ));
            }
            r.status = "running".into();
            r.summary.clear();
            r.clone()
        };
        self.save();
        self.announce(&record);
        let dir = self.svc.conversations_dir();
        let mut messages = store::load(&dir, &format!("task-{id}"))
            .unwrap_or_else(|| vec![json!({ "role": "user", "content": record.instructions })]);
        messages.push(json!({ "role": "user", "content":
            "You were interrupted. Carry on from where you left off, and finish with a short summary." }));
        self.spawn(&record, messages);
        Ok(())
    }

    /// Cancel one running task. Ok(false) when it was not running.
    pub fn stop(&self, id: &str) -> Result<bool, String> {
        if let Some(c) = self
            .cancels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
        {
            c.cancel();
            return Ok(true);
        }
        if self.get(id).is_some() {
            Ok(false)
        } else {
            Err(format!("there is no task {id}"))
        }
    }

    pub fn stop_all(&self) {
        for c in self
            .cancels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            c.cancel();
        }
    }

    /// Wait until no task is running (for tests and shutdown).
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while self.running_count() > 0 {
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    fn spawn(self: &Arc<Self>, record: &TaskRecord, messages: Vec<Value>) {
        let cancel = Cancel::new();
        self.cancels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(record.id.clone(), cancel.clone());
        let me = self.clone();
        let (id, title) = (record.id.clone(), record.title.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("task-{id}"))
            .spawn(move || {
                let (status, summary) = me.run(&id, &title, messages, &cancel);
                me.cancels
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                {
                    // The event goes out before the status can be seen as finished.
                    let mut records = me.records();
                    if let Some(r) = records.iter_mut().find(|r| r.id == id) {
                        r.status = status.to_string();
                        r.summary = clip(&summary, 600);
                        me.announce(r);
                    }
                    // Saved before the lock is released: a reader that sees the task finished
                    // (running_count, wait_idle) must also find it finished on disk.
                    me.write_registry(&records);
                }
            });
        if spawned.is_err() {
            let mut records = self.records();
            if let Some(r) = records.iter_mut().find(|r| r.id == record.id) {
                r.status = "failed".into();
                r.summary = "could not start a thread".into();
            }
        }
    }

    fn run(
        self: &Arc<Self>,
        id: &str,
        title: &str,
        mut messages: Vec<Value>,
        cancel: &Cancel,
    ) -> (&'static str, String) {
        let chat = match (self.svc.chat)() {
            Ok(c) => c,
            Err(e) => return ("failed", e),
        };
        let memory = Mutex::new(Memory::default());
        let cfg = LoopCfg {
            svc: &self.svc,
            chat: chat.as_ref(),
            kind: Kind::Task,
            label: Some(title),
            cancel,
            tasks: self,
            memory: &memory,
            system: agent::task_prompt(title),
            max_iterations: self.svc.limits.task_iterations,
            max_tokens: self.svc.limits.turn_tokens,
            time: self.svc.limits.task_time,
        };
        let dir = self.svc.conversations_dir();
        let name = format!("task-{id}");
        let end = agent::run(
            &cfg,
            &mut TurnCtx::default(),
            &mut messages,
            &|m: &[Value]| store::save(&dir, &name, m),
        );
        store::save(&dir, &name, &messages);
        match end {
            LoopEnd::Final(t) => ("done", t),
            LoopEnd::Cancelled => ("stopped", "stopped by the user".into()),
            LoopEnd::Failed(e) => ("failed", e),
            LoopEnd::OutOfSteps => ("failed", "ran out of steps".into()),
            LoopEnd::OutOfTime => ("failed", "ran out of time".into()),
            LoopEnd::Stuck(r) => ("failed", r),
        }
    }
}
