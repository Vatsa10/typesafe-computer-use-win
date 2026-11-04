//! The assistant layer, end to end against fakes.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use api::chat::Cancel;
use serde_json::{json, Value};

use super::testing::{call, calling, saying, FakeChat, Harness, Script};
use super::tools::{self, Kind};
use super::{store, Limits};

fn turn(h: &Harness, text: &str) {
    h.assistant().run_turn(text, "typed", &Cancel::new());
}

/// The content of the tool message answering call `id` in the last request the model saw.
fn tool_result(h: &Harness, id: &str) -> String {
    let calls = h.chat.calls();
    for c in calls.iter().rev() {
        for m in &c.messages {
            if m["role"] == "tool" && m["tool_call_id"] == id {
                return m["content"].as_str().unwrap_or("").to_string();
            }
        }
    }
    panic!("no tool result for {id}");
}

fn wait(what: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !what() {
        assert!(Instant::now() < end, "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ------------------------------------------------------------ interact

#[test]
fn interact_draws_shows_and_speaks_together_in_physical_pixels() {
    let h = Harness::new(
        "interact",
        vec![
            calling(vec![
                call("1", "observe", json!({})),
                call(
                    "2",
                    "interact",
                    json!({ "speech": "Press Send.", "text": "Send is highlighted",
                            "marks": [{ "item": 1, "shape": "circle", "label": "Send" },
                                      { "item": 0, "shape": "sparkles" }] }),
                ),
            ]),
            saying("all done"),
        ],
    );
    turn(&h, "where is send");
    let names = h.emit.names();
    let pos = |n: &str| names.iter().position(|x| x == n).unwrap();
    // The marks, then the text, then the voice.
    assert!(pos("point") < pos("reply") && pos("reply") < pos("speech"));
    let points = h.emit.of("point");
    assert_eq!(points[0], json!({ "clear": true }));
    assert_eq!(points[1]["x"], 2760.0, "origin added once");
    assert_eq!(points[1]["y"], -1340.0);
    assert_eq!(
        (points[1]["w"].clone(), points[1]["h"].clone()),
        (json!(100.0), json!(40.0))
    );
    assert_eq!(points[1]["shape"], "circle");
    assert_eq!(points[2]["shape"], "box", "an unknown shape draws a box");
    assert_eq!(h.emit.of("reply")[0]["text"], "Send is highlighted");
    let speech = h.emit.of("speech");
    assert_eq!(speech[0]["action"], "start");
    assert_eq!(speech[1]["action"], "end");
    assert_eq!(
        *h.voice.spoken.lock().unwrap(),
        vec!["Press Send.".to_string()]
    );
    // The closing text after an interact is not shown a second time.
    assert_eq!(h.emit.of("reply").len(), 1);
    assert!(tool_result(&h, "2").starts_with("ok"));
}

#[test]
fn one_bad_reference_rejects_the_whole_call_before_any_effect() {
    let h = Harness::new(
        "reject",
        vec![
            calling(vec![
                call("1", "observe", json!({})),
                call(
                    "2",
                    "interact",
                    json!({ "speech": "Look.", "marks": [{ "item": 0 }, { "item": 99 }] }),
                ),
                // Marks with no observe at all are refused too.
                call(
                    "3",
                    "interact",
                    json!({ "speech": "x", "marks": [{ "item": "zero" }] }),
                ),
                call("4", "interact", json!({})),
            ]),
            saying("ok"),
        ],
    );
    turn(&h, "point");
    assert!(tool_result(&h, "2").contains("item 99 is not in the latest observe"));
    assert!(tool_result(&h, "3").starts_with("error:"));
    assert!(tool_result(&h, "4").contains("needs speech, text or marks"));
    assert!(h.emit.of("point").is_empty(), "nothing was drawn");
    assert!(h.emit.of("speech").is_empty());
    assert!(h.voice.spoken.lock().unwrap().is_empty());
    // And the failed calls did not count as the turn's interact: the closing text shows.
    assert_eq!(h.emit.of("reply")[0]["text"], "ok");
}

#[test]
fn marks_without_an_observe_are_refused() {
    let h = Harness::new(
        "noobserve",
        vec![
            calling(vec![call(
                "1",
                "interact",
                json!({ "marks": [{ "item": 0 }] }),
            )]),
            saying("x"),
        ],
    );
    turn(&h, "point");
    assert!(tool_result(&h, "1").contains("call observe before"));
    assert!(h.emit.of("point").is_empty());
}

#[test]
fn an_identical_repeat_is_a_no_op() {
    let say = json!({ "speech": "Done.", "text": "Done." });
    let h = Harness::new(
        "dup",
        vec![
            calling(vec![
                call("1", "interact", say.clone()),
                call("2", "interact", say.clone()),
                call("3", "interact", json!({ "speech": "Different." })),
            ]),
            saying("x"),
        ],
    );
    turn(&h, "hi");
    assert!(tool_result(&h, "2").starts_with("duplicate"));
    assert_eq!(
        *h.voice.spoken.lock().unwrap(),
        vec!["Done.".to_string(), "Different.".to_string()]
    );
    assert_eq!(
        h.emit.of("speech").len(),
        4,
        "two lines, start and end each"
    );
}

#[test]
fn a_turn_without_interact_shows_its_text_silently() {
    let h = Harness::new("silent", vec![saying("It is three o'clock.")]);
    turn(&h, "what time is it");
    assert_eq!(
        h.emit.of("reply"),
        vec![json!({ "text": "It is three o'clock." })]
    );
    assert!(h.voice.spoken.lock().unwrap().is_empty());
    assert!(h.emit.of("speech").is_empty());
    let states: Vec<String> = h
        .emit
        .of("assistant")
        .iter()
        .map(|s| s["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(states.first().map(String::as_str), Some("thinking"));
    assert_eq!(states.last().map(String::as_str), Some("idle"));
}

#[test]
fn a_model_error_is_a_short_reply_not_a_crash() {
    let h = Harness::new(
        "modelerr",
        vec![Script::Fail(api::chat::ChatError::Http(401, "bad".into()))],
    );
    turn(&h, "hi");
    let reply = h.emit.of("reply")[0]["text"].as_str().unwrap().to_string();
    assert!(reply.contains("the agent key was rejected"), "{reply}");
}

// ------------------------------------------------------------ cancelling

#[test]
fn a_new_chat_cancels_the_unfinished_turn_and_continues_the_conversation() {
    let h = Harness::new("cancel", vec![Script::Hang, saying("second answer")]);
    let a = h.assistant();
    a.chat("first", "voice").unwrap();
    wait(|| h.chat.calls().len() == 1);
    assert!(a.busy());
    a.chat("second", "typed").unwrap();
    wait(|| !h.emit.of("reply").is_empty());
    wait(|| !a.busy());
    assert_eq!(h.emit.of("reply")[0]["text"], "second answer");
    assert!(
        h.voice.stops.load(Ordering::SeqCst) >= 1,
        "the old speech was cut off"
    );
    // The same conversation: the second request carried the first line too.
    let second = &h.chat.calls()[1].messages;
    let users: Vec<&str> = second
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(users, vec!["first", "second"]);
}

#[test]
fn cancelling_a_running_pointer_run_leaves_a_consistent_history() {
    let h = Harness::new(
        "cancelrun",
        vec![
            calling(vec![call(
                "1",
                "pointer_run",
                json!({ "goal": "open mail" }),
            )]),
            saying("second answer"),
        ],
    );
    h.hands.hang.store(true, Ordering::SeqCst);
    let a = h.assistant();
    a.chat("open mail", "voice").unwrap();
    wait(|| !h.hands.runs.lock().unwrap().is_empty());
    a.chat("never mind, what time is it", "voice").unwrap();
    wait(|| !h.emit.of("reply").is_empty());
    wait(|| !a.busy());
    assert!(tool_result(&h, "1").starts_with("error: the run was stopped"));
    // Every call in the saved history has its result.
    let dir = h.svc.conversations_dir();
    let id = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| {
            e.file_name()
                .to_string_lossy()
                .trim_end_matches(".json")
                .to_string()
        })
        .find(|n| n.starts_with("chat-"))
        .unwrap();
    let saved = store::load(&dir, &id).unwrap();
    let mut repaired = saved.clone();
    store::repair(&mut repaired);
    assert_eq!(saved, repaired);
}

#[test]
fn stop_all_cancels_the_turn_and_speech_and_tasks() {
    let h = Harness::new("stopall", vec![Script::Hang, Script::Hang]);
    let a = h.assistant();
    a.chat("go", "typed").unwrap();
    wait(|| !h.chat.calls().is_empty());
    a.stop_all();
    wait(|| !a.busy());
    assert!(h.voice.stops.load(Ordering::SeqCst) >= 1);
    let last = h.emit.of("assistant").last().unwrap().clone();
    assert_eq!(last["state"], "idle");
}

// ------------------------------------------------------------ pointer_run

#[test]
fn pointer_run_follows_the_panel_mode() {
    for act in [false, true] {
        let h = Harness::new(
            "run",
            vec![
                calling(vec![call(
                    "1",
                    "pointer_run",
                    json!({ "goal": "open youtube" }),
                )]),
                saying("x"),
            ],
        );
        h.act.store(act, Ordering::SeqCst);
        turn(&h, "open youtube");
        assert_eq!(
            *h.hands.runs.lock().unwrap(),
            vec![("open youtube".to_string(), act)]
        );
        let result: Value = serde_json::from_str(&tool_result(&h, "1")).unwrap();
        assert_eq!(result["outcome"], if act { "done" } else { "dry run" });
        assert_eq!(result["answer"], "It is open.");
        assert_eq!(result["acted"], act);
        let states: Vec<String> = h
            .emit
            .of("assistant")
            .iter()
            .map(|s| s["state"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(states.contains(&"control".to_string()), act);
        // The system prompt told the model which mode it is in.
        let system = h.chat.calls()[0].messages[0]["content"]
            .as_str()
            .unwrap()
            .to_lowercase();
        assert_eq!(system.contains("act mode"), act);
        assert_eq!(system.contains("dry mode"), !act);
    }
}

#[test]
fn an_acting_run_makes_the_last_observe_stale() {
    let h = Harness::new(
        "stale",
        vec![
            calling(vec![
                call("1", "observe", json!({})),
                call("2", "pointer_run", json!({ "goal": "x" })),
                call(
                    "3",
                    "interact",
                    json!({ "speech": "here", "marks": [{ "item": 0 }] }),
                ),
            ]),
            saying("x"),
        ],
    );
    h.act.store(true, Ordering::SeqCst);
    turn(&h, "go");
    assert!(tool_result(&h, "3").contains("call observe before"));
}

// ------------------------------------------------------------ files and the shell

#[test]
fn reads_are_free_inside_the_profile_and_secrets_are_refused() {
    let outside_name = format!("outside-{}.txt", std::process::id());
    let chat = FakeChat::scripted(vec![
        calling(vec![
            call("a", "read_file", json!({ "path": "notes.txt" })),
            call("b", "read_file", json!({ "path": ".env" })),
            call("c", "read_file", json!({ "path": ".ssh/id_ed25519" })),
            call(
                "d",
                "read_file",
                json!({ "path": "AppData/Local/Google/Chrome/User Data/Default/Login Data" }),
            ),
            call(
                "e",
                "read_file",
                json!({ "path": std::env::temp_dir().join(&outside_name).to_string_lossy() }),
            ),
            call("f", "list_dir", json!({ "path": "." })),
        ]),
        saying("x"),
    ]);
    let h = Harness::with("reads", chat, Limits::default());
    std::fs::write(h.profile.join("notes.txt"), "remember the milk").unwrap();
    std::fs::write(h.profile.join(".env"), "OPENAI_API_KEY=sk-secret").unwrap();
    std::fs::create_dir_all(h.profile.join(".ssh")).unwrap();
    std::fs::write(h.profile.join(".ssh").join("id_ed25519"), "PRIVATE").unwrap();
    let chrome = h
        .profile
        .join("AppData/Local/Google/Chrome/User Data/Default");
    std::fs::create_dir_all(&chrome).unwrap();
    std::fs::write(chrome.join("Login Data"), "x").unwrap();
    std::fs::write(std::env::temp_dir().join(&outside_name), "elsewhere").unwrap();
    turn(&h, "read");
    assert_eq!(tool_result(&h, "a"), "remember the milk");
    for id in ["b", "c", "d"] {
        assert!(tool_result(&h, id).starts_with("error: refused"), "{id}");
    }
    assert!(tool_result(&h, "e").contains("outside your user folder"));
    let listing = tool_result(&h, "f");
    assert!(listing.contains("notes.txt"));
    assert!(
        !listing.contains(".env") && !listing.contains(".ssh"),
        "{listing}"
    );
    assert!(h.emit.of("confirm").is_empty(), "reads never ask");
    let _ = std::fs::remove_file(std::env::temp_dir().join(&outside_name));
}

#[test]
fn a_write_needs_a_yes_and_a_no_leaves_nothing_behind() {
    for allow in [true, false] {
        let h = Harness::new(
            "write",
            vec![
                calling(vec![call(
                    "1",
                    "write_file",
                    json!({ "path": "out/a.txt", "content": "hello" }),
                )]),
                saying("x"),
            ],
        );
        h.answer_confirms(allow);
        turn(&h, "save it");
        let confirms = h.emit.of("confirm");
        assert_eq!(confirms.len(), 1);
        assert!(confirms[0]["summary"].as_str().unwrap().contains("a.txt"));
        let file = h.profile.join("out").join("a.txt");
        if allow {
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
            assert!(tool_result(&h, "1").starts_with("ok"));
        } else {
            assert!(!file.exists());
            assert!(tool_result(&h, "1").contains("denied: the user said no"));
        }
    }
}

#[test]
fn no_answer_to_a_confirm_is_a_denial() {
    let h = Harness::new(
        "timeout",
        vec![
            calling(vec![call("1", "run_shell", json!({ "command": "dir" }))]),
            saying("x"),
        ],
    );
    turn(&h, "list files");
    assert_eq!(h.emit.of("confirm").len(), 1);
    assert!(tool_result(&h, "1").contains("denied: the user did not answer"));
    assert!(h.shell.ran.lock().unwrap().is_empty(), "nothing ran");
    // A late yes is refused: the id is closed.
    let id = h.emit.of("confirm")[0]["id"].as_str().unwrap().to_string();
    assert!(!h.svc.confirms.reply(&id, true));
    // The real wait is sixty seconds.
    assert_eq!(Limits::default().confirm_wait, Duration::from_secs(60));
}

#[test]
fn shell_runs_only_after_a_yes_and_never_reaches_for_secrets() {
    let h = Harness::new(
        "shell",
        vec![
            calling(vec![
                call("1", "run_shell", json!({ "command": "type .env" })),
                call("2", "run_shell", json!({ "command": "echo hi" })),
            ]),
            saying("x"),
        ],
    );
    h.answer_confirms(true);
    turn(&h, "run it");
    assert!(tool_result(&h, "1").starts_with("error: refused"));
    assert!(
        tool_result(&h, "2").contains("exit code 0")
            && tool_result(&h, "2").contains("hello from the shell")
    );
    assert_eq!(*h.shell.ran.lock().unwrap(), vec!["echo hi".to_string()]);
    assert_eq!(h.emit.of("confirm").len(), 1, "the refused one never asked");
}

#[test]
fn a_write_outside_the_profile_or_to_a_secret_never_asks() {
    let h = Harness::new(
        "badwrite",
        vec![
            calling(vec![
                call("1", "write_file", json!({ "path": ".env", "content": "x" })),
                call(
                    "2",
                    "write_file",
                    json!({ "path": "C:/Windows/evil.txt", "content": "x" }),
                ),
            ]),
            saying("x"),
        ],
    );
    turn(&h, "w");
    assert!(tool_result(&h, "1").starts_with("error:"));
    assert!(tool_result(&h, "2").starts_with("error:"));
    assert!(h.emit.of("confirm").is_empty());
}

// ------------------------------------------------------------ tasks

#[test]
fn a_task_sees_no_screen_speech_ui_or_camera_tools() {
    let fg: Vec<&str> = tools::names(Kind::Foreground).to_vec();
    let task: Vec<&str> = tools::names(Kind::Task).to_vec();
    for banned in [
        "interact",
        "observe",
        "pointer_run",
        "camera_look",
        "start_task",
    ] {
        assert!(fg.contains(&banned));
        assert!(!task.contains(&banned), "{banned}");
    }
    let shown: Vec<String> = tools::schemas(Kind::Task)
        .iter()
        .map(|s| s["function"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(shown, task);
}

#[test]
fn a_task_cannot_speak_draw_or_click_even_if_it_tries() {
    let h = Harness::new(
        "taskui",
        vec![
            calling(vec![
                call("1", "interact", json!({ "speech": "hello", "text": "hi" })),
                call("2", "observe", json!({})),
                call("3", "pointer_run", json!({ "goal": "x" })),
                call("4", "camera_look", json!({})),
            ]),
            saying("summary of the work"),
        ],
    );
    h.camera.store(true, Ordering::SeqCst);
    let a = h.assistant();
    let id = a.tasks().start("tidy up", "tidy the downloads").unwrap();
    assert!(a.tasks().wait_idle(Duration::from_secs(5)));
    for n in ["1", "2", "3", "4"] {
        assert!(tool_result(&h, n).contains("there is no tool"), "{n}");
    }
    assert!(h.voice.spoken.lock().unwrap().is_empty());
    assert!(h.hands.runs.lock().unwrap().is_empty());
    for ui in ["point", "reply", "speech", "camera", "highlight"] {
        assert!(h.emit.of(ui).is_empty(), "{ui}");
    }
    let rec = a.tasks().get(&id).unwrap();
    assert_eq!(
        (rec.status.as_str(), rec.summary.as_str()),
        ("done", "summary of the work")
    );
    let events = h.emit.of("task");
    assert_eq!(events.first().unwrap()["status"], "running");
    assert_eq!(events.last().unwrap()["status"], "done");
    assert_eq!(events.last().unwrap()["summary"], "summary of the work");
    // Its own conversation, apart from the foreground one.
    assert!(h
        .svc
        .conversations_dir()
        .join(format!("task-{id}.json"))
        .exists());
    let seen = h.chat.calls();
    let names: Vec<&str> = seen[0]
        .tools
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, tools::names(Kind::Task));
}

#[test]
fn at_most_three_tasks_run_at_once() {
    let h = Harness::new("three", vec![Script::Hang, Script::Hang, Script::Hang]);
    let a = h.assistant();
    for i in 0..3 {
        a.tasks().start(&format!("t{i}"), "wait").unwrap();
    }
    let err = a.tasks().start("fourth", "wait").unwrap_err();
    assert!(err.contains("3 background tasks"), "{err}");
    assert_eq!(a.tasks().running_count(), 3);
    a.tasks().stop_all();
    assert!(a.tasks().wait_idle(Duration::from_secs(5)));
    assert!(a.tasks().list().iter().all(|t| t["status"] == "stopped"));
}

#[test]
fn a_task_that_was_running_comes_back_interrupted_and_waits_to_be_asked() {
    let h = Harness::new("interrupted", vec![saying("picked up and finished")]);
    std::fs::write(
        h.home.join("tasks.json"),
        json!([{ "id": "t7", "title": "sort", "instructions": "sort the files",
                 "status": "running", "summary": "" }])
        .to_string(),
    )
    .unwrap();
    let a = h.assistant();
    let rec = a.tasks().get("t7").unwrap();
    assert_eq!(rec.status, "interrupted");
    std::thread::sleep(Duration::from_millis(150));
    assert!(h.chat.calls().is_empty(), "nothing resumed by itself");
    assert_eq!(a.tasks().running_count(), 0);
    // On request it resumes, and the registry on disk says what happened.
    a.tasks().resume("t7").unwrap();
    assert!(a.tasks().wait_idle(Duration::from_secs(5)));
    assert_eq!(a.tasks().get("t7").unwrap().status, "done");
    let saved = std::fs::read_to_string(h.home.join("tasks.json")).unwrap();
    assert!(saved.contains("\"done\"") && saved.contains("picked up and finished"));
    assert!(a.tasks().resume("t7").unwrap_err().contains("already done"));
    assert!(a.tasks().resume("nope").is_err());
    // The next id does not collide with a loaded one.
    let id = a.tasks().start("another", "go").unwrap();
    assert_eq!(id, "t8");
    a.tasks().wait_idle(Duration::from_secs(5));
}

#[test]
fn the_foreground_can_start_list_and_stop_tasks() {
    let h = Harness::new(
        "fgtasks",
        vec![
            calling(vec![call(
                "1",
                "start_task",
                json!({ "title": "report", "instructions": "write it" }),
            )]),
            saying("a"),
        ],
    );
    let a = h.assistant();
    // The task's own model call takes the next script entry: leave it as the default reply.
    a.run_turn("start a report task", "typed", &Cancel::new());
    assert!(tool_result(&h, "1").starts_with("started task t1"));
    a.tasks().wait_idle(Duration::from_secs(5));
    assert_eq!(a.tasks().list().len(), 1);
}

// ------------------------------------------------------------ the camera

#[test]
fn the_camera_is_refused_when_the_setting_is_off() {
    let h = Harness::new(
        "camoff",
        vec![
            calling(vec![call("1", "camera_look", json!({}))]),
            saying("x"),
        ],
    );
    turn(&h, "look at me");
    assert!(tool_result(&h, "1").contains("the camera is off"));
    assert!(h.emit.of("camera").is_empty());
}

#[test]
fn camera_frames_go_to_the_model_and_nowhere_else() {
    let h = Harness::new(
        "camon",
        vec![
            calling(vec![call("1", "camera_look", json!({ "seconds": 2 }))]),
            saying("a mug"),
        ],
    );
    h.camera.store(true, Ordering::SeqCst);
    let a = h.assistant();
    let a2 = a.clone();
    let emit = h.emit.clone();
    std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if let Some(c) = emit.of("camera").first() {
                assert_eq!(c["action"], "capture");
                assert!(a2.camera_frames(c["id"].as_str().unwrap(), vec!["QUJDRA==".into()], None));
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    a.run_turn("what is in my hand", "voice", &Cancel::new());
    let second = &h.chat.calls()[1].messages;
    let image = second
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .find(|p| p["type"] == "image_url")
        .expect("the frame reached the model request");
    assert_eq!(image["image_url"]["url"], "data:image/jpeg;base64,QUJDRA==");
    // Not on disk, and not kept in memory once the turn is over.
    for entry in std::fs::read_dir(h.svc.conversations_dir())
        .unwrap()
        .flatten()
    {
        let text = std::fs::read_to_string(entry.path()).unwrap();
        assert!(
            !text.contains("QUJDRA") && !text.contains("image_url"),
            "{:?}",
            entry.path()
        );
    }
    assert!(
        !h.home.join("tasks.json").exists()
            || !std::fs::read_to_string(h.home.join("tasks.json"))
                .unwrap()
                .contains("QUJD")
    );
}

#[test]
fn the_camera_times_out_and_reports_errors() {
    let h = Harness::new(
        "camtimeout",
        vec![
            calling(vec![call("1", "camera_look", json!({}))]),
            saying("x"),
        ],
    );
    h.camera.store(true, Ordering::SeqCst);
    turn(&h, "look");
    assert!(tool_result(&h, "1").contains("the camera did not answer"));
    assert_eq!(Limits::default().camera_wait, Duration::from_secs(10));

    let h = Harness::new(
        "camerror",
        vec![
            calling(vec![call("1", "camera_look", json!({}))]),
            saying("x"),
        ],
    );
    h.camera.store(true, Ordering::SeqCst);
    let a = h.assistant();
    let (a2, emit) = (a.clone(), h.emit.clone());
    std::thread::spawn(move || {
        emit.wait_for(|e| !e.of("camera").is_empty());
        let id = emit.of("camera")[0]["id"].as_str().unwrap().to_string();
        a2.camera_frames(&id, vec![], Some("permission denied".into()));
    });
    a.run_turn("look", "voice", &Cancel::new());
    assert!(tool_result(&h, "1").contains("permission denied"));
}

// ------------------------------------------------------------ bounds, persistence

#[test]
fn tool_iterations_and_time_are_bounded() {
    let looping: Vec<Script> = (0..10)
        .map(|i| calling(vec![call(&format!("c{i}"), "observe", json!({}))]))
        .collect();
    let h = Harness::with(
        "bound",
        FakeChat::scripted(looping),
        Limits {
            max_iterations: 3,
            ..Limits::default()
        },
    );
    turn(&h, "loop");
    assert_eq!(h.chat.calls().len(), 3);
    assert!(h.emit.of("reply")[0]["text"]
        .as_str()
        .unwrap()
        .contains("all the steps"));

    let h = Harness::with(
        "time",
        FakeChat::scripted(vec![saying("never asked")]),
        Limits {
            turn_time: Duration::ZERO,
            ..Limits::default()
        },
    );
    turn(&h, "slow");
    assert!(h.chat.calls().is_empty());
    assert!(h.emit.of("reply")[0]["text"]
        .as_str()
        .unwrap()
        .contains("longer than I am allowed"));
}

#[test]
fn conversations_persist_and_a_new_one_starts_clean() {
    let h = Harness::new(
        "persist",
        vec![saying("one"), saying("two"), saying("three")],
    );
    let a = h.assistant();
    a.run_turn("first", "typed", &Cancel::new());
    assert!(a.messages_len() >= 2);
    // A restart continues the same talk.
    let again = h.assistant();
    assert_eq!(again.messages_len(), a.messages_len());
    again.run_turn("second", "typed", &Cancel::new());
    let users =
        |c: &super::testing::SeenCall| c.messages.iter().filter(|m| m["role"] == "user").count();
    assert_eq!(users(&h.chat.calls()[1]), 2);
    again.new_conversation();
    again.run_turn("fresh", "typed", &Cancel::new());
    assert_eq!(users(&h.chat.calls()[2]), 1);
    // The old one is still searchable.
    let hits = store::search(&h.svc.conversations_dir(), "first", 5);
    assert_eq!(hits.len(), 1);
}

#[test]
fn the_gate_reads_the_current_request_and_last_spoken_line() {
    let h = Harness::new(
        "gatemem",
        vec![
            calling(vec![call(
                "1",
                "interact",
                json!({ "speech": "Opening mail." }),
            )]),
            saying("x"),
        ],
    );
    let a = h.assistant();
    a.run_turn("open mail", "voice", &Cancel::new());
    // No TypeSafe in the harness: the classifier errors, so the gate interrupts; a correction
    // never reaches it at all.
    assert!(a.gate("hmm what").interrupt);
    assert_eq!(a.gate("no wait").why, "correction");
}

#[test]
fn only_the_assistant_replies_to_confirm_and_camera_ids() {
    let h = Harness::new("ids", vec![]);
    let a = h.assistant();
    assert!(!a.confirm_reply("confirm-1", true));
    assert!(!a.camera_frames("cam-1", vec![], None));
    assert!(a.chat("  ", "typed").is_err());
}
