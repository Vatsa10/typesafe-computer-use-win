//! A headless Chrome as a Pointer desktop. TEST ONLY.
//!
//! Implements `perception::Desktop` and `actions::Desktop` over the Chrome DevTools Protocol, so
//! decide, the run loop, forms and goal pursuit run unchanged against a page nobody can see.
//!
//! Nothing here may reach the user's screen: Chrome is started with `--headless=new`, its own
//! throwaway `--user-data-dir`, and a debugging port on 127.0.0.1 that Chrome picks itself. The
//! user's Chrome, profile, mouse and keyboard are never touched, and only the process spawned here
//! is ever killed (on drop).
//!
//! Coordinates: the page is a fixed 1920x1080 viewport shown as one fake monitor whose top-left
//! sits at `origin` on the "virtual desktop". Hits and window rectangles are virtual-desktop
//! points (viewport + origin); `click_at` subtracts the origin again. A non-zero origin exercises
//! the same origin math the real multi-monitor path needs.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use platform::capture::Shot;
use platform::display::Monitor;
use platform::uia::{Hit, PressHandle, Walked};
use platform::winlist::WindowInfo;
use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};
use wcore::actions::{self, Key};
use wcore::models::Field;
use wcore::perception::{self, Line};

pub const VIEWPORT: (u32, u32) = (1920, 1080);
/// The one synthetic window's handle.
pub const WINDOW_HWND: isize = 1;
const QUIET_MS: f64 = 250.0;
const MAX_PICKS: usize = 900;
const OFFSCREEN_CAP: usize = 120;
const MIN_SIDE: f64 = 4.0;

// ---------------------------------------------------------------- the wire

struct Conn {
    ws: WebSocket<TcpStream>,
    next: u64,
}

impl Conn {
    fn open(url: &str) -> Result<Conn, String> {
        let rest = url.strip_prefix("ws://").ok_or("only ws:// endpoints")?;
        let host = rest.split('/').next().unwrap_or("");
        if !(host.starts_with("127.0.0.1:") || host.starts_with("localhost:")) {
            return Err("the debugging endpoint must be on 127.0.0.1".into());
        }
        let stream = TcpStream::connect(host).map_err(|e| format!("connect: {e}"))?;
        let _ = stream.set_nodelay(true);
        let (ws, _) = tungstenite::client(url, stream).map_err(|e| format!("handshake: {e}"))?;
        Ok(Conn { ws, next: 1 })
    }

    fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<u64, String> {
        let id = self.next;
        self.next += 1;
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.ws
            .send(Message::text(msg.to_string()))
            .map_err(|e| format!("send {method}: {e}"))?;
        Ok(id)
    }

    /// One message, or None after `wait` with nothing to read. Err when the socket is gone.
    fn read(&mut self, wait: Duration) -> Result<Option<Value>, String> {
        let _ = self
            .ws
            .get_ref()
            .set_read_timeout(Some(wait.max(Duration::from_millis(1))));
        match self.ws.read() {
            Ok(Message::Text(t)) => Ok(serde_json::from_str(t.as_str()).ok()),
            Ok(Message::Close(_)) => Err("the browser closed the connection".into()),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(format!("read: {e}")),
        }
    }
}

// ---------------------------------------------------------------- the accessibility tree

/// One accessibility node, as Chrome reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AxNode {
    pub id: String,
    pub parent: Option<String>,
    pub role: String,
    pub name: String,
    pub value: String,
    pub backend: Option<u64>,
    pub checked: bool,
    pub disabled: bool,
    pub multiline: bool,
}

fn text_of(v: &Value) -> String {
    match &v["value"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// The nodes of an `Accessibility.getFullAXTree` result, ignored nodes dropped.
pub fn parse_ax(result: &Value) -> Vec<AxNode> {
    let mut nodes: Vec<AxNode> = Vec::new();
    let mut child_of: HashMap<String, String> = HashMap::new();
    for n in result["nodes"].as_array().into_iter().flatten() {
        let id = n["nodeId"].as_str().unwrap_or("").to_string();
        for c in n["childIds"].as_array().into_iter().flatten() {
            if let Some(c) = c.as_str() {
                child_of.insert(c.to_string(), id.clone());
            }
        }
        if n["ignored"].as_bool().unwrap_or(false) {
            continue;
        }
        let mut node = AxNode {
            id,
            parent: n["parentId"].as_str().map(str::to_string),
            role: text_of(&n["role"]),
            name: text_of(&n["name"]).trim().to_string(),
            value: text_of(&n["value"]),
            backend: n["backendDOMNodeId"].as_u64(),
            ..Default::default()
        };
        for p in n["properties"].as_array().into_iter().flatten() {
            let v = text_of(&p["value"]);
            match p["name"].as_str().unwrap_or("") {
                "checked" => node.checked = v == "true",
                "disabled" => node.disabled = v == "true",
                "multiline" => node.multiline = v == "true",
                _ => {}
            }
        }
        nodes.push(node);
    }
    for n in &mut nodes {
        if n.parent.is_none() {
            n.parent = child_of.get(&n.id).cloned();
        }
    }
    nodes
}

/// The AX role vocabulary Pointer speaks, for an ARIA/Chrome role.
pub fn ax_role(role: &str, multiline: bool) -> Option<&'static str> {
    Some(match role {
        "button" => "AXButton",
        "link" => "AXLink",
        "textbox" => {
            if multiline {
                "AXTextArea"
            } else {
                "AXTextField"
            }
        }
        "searchbox" => "AXSearchField",
        "spinbutton" => "AXTextField",
        "combobox" | "listbox" | "PopUpButton" | "MenuListPopup" => "AXComboBox",
        "checkbox" | "switch" | "menuitemcheckbox" => "AXCheckBox",
        "radio" | "menuitemradio" => "AXRadioButton",
        "slider" => "AXSlider",
        "tab" => "AXTab",
        "menuitem" => "AXMenuBarItem",
        "option" | "listitem" | "cell" | "gridcell" | "columnheader" | "rowheader" => "AXCell",
        "treeitem" => "AXRow",
        "img" | "image" => "AXImage",
        "heading" | "StaticText" => "AXStaticText",
        _ => return None,
    })
}

/// A node chosen to become a Pointer item.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub node: usize,
    pub role: &'static str,
    pub label: String,
    pub pressable: bool,
    pub text_node: bool,
    /// An option of a select: borrows the select's box.
    pub option_of: Option<usize>,
}

fn is_text_role(r: &str) -> bool {
    matches!(r, "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox")
}

/// The nodes worth offering. `focused` is the backend id of the focused element: a dropdown's
/// options are offered only while its select holds focus, as an opened list would be.
pub fn select_nodes(nodes: &[AxNode], focused: Option<u64>) -> Vec<Pick> {
    let by_id: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    let ancestor_select = |mut i: usize| -> Option<usize> {
        for _ in 0..4 {
            let p = nodes[i].parent.as_deref().and_then(|p| by_id.get(p).copied())?;
            if nodes[p].role == "combobox" || nodes[p].role == "listbox" {
                return Some(p);
            }
            i = p;
        }
        None
    };
    let mut out = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        let Some(role) = ax_role(&n.role, n.multiline) else {
            continue;
        };
        let Some(_) = n.backend else { continue };
        if n.role == "MenuListPopup" {
            continue;
        }
        let mut option_of = None;
        if n.role == "option" {
            match ancestor_select(i) {
                Some(s) if nodes[s].backend.is_some() && nodes[s].backend == focused => {
                    option_of = Some(s)
                }
                Some(_) => continue,
                None => {}
            }
        }
        let text_node = n.role == "StaticText";
        if text_node {
            let parent = n.parent.as_deref().and_then(|p| by_id.get(p)).map(|&p| &nodes[p]);
            if n.name.is_empty() || parent.is_some_and(|p| p.name.contains(&n.name)) {
                continue;
            }
        }
        let mut label = n.name.clone();
        if is_text_role(role) {
            let v = n.value.trim();
            if !v.is_empty() && v != label {
                label = if label.is_empty() { v.to_string() } else { format!("{label}: {v}") };
            }
        } else if matches!(role, "AXCheckBox" | "AXRadioButton") && n.checked && !label.is_empty() {
            label.push_str(" (checked)");
        }
        if label.is_empty() {
            continue;
        }
        let pressable = !n.disabled
            && matches!(
                role,
                "AXButton" | "AXLink" | "AXCheckBox" | "AXRadioButton" | "AXTab"
                    | "AXMenuBarItem" | "AXComboBox" | "AXCell" | "AXRow"
            )
            && !text_node;
        out.push(Pick { node: i, role, label, pressable, text_node, option_of });
        if out.len() >= MAX_PICKS {
            break;
        }
    }
    // Text that only repeats a control's name or value (a field's label, the echo of what was
    // typed) adds items, not information.
    let named: HashSet<String> = out
        .iter()
        .filter(|p| !p.text_node)
        .flat_map(|p| {
            let n = &nodes[p.node];
            [n.name.trim_end_matches(" *").trim().to_lowercase(), n.value.trim().to_lowercase()]
        })
        .filter(|s| !s.is_empty())
        .collect();
    out.retain(|p| {
        !p.text_node || !named.contains(p.label.trim_end_matches(" *").trim().to_lowercase().as_str())
    });
    out
}

/// The bounding box of the quads in a box-model or content-quads result: x, y, w, h.
pub fn bbox_of(result: &Value) -> Option<(f64, f64, f64, f64)> {
    let mut quads: Vec<Vec<f64>> = Vec::new();
    if let Some(b) = result["model"]["border"].as_array() {
        quads.push(b.iter().filter_map(Value::as_f64).collect());
    }
    for q in result["quads"].as_array().into_iter().flatten() {
        quads.push(q.as_array()?.iter().filter_map(Value::as_f64).collect());
    }
    let (mut x1, mut y1, mut x2, mut y2) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for q in quads.iter().filter(|q| q.len() >= 8) {
        for p in q.chunks(2) {
            x1 = x1.min(p[0]);
            x2 = x2.max(p[0]);
            y1 = y1.min(p[1]);
            y2 = y2.max(p[1]);
        }
    }
    (x1 <= x2 && y1 <= y2).then_some((x1, y1, x2 - x1, y2 - y1))
}

/// Picks plus their boxes as hits in virtual-desktop points, split into on-screen and off-screen.
/// `boxes[i]` is the viewport box of `picks[i]` when it has one.
pub fn build_hits(
    picks: &[Pick],
    nodes: &[AxNode],
    boxes: &[Option<(f64, f64, f64, f64)>],
    origin: (f64, f64),
    view: (f64, f64),
    mk: &dyn Fn(u64) -> Option<PressHandle>,
) -> Walked {
    let by_node: HashMap<usize, usize> = picks.iter().enumerate().map(|(i, p)| (p.node, i)).collect();
    let mut walked = Walked { on_screen: Vec::new(), off_screen: Vec::new(), capped: false };
    for (i, p) in picks.iter().enumerate() {
        let b = boxes.get(i).copied().flatten().or_else(|| {
            let s = by_node.get(&p.option_of?)?;
            boxes.get(*s).copied().flatten()
        });
        let Some((x, y, w, h)) = b else { continue };
        if w < 1.0 || h < 1.0 {
            continue;
        }
        let Some(handle) = nodes[p.node].backend.and_then(mk) else { continue };
        let visible = x + w > 0.0 && y + h > 0.0 && x < view.0 && y < view.1;
        let hit = Hit {
            role: p.role.to_string(),
            label: p.label.clone(),
            x: x + origin.0,
            y: y + origin.1,
            w,
            h,
            pressable: p.pressable,
            handle,
        };
        if visible && w >= MIN_SIDE && h >= MIN_SIDE {
            walked.on_screen.push(hit);
        } else if !visible && walked.off_screen.len() < OFFSCREEN_CAP && !p.text_node {
            walked.off_screen.push(hit);
        }
    }
    walked
}

// ---------------------------------------------------------------- page scripts

const OBSERVER: &str = "(()=>{if(window.__pm)return;window.__pm={n:0,t:performance.now()};\
const o=new MutationObserver(()=>{window.__pm.n++;window.__pm.t=performance.now()});\
o.observe(document,{subtree:true,childList:true,attributes:true,characterData:true});})()";

const JS_PRESS: &str = "function(){const el=this;if(el.disabled)return false;\
if(el.tagName==='OPTION'){const s=el.closest('select');if(!s)return false;s.value=el.value;\
s.dispatchEvent(new Event('input',{bubbles:true}));s.dispatchEvent(new Event('change',{bubbles:true}));s.blur();return true;}\
if(el.tagName==='SELECT'){el.focus();return true;}\
if(el.scrollIntoViewIfNeeded)el.scrollIntoViewIfNeeded();if(el.focus)el.focus();el.click();return true;}";

const JS_PREPARE: &str = "function(){const el=this,t=el.tagName;\
if(t==='SELECT')return 'select';\
if(t==='TEXTAREA'||(t==='INPUT'&&!['checkbox','radio','file','button','submit','range','color'].includes(el.type))){el.focus();\
try{el.select()}catch(e){}return el.type==='date'||el.type==='time'||el.type==='number'||el.type==='month'||el.type==='week'?'setter':'text';}\
if(el.isContentEditable){el.focus();return 'editable';}return 'none';}";

const JS_SETTER: &str = "function(v){const el=this,t=el.tagName;\
if(t==='SELECT'){const o=[...el.options],q=String(v).trim().toLowerCase();\
const m=o.find(x=>x.text.trim().toLowerCase()===q||x.value.toLowerCase()===q)||o.find(x=>x.text.trim().toLowerCase().startsWith(q));\
if(!m)return false;el.value=m.value;el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));return true;}\
if(t==='INPUT'||t==='TEXTAREA'){const d=Object.getOwnPropertyDescriptor(t==='INPUT'?HTMLInputElement.prototype:HTMLTextAreaElement.prototype,'value');\
if(!d||!d.set)return false;d.set.call(el,String(v));\
el.dispatchEvent(new InputEvent('input',{bubbles:true,inputType:'insertText',data:String(v)}));\
el.dispatchEvent(new Event('change',{bubbles:true}));return el.value===String(v);}\
if(el.isContentEditable){el.textContent=String(v);el.dispatchEvent(new InputEvent('input',{bubbles:true}));return true;}return false;}";

const JS_VALUE: &str = "function(){const el=this;\
if(el.tagName==='SELECT'){const o=el.selectedOptions[0];return o?o.text:'';}\
if('value' in el)return String(el.value);return el.textContent||'';}";

const JS_CHANGE: &str = "function(){this.dispatchEvent(new Event('change',{bubbles:true}));return true;}";

const JS_FIELD: &str = "(()=>{const e=document.activeElement;if(!e||e===document.body||e===document.documentElement)return null;\
const t=e.tagName,ty=(e.type||'').toLowerCase();let role=null;\
if(t==='TEXTAREA')role='AXTextArea';else if(t==='SELECT')role='AXComboBox';\
else if(t==='INPUT'&&!['checkbox','radio','button','submit','file','range','color','image','reset'].includes(ty))role=ty==='search'?'AXSearchField':'AXTextField';\
else if(e.isContentEditable)role='AXTextArea';\
if(!role)return null;const r=e.getBoundingClientRect();\
const l=(e.labels&&e.labels[0]&&e.labels[0].innerText)||e.getAttribute('aria-label')||e.placeholder||e.name||e.id||'';\
return {role,label:l.trim(),placeholder:e.placeholder||'',value:t==='SELECT'?(e.selectedOptions[0]?e.selectedOptions[0].text:''):String(e.value??e.textContent??''),x:r.x,y:r.y,w:r.width,h:r.height};})()";

const JS_ACTIVE: &str = "(document.activeElement&&document.activeElement!==document.body&&document.activeElement!==document.documentElement)?document.activeElement:null";

/// What a settle found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settled {
    /// True when the page went quiet, false when the time ran out first.
    pub quiet: bool,
    pub seconds: f64,
}

#[derive(Debug, Clone, Default)]
struct Tab {
    id: String,
    title: String,
    url: String,
    session: Option<String>,
}

struct Inner {
    conn: Conn,
    dead: Option<String>,
    session: String,
    tabs: Vec<Tab>,
    active: String,
    inflight: HashSet<String>,
    last_net: Instant,
    opened: Option<String>,
    dialog: Option<String>,
    mouse: (f64, f64),
}

type R<T> = Result<T, String>;

impl Inner {
    fn absorb(&mut self, ev: &Value) {
        let Some(method) = ev["method"].as_str() else { return };
        let p = &ev["params"];
        let session = ev["sessionId"].as_str().unwrap_or("");
        match method {
            "Network.requestWillBeSent" if session == self.session => {
                let ty = p["type"].as_str().unwrap_or("");
                let url = p["request"]["url"].as_str().unwrap_or("");
                if !matches!(ty, "WebSocket" | "EventSource" | "Ping" | "Prefetch")
                    && !url.starts_with("data:")
                {
                    if let Some(id) = p["requestId"].as_str() {
                        self.inflight.insert(id.to_string());
                    }
                }
                self.last_net = Instant::now();
            }
            "Network.loadingFinished" | "Network.loadingFailed" if session == self.session => {
                if let Some(id) = p["requestId"].as_str() {
                    self.inflight.remove(id);
                }
                self.last_net = Instant::now();
            }
            "Page.javascriptDialogOpening" if session == self.session => {
                self.dialog = Some(session.to_string());
            }
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                let t = &p["targetInfo"];
                if t["type"].as_str() != Some("page") {
                    return;
                }
                let id = t["targetId"].as_str().unwrap_or("").to_string();
                let (title, url) = (
                    t["title"].as_str().unwrap_or("").to_string(),
                    t["url"].as_str().unwrap_or("").to_string(),
                );
                if let Some(tab) = self.tabs.iter_mut().find(|x| x.id == id) {
                    tab.title = title;
                    tab.url = url;
                } else {
                    let opener = t["openerId"].as_str().is_some_and(|o| !o.is_empty());
                    self.tabs.push(Tab { id: id.clone(), title, url, session: None });
                    if opener && method == "Target.targetCreated" {
                        self.opened = Some(id);
                    }
                }
            }
            "Target.targetDestroyed" => {
                let id = p["targetId"].as_str().unwrap_or("");
                self.tabs.retain(|t| t.id != id);
            }
            _ => {}
        }
    }

    fn after_message(&mut self) {
        if let Some(s) = self.dialog.take() {
            let _ = self.conn.send("Page.handleJavaScriptDialog", json!({"accept": true}), Some(&s));
        }
    }

    fn call_in(&mut self, method: &str, params: Value, session: Option<String>, limit: Duration) -> R<Value> {
        if let Some(d) = &self.dead {
            return Err(d.clone());
        }
        let id = self.conn.send(method, params, session.as_deref()).inspect_err(|e| self.dead = Some(e.clone()))?;
        let end = Instant::now() + limit;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("{method} timed out"));
            }
            match self.conn.read(left.min(Duration::from_millis(200))) {
                Err(e) => {
                    self.dead = Some(e.clone());
                    return Err(e);
                }
                Ok(None) => {}
                Ok(Some(m)) => {
                    if m["id"].as_u64() == Some(id) {
                        self.after_message();
                        if let Some(e) = m.get("error") {
                            return Err(format!("{method}: {}", e["message"].as_str().unwrap_or("error")));
                        }
                        return Ok(m["result"].clone());
                    }
                    self.absorb(&m);
                    self.after_message();
                }
            }
        }
    }

    fn cdp(&mut self, method: &str, params: Value) -> R<Value> {
        let s = self.session.clone();
        self.call_in(method, params, Some(s), Duration::from_secs(30))
    }

    /// Several requests sent together, answered in order: one round trip for the lot.
    fn cdp_many(&mut self, reqs: Vec<(&str, Value)>) -> Vec<R<Value>> {
        let session = self.session.clone();
        let mut ids = Vec::new();
        for (m, p) in &reqs {
            match self.conn.send(m, p.clone(), Some(&session)) {
                Ok(id) => ids.push(Ok(id)),
                Err(e) => ids.push(Err(e)),
            }
        }
        let mut got: HashMap<u64, Value> = HashMap::new();
        let want = ids.iter().filter(|i| i.is_ok()).count();
        let end = Instant::now() + Duration::from_secs(30);
        while got.len() < want && Instant::now() < end {
            match self.conn.read(Duration::from_millis(200)) {
                Err(e) => {
                    self.dead = Some(e);
                    break;
                }
                Ok(None) => {}
                Ok(Some(m)) => {
                    if let Some(id) = m["id"].as_u64() {
                        got.insert(id, m);
                    } else {
                        self.absorb(&m);
                    }
                    self.after_message();
                }
            }
        }
        ids.into_iter()
            .map(|id| {
                let id = id?;
                let m = got.remove(&id).ok_or("no answer")?;
                match m.get("error") {
                    Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                    None => Ok(m["result"].clone()),
                }
            })
            .collect()
    }

    fn pump(&mut self, wait: Duration) {
        let end = Instant::now() + wait;
        while Instant::now() < end {
            match self.conn.read(end.saturating_duration_since(Instant::now()).min(Duration::from_millis(50))) {
                Ok(Some(m)) => {
                    self.absorb(&m);
                    self.after_message();
                }
                Ok(None) => {}
                Err(e) => {
                    self.dead = Some(e);
                    return;
                }
            }
        }
    }

    fn eval(&mut self, expr: &str) -> R<Value> {
        let r = self.cdp("Runtime.evaluate", json!({"expression": expr, "returnByValue": true}))?;
        if let Some(ex) = r.get("exceptionDetails") {
            return Err(format!("page script: {}", ex["text"].as_str().unwrap_or("error")));
        }
        Ok(r["result"]["value"].clone())
    }

    fn enable_page(&mut self) -> R<()> {
        self.cdp("Page.enable", json!({}))?;
        self.cdp("Network.enable", json!({}))?;
        self.cdp("DOM.enable", json!({}))?;
        let _ = self.cdp("Accessibility.enable", json!({}));
        self.cdp(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": VIEWPORT.0, "height": VIEWPORT.1, "deviceScaleFactor": 1, "mobile": false}),
        )?;
        let _ = self.cdp("Emulation.setFocusEmulationEnabled", json!({"enabled": true}));
        self.cdp("Page.addScriptToEvaluateOnNewDocument", json!({"source": OBSERVER}))?;
        let _ = self.eval(OBSERVER);
        Ok(())
    }

    fn attach(&mut self, target: &str) -> R<()> {
        let existing = self.tabs.iter().find(|t| t.id == target).and_then(|t| t.session.clone());
        let session = match existing {
            Some(s) => s,
            None => {
                let r = self.call_in(
                    "Target.attachToTarget",
                    json!({"targetId": target, "flatten": true}),
                    None,
                    Duration::from_secs(15),
                )?;
                let s = r["sessionId"].as_str().ok_or("no session")?.to_string();
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == target) {
                    t.session = Some(s.clone());
                }
                self.session = s.clone();
                self.active = target.to_string();
                self.inflight.clear();
                self.enable_page()?;
                return Ok(());
            }
        };
        self.session = session;
        self.active = target.to_string();
        self.inflight.clear();
        let _ = self.cdp("Page.bringToFront", json!({}));
        Ok(())
    }

    fn follow_new_tab(&mut self) {
        if let Some(id) = self.opened.take() {
            let _ = self.attach(&id);
        }
    }

    fn settle(&mut self, max: Duration) -> Settled {
        let start = Instant::now();
        loop {
            self.pump(Duration::from_millis(40));
            self.follow_new_tab();
            let net_quiet = self.inflight.is_empty() && self.last_net.elapsed().as_secs_f64() * 1000.0 >= QUIET_MS;
            if net_quiet {
                let q = self.eval(
                    "document.readyState==='complete'?(window.__pm?performance.now()-window.__pm.t:1e9):-1",
                );
                if q.ok().and_then(|v| v.as_f64()).is_some_and(|ms| ms >= QUIET_MS) {
                    return Settled { quiet: true, seconds: start.elapsed().as_secs_f64() };
                }
            }
            if start.elapsed() >= max || self.dead.is_some() {
                return Settled { quiet: false, seconds: start.elapsed().as_secs_f64() };
            }
        }
    }

    fn object_of(&mut self, backend: u64) -> R<String> {
        let r = self.cdp("DOM.resolveNode", json!({"backendNodeId": backend}))?;
        r["object"]["objectId"].as_str().map(str::to_string).ok_or_else(|| "node is gone".into())
    }

    fn on_node(&mut self, backend: u64, func: &str, arg: Option<&str>) -> R<Value> {
        let oid = self.object_of(backend)?;
        let args: Vec<Value> = arg.map(|a| json!({"value": a})).into_iter().collect();
        let r = self.cdp(
            "Runtime.callFunctionOn",
            json!({"objectId": oid, "functionDeclaration": func, "arguments": args, "returnByValue": true, "awaitPromise": false}),
        );
        let _ = self.cdp("Runtime.releaseObject", json!({"objectId": oid}));
        let r = r?;
        if let Some(ex) = r.get("exceptionDetails") {
            return Err(format!("page script: {}", ex["text"].as_str().unwrap_or("error")));
        }
        Ok(r["result"]["value"].clone())
    }

    fn active_backend(&mut self) -> Option<u64> {
        let r = self.cdp("Runtime.evaluate", json!({"expression": JS_ACTIVE})).ok()?;
        let oid = r["result"]["objectId"].as_str()?.to_string();
        let d = self.cdp("DOM.describeNode", json!({"objectId": oid}));
        let _ = self.cdp("Runtime.releaseObject", json!({"objectId": oid}));
        d.ok()?["node"]["backendNodeId"].as_u64()
    }

    fn mouse(&mut self, kind: &str, x: f64, y: f64, extra: Value) {
        let mut p = json!({"type": kind, "x": x, "y": y, "button": "none", "modifiers": 0});
        if let (Some(o), Some(e)) = (p.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        let _ = self.cdp("Input.dispatchMouseEvent", p);
    }
}

// ---------------------------------------------------------------- the desktop

/// Where Chrome or Edge lives, or None.
pub fn find_browser() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CHROME_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Ok(v) = std::env::var(var) {
            roots.push(PathBuf::from(v));
        }
    }
    let rels = [
        "Google/Chrome/Application/chrome.exe",
        "Microsoft/Edge/Application/msedge.exe",
    ];
    for rel in rels {
        for r in &roots {
            let p = r.join(rel);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Headless Chrome behind Pointer's two desktop traits.
pub struct HeadlessDesktop {
    inner: RefCell<Inner>,
    origin: (i32, i32),
    pid: u32,
    guard: Option<Guard>,
}

/// Owns the spawned browser: kills that process and removes its throwaway profile on drop.
struct Guard {
    child: Option<Child>,
    dir: Option<PathBuf>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(d) = self.dir.take() {
            for _ in 0..10 {
                if std::fs::remove_dir_all(&d).is_ok() || !d.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

impl HeadlessDesktop {
    /// Start a headless Chrome of its own and attach to its first tab. `origin` is where the fake
    /// monitor sits on the virtual desktop; pass (0, 0) normally and something else to test the
    /// origin math.
    pub fn launch(origin: (i32, i32)) -> R<Self> {
        let exe = find_browser().ok_or("no Chrome or Edge found (set CHROME_PATH)")?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("pointer-headless-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&exe);
        cmd.arg("--headless=new")
            .arg("--remote-debugging-port=0")
            .arg("--remote-debugging-address=127.0.0.1")
            .arg("--remote-allow-origins=*")
            .arg(format!("--user-data-dir={}", dir.display()))
            .arg(format!("--window-size={},{}", VIEWPORT.0, VIEWPORT.1))
            .args([
                "--force-device-scale-factor=1",
                "--hide-scrollbars",
                "--mute-audio",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-background-networking",
                "--disable-gpu",
                "about:blank",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let child = cmd.spawn().map_err(|e| format!("could not start the browser: {e}"))?;
        let pid = child.id();
        // From here the guard owns the process: any early return kills it and clears the profile.
        let guard = Guard { child: Some(child), dir: Some(dir.clone()) };
        // Chrome writes the port it chose into the profile folder.
        let end = Instant::now() + Duration::from_secs(25);
        let ws = loop {
            if let Ok(t) = std::fs::read_to_string(dir.join("DevToolsActivePort")) {
                let mut l = t.lines();
                if let (Some(port), Some(path)) = (l.next(), l.next()) {
                    break format!("ws://127.0.0.1:{}{}", port.trim(), path.trim());
                }
            }
            if Instant::now() > end {
                return Err("the browser did not open its debugging port".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let mut d = Self::connect(&ws, origin, pid)?;
        d.guard = Some(guard);
        Ok(d)
    }

    /// Attach to a browser endpoint already running (a fake in tests). Does not own a process.
    pub fn connect(ws_url: &str, origin: (i32, i32), pid: u32) -> R<Self> {
        let conn = Conn::open(ws_url)?;
        let mut inner = Inner::new(conn);
        inner.call_in("Target.setDiscoverTargets", json!({"discover": true}), None, Duration::from_secs(15))?;
        let r = inner.call_in("Target.getTargets", json!({}), None, Duration::from_secs(15))?;
        for t in r["targetInfos"].as_array().into_iter().flatten() {
            if t["type"].as_str() == Some("page") {
                inner.tabs.push(Tab {
                    id: t["targetId"].as_str().unwrap_or("").into(),
                    title: t["title"].as_str().unwrap_or("").into(),
                    url: t["url"].as_str().unwrap_or("").into(),
                    session: None,
                });
            }
        }
        // targetCreated events for the same tabs may already be queued; keep one entry per id.
        let mut seen = HashSet::new();
        inner.tabs.retain(|t| seen.insert(t.id.clone()));
        let first = match inner.tabs.first() {
            Some(t) => t.id.clone(),
            None => {
                let r = inner.call_in("Target.createTarget", json!({"url": "about:blank"}), None, Duration::from_secs(15))?;
                let id = r["targetId"].as_str().ok_or("no target")?.to_string();
                inner.tabs.push(Tab { id: id.clone(), ..Default::default() });
                id
            }
        };
        inner.attach(&first)?;
        Ok(HeadlessDesktop { inner: RefCell::new(inner), origin, pid, guard: None })
    }

    // ------------------------------------------------------------ public helpers

    /// Wait for the network to go idle and the DOM to stop changing, up to `max_seconds`.
    /// The run loop can call this instead of a fixed sleep.
    pub fn settle(&self, max_seconds: f64) -> Settled {
        self.inner.borrow_mut().settle(Duration::from_secs_f64(max_seconds.max(0.0)))
    }

    /// Navigate the current tab and wait for it to settle.
    pub fn navigate(&self, url: &str) -> R<()> {
        {
            let mut i = self.inner.borrow_mut();
            let r = i.cdp("Page.navigate", json!({"url": url}))?;
            if let Some(e) = r["errorText"].as_str() {
                return Err(format!("navigate: {e}"));
            }
            i.last_net = Instant::now();
        }
        self.settle(15.0);
        Ok(())
    }

    /// The tabs: id, title, url.
    pub fn tabs(&self) -> Vec<(String, String, String)> {
        let mut i = self.inner.borrow_mut();
        i.pump(Duration::from_millis(5));
        i.tabs.iter().map(|t| (t.id.clone(), t.title.clone(), t.url.clone())).collect()
    }

    pub fn new_tab(&self, url: &str) -> R<()> {
        let mut i = self.inner.borrow_mut();
        let r = i.call_in("Target.createTarget", json!({"url": "about:blank"}), None, Duration::from_secs(15))?;
        let id = r["targetId"].as_str().ok_or("no target")?.to_string();
        if !i.tabs.iter().any(|t| t.id == id) {
            i.tabs.push(Tab { id: id.clone(), ..Default::default() });
        }
        i.opened = None;
        i.attach(&id)?;
        drop(i);
        self.navigate(url)
    }

    pub fn switch_tab(&self, index: usize) -> R<()> {
        let mut i = self.inner.borrow_mut();
        let id = i.tabs.get(index).ok_or("no such tab")?.id.clone();
        i.attach(&id)
    }

    pub fn close_tab(&self, index: usize) -> R<()> {
        let mut i = self.inner.borrow_mut();
        let id = i.tabs.get(index).ok_or("no such tab")?.id.clone();
        i.call_in("Target.closeTarget", json!({"targetId": id}), None, Duration::from_secs(10))?;
        i.tabs.retain(|t| t.id != id);
        if i.active == id {
            if let Some(next) = i.tabs.first().map(|t| t.id.clone()) {
                i.attach(&next)?;
            }
        }
        Ok(())
    }

    /// Give a file input files by path (DOM.setFileInputFiles).
    pub fn set_files(&self, handle: &PressHandle, files: &[&Path]) -> bool {
        let list: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
        self.inner
            .borrow_mut()
            .cdp("DOM.setFileInputFiles", json!({"files": list, "backendNodeId": handle.remote_id()}))
            .is_ok()
    }

    /// Run a page script and return its JSON value.
    pub fn eval(&self, expr: &str) -> R<Value> {
        self.inner.borrow_mut().eval(expr)
    }

    /// Tap the space bar (toggles a focused checkbox or radio).
    pub fn press_space(&self) {
        let mut i = self.inner.borrow_mut();
        let down = json!({"type": "keyDown", "key": " ", "code": "Space", "text": " ",
            "windowsVirtualKeyCode": 32, "nativeVirtualKeyCode": 32, "modifiers": 0});
        let up = json!({"type": "keyUp", "key": " ", "code": "Space",
            "windowsVirtualKeyCode": 32, "nativeVirtualKeyCode": 32, "modifiers": 0});
        let _ = i.cdp("Input.dispatchKeyEvent", down);
        let _ = i.cdp("Input.dispatchKeyEvent", up);
        i.pump(Duration::from_millis(30));
    }

    pub fn url(&self) -> String {
        self.inner.borrow_mut().eval("location.href").ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
    }

    pub fn title(&self) -> String {
        self.inner.borrow_mut().eval("document.title").ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
    }

    /// The page's visible text, for a final read.
    pub fn text(&self) -> String {
        self.inner
            .borrow_mut()
            .eval("document.body?document.body.innerText:''")
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    fn handle(id: u64) -> Option<PressHandle> {
        PressHandle::remote(id)
    }
}

impl Inner {
    fn new(conn: Conn) -> Inner {
        Inner {
            conn,
            dead: None,
            session: String::new(),
            tabs: Vec::new(),
            active: String::new(),
            inflight: HashSet::new(),
            last_net: Instant::now(),
            opened: None,
            dialog: None,
            mouse: (VIEWPORT.0 as f64 / 2.0, VIEWPORT.1 as f64 / 2.0),
        }
    }
}

// ---------------------------------------------------------------- perception

impl perception::Desktop for HeadlessDesktop {
    fn monitors(&self) -> Vec<Monitor> {
        vec![Monitor {
            index: 0,
            left: self.origin.0,
            top: self.origin.1,
            right: self.origin.0 + VIEWPORT.0 as i32,
            bottom: self.origin.1 + VIEWPORT.1 as i32,
            primary: true,
        }]
    }

    fn open_windows(&self) -> Vec<WindowInfo> {
        let mut i = self.inner.borrow_mut();
        let title = i.eval("document.title").ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        vec![WindowInfo {
            hwnd: WINDOW_HWND,
            title,
            app: "chrome".into(),
            pid: self.pid,
            rect: (self.origin.0, self.origin.1, self.origin.0 + VIEWPORT.0 as i32, self.origin.1 + VIEWPORT.1 as i32),
            monitor: 0,
            minimized: false,
            foreground: true,
        }]
    }

    fn foreground_hwnd(&self) -> isize {
        WINDOW_HWND
    }

    fn process_name(&self, pid: u32) -> String {
        if pid == self.pid { "chrome".into() } else { String::new() }
    }

    fn screenshot(&self, _monitor: Option<&Monitor>) -> Option<Shot> {
        let r = self
            .inner
            .borrow_mut()
            .cdp("Page.captureScreenshot", json!({"format": "png", "fromSurface": true}))
            .ok()?;
        let png = base64::engine::general_purpose::STANDARD.decode(r["data"].as_str()?).ok()?;
        let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).ok()?.to_rgba8();
        let (width, height) = img.dimensions();
        let mut bgra = img.into_raw();
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Some(Shot { width, height, bgra, origin: self.origin })
    }

    fn focused_field(&self) -> Option<Field> {
        let v = self.inner.borrow_mut().eval(JS_FIELD).ok()?;
        if v.is_null() {
            return None;
        }
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
        Some(Field {
            role: s("role"),
            label: s("label"),
            placeholder: s("placeholder"),
            value: s("value"),
            x: f("x") + self.origin.0 as f64,
            y: f("y") + self.origin.1 as f64,
            w: f("w"),
            h: f("h"),
        })
    }

    fn browser_url(&self, _hwnd: isize, _display: (f64, f64), _origin: (f64, f64)) -> Option<String> {
        Some(self.url()).filter(|u| !u.is_empty())
    }

    /// The AX tree names every control, so no pixels are read.
    fn ocr(&self, _bgra: &[u8], _width: u32, _height: u32) -> Vec<Line> {
        Vec::new()
    }

    fn walk(&self, _hwnd: isize, display_w: f64, display_h: f64, origin: (f64, f64)) -> Walked {
        let mut i = self.inner.borrow_mut();
        let Ok(tree) = i.cdp("Accessibility.getFullAXTree", json!({})) else {
            return Walked { on_screen: vec![], off_screen: vec![], capped: false };
        };
        let nodes = parse_ax(&tree);
        let focused = i.active_backend();
        let picks = select_nodes(&nodes, focused);
        let mut reqs: Vec<(&str, Value)> = Vec::new();
        let mut slot: Vec<Option<usize>> = Vec::new();
        for p in &picks {
            let b = nodes[p.node].backend.unwrap_or(0);
            if p.option_of.is_some() {
                slot.push(None);
            } else if p.text_node {
                slot.push(Some(reqs.len()));
                reqs.push(("DOM.getContentQuads", json!({"backendNodeId": b})));
            } else {
                slot.push(Some(reqs.len()));
                reqs.push(("DOM.getBoxModel", json!({"backendNodeId": b})));
            }
        }
        let answers = i.cdp_many(reqs);
        let boxes: Vec<Option<(f64, f64, f64, f64)>> = slot
            .iter()
            .map(|s| s.and_then(|k| answers.get(k)).and_then(|r| r.as_ref().ok()).and_then(bbox_of))
            .collect();
        let _ = (display_w, display_h);
        build_hits(&picks, &nodes, &boxes, origin, (VIEWPORT.0 as f64, VIEWPORT.1 as f64), &Self::handle)
    }
}

// ---------------------------------------------------------------- actions

fn key_parts(key: Key) -> (&'static str, &'static str, u32, Option<&'static str>) {
    match key {
        Key::Return => ("Enter", "Enter", 13, Some("\r")),
        Key::Tab => ("Tab", "Tab", 9, None),
        Key::Escape => ("Escape", "Escape", 27, None),
        Key::A => ("a", "KeyA", 65, None),
        Key::Delete => ("Delete", "Delete", 46, None),
        Key::Back => ("Backspace", "Backspace", 8, None),
    }
}

impl actions::Desktop for HeadlessDesktop {
    type Handle = PressHandle;

    fn ax_press(&self, h: &PressHandle) -> bool {
        let mut i = self.inner.borrow_mut();
        let ok = i.on_node(h.remote_id(), JS_PRESS, None).map(|v| v.as_bool().unwrap_or(false)).unwrap_or(false);
        if ok {
            i.pump(Duration::from_millis(30));
            i.follow_new_tab();
        }
        ok
    }

    fn ax_focus(&self, h: &PressHandle) -> bool {
        self.inner
            .borrow_mut()
            .on_node(h.remote_id(), "function(){this.focus();return document.activeElement===this;}", None)
            .map(|v| v.as_bool().unwrap_or(false))
            .unwrap_or(false)
    }

    fn ax_set_value(&self, h: &PressHandle, text: &str) -> bool {
        let mut i = self.inner.borrow_mut();
        let id = h.remote_id();
        let kind = i.on_node(id, JS_PREPARE, None).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        if kind == "text" || kind == "editable" {
            // Real input events: the page sees a user typing.
            if i.cdp("Input.insertText", json!({"text": text})).is_ok() {
                let back = i.on_node(id, JS_VALUE, None).ok().and_then(|v| v.as_str().map(str::to_string));
                if back.as_deref().is_some_and(|b| b == text || (kind == "editable" && b.contains(text))) {
                    let _ = i.on_node(id, JS_CHANGE, None);
                    return true;
                }
            }
        }
        if kind == "none" || kind.is_empty() {
            return false;
        }
        i.on_node(id, JS_SETTER, Some(text)).map(|v| v.as_bool().unwrap_or(false)).unwrap_or(false)
    }

    fn ax_value(&self, h: &PressHandle) -> Option<String> {
        self.inner.borrow_mut().on_node(h.remote_id(), JS_VALUE, None).ok()?.as_str().map(str::to_string)
    }

    fn focused_handle(&self) -> Option<PressHandle> {
        let id = self.inner.borrow_mut().active_backend()?;
        Self::handle(id)
    }

    fn focused_field(&self) -> Option<Field> {
        perception::Desktop::focused_field(self)
    }

    fn click_at(&self, point: (f64, f64)) {
        let (x, y) = (point.0 - self.origin.0 as f64, point.1 - self.origin.1 as f64);
        let mut i = self.inner.borrow_mut();
        i.mouse = (x, y);
        i.mouse("mouseMoved", x, y, json!({}));
        i.mouse("mousePressed", x, y, json!({"button": "left", "buttons": 1, "clickCount": 1}));
        i.mouse("mouseReleased", x, y, json!({"button": "left", "buttons": 0, "clickCount": 1}));
        i.pump(Duration::from_millis(30));
        i.follow_new_tab();
    }

    fn type_text(&self, text: &str) {
        let mut i = self.inner.borrow_mut();
        // A focused dropdown cannot take text: choose the option that starts with it.
        let picked = i
            .eval(&format!(
                "(()=>{{const e=document.activeElement;if(!e||e.tagName!=='SELECT')return false;const q={}.toLowerCase();\
const m=[...e.options].find(o=>o.text.trim().toLowerCase().startsWith(q));if(!m)return false;e.value=m.value;\
e.dispatchEvent(new Event('input',{{bubbles:true}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));return true;}})()",
                Value::String(text.to_string())
            ))
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !picked {
            let _ = i.cdp("Input.insertText", json!({"text": text}));
        }
    }

    fn press(&self, key: Key, ctrl: bool) {
        let (k, code, vk, text) = key_parts(key);
        let mut i = self.inner.borrow_mut();
        let mut down = json!({
            "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": k, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
            "modifiers": if ctrl { 2 } else { 0 },
        });
        if let Some(t) = text {
            down["text"] = json!(t);
        }
        if ctrl && key == Key::A {
            down["commands"] = json!(["selectAll"]);
        }
        let _ = i.cdp("Input.dispatchKeyEvent", down);
        let up = json!({"type": "keyUp", "key": k, "code": code, "windowsVirtualKeyCode": vk,
            "nativeVirtualKeyCode": vk, "modifiers": if ctrl { 2 } else { 0 }});
        let _ = i.cdp("Input.dispatchKeyEvent", up);
        i.pump(Duration::from_millis(30));
    }

    fn scroll(&self, lines: i32) {
        let mut i = self.inner.borrow_mut();
        let (x, y) = i.mouse;
        i.mouse("mouseWheel", x, y, json!({"deltaX": 0, "deltaY": -(lines as f64) * 40.0}));
    }

    fn activate_window(&self, hwnd: isize) -> bool {
        hwnd == WINDOW_HWND
    }

    fn foreground_window(&self) -> isize {
        WINDOW_HWND
    }

    fn activate(&self, _app: &str) -> bool {
        true
    }

    fn open_url(&self, _browser: &str, url: &str) -> bool {
        self.navigate(url).is_ok()
    }

    fn launch(&self, _app: &wcore::apps::App) -> bool {
        false
    }

    fn remember_site(&self, _url: &str) {}

    /// Instead of sleeping, wait until the page is quiet (never longer than asked).
    fn pause(&self, seconds: f64) {
        self.settle(seconds.clamp(0.3, 10.0));
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn tree() -> Value {
        json!({"nodes": [
            {"nodeId": "1", "ignored": false, "role": {"value": "RootWebArea"}, "name": {"value": "Form"}, "childIds": ["2","3","4","5","6","7"], "backendDOMNodeId": 1},
            {"nodeId": "2", "parentId": "1", "ignored": false, "role": {"value": "textbox"}, "name": {"value": "Full name"}, "value": {"value": "Asha"}, "backendDOMNodeId": 10},
            {"nodeId": "3", "parentId": "1", "ignored": false, "role": {"value": "button"}, "name": {"value": "Submit"}, "childIds": ["8"], "backendDOMNodeId": 11},
            {"nodeId": "8", "ignored": false, "role": {"value": "StaticText"}, "name": {"value": "Submit"}, "backendDOMNodeId": 18},
            {"nodeId": "4", "parentId": "1", "ignored": false, "role": {"value": "checkbox"}, "name": {"value": "Certify"}, "properties": [{"name": "checked", "value": {"type": "tristate", "value": "true"}}], "backendDOMNodeId": 12},
            {"nodeId": "5", "parentId": "1", "ignored": true, "role": {"value": "button"}, "name": {"value": "Hidden"}, "backendDOMNodeId": 13},
            {"nodeId": "6", "parentId": "1", "ignored": false, "role": {"value": "combobox"}, "name": {"value": "Country"}, "value": {"value": "India"}, "childIds": ["9"], "backendDOMNodeId": 14},
            {"nodeId": "9", "ignored": false, "role": {"value": "option"}, "name": {"value": "India"}, "backendDOMNodeId": 15},
            {"nodeId": "7", "parentId": "1", "ignored": false, "role": {"value": "StaticText"}, "name": {"value": "Apply now"}, "backendDOMNodeId": 16}
        ]})
    }

    #[test]
    fn parses_and_selects_nodes() {
        let nodes = parse_ax(&tree());
        assert_eq!(nodes.len(), 8, "ignored node dropped");
        assert_eq!(nodes.iter().find(|n| n.id == "8").unwrap().parent.as_deref(), Some("3"));
        let picks = select_nodes(&nodes, None);
        let labels: Vec<&str> = picks.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["Full name: Asha", "Submit", "Certify (checked)", "Country: India", "Apply now"]);
        assert_eq!(picks[0].role, "AXTextField");
        assert!(picks[1].pressable && !picks[0].pressable);
        // the select's options appear once it holds focus
        let open = select_nodes(&nodes, Some(14));
        assert!(open.iter().any(|p| p.label == "India" && p.option_of.is_some()));
    }

    #[test]
    fn role_vocabulary() {
        assert_eq!(ax_role("textbox", true), Some("AXTextArea"));
        assert_eq!(ax_role("link", false), Some("AXLink"));
        assert_eq!(ax_role("radio", false), Some("AXRadioButton"));
        assert_eq!(ax_role("generic", false), None);
    }

    #[test]
    fn box_models_and_quads() {
        let m = json!({"model": {"border": [10, 20, 110, 20, 110, 60, 10, 60]}});
        assert_eq!(bbox_of(&m), Some((10.0, 20.0, 100.0, 40.0)));
        let q = json!({"quads": [[0,0,10,0,10,5,0,5], [0,10,30,10,30,15,0,15]]});
        assert_eq!(bbox_of(&q), Some((0.0, 0.0, 30.0, 15.0)));
        assert_eq!(bbox_of(&json!({})), None);
    }

    /// A browser that answers from a script, and records every method it was asked.
    fn fake_browser(log: Arc<Mutex<Vec<String>>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((s, _)) = listener.accept() else { return };
            let mut ws = tungstenite::accept(s).unwrap();
            while let Ok(Message::Text(t)) = ws.read() {
                let m: Value = serde_json::from_str(t.as_str()).unwrap();
                let method = m["method"].as_str().unwrap_or("").to_string();
                log.lock().unwrap().push(method.clone());
                let result = match method.as_str() {
                    "Target.getTargets" => json!({"targetInfos": [{"targetId": "T1", "type": "page", "title": "Form", "url": "http://x/"}]}),
                    "Target.attachToTarget" => json!({"sessionId": "S1"}),
                    "Accessibility.getFullAXTree" => tree(),
                    "DOM.getBoxModel" => {
                        let b = m["params"]["backendNodeId"].as_u64().unwrap_or(0) as f64;
                        json!({"model": {"border": [100, b*10.0, 300, b*10.0, 300, b*10.0+30.0, 100, b*10.0+30.0]}})
                    }
                    "DOM.getContentQuads" => json!({"quads": [[5, 5, 55, 5, 55, 25, 5, 25]]}),
                    "Runtime.evaluate" => {
                        let e = m["params"]["expression"].as_str().unwrap_or("");
                        if e.contains("document.title") { json!({"result": {"type": "string", "value": "Form"}}) }
                        else if e.contains("location.href") { json!({"result": {"type": "string", "value": "http://x/"}}) }
                        else { json!({"result": {"type": "object", "subtype": "null", "value": null}}) }
                    }
                    "Page.captureScreenshot" => {
                        let img = image::RgbaImage::from_pixel(4, 2, image::Rgba([10, 20, 30, 255]));
                        let mut png = Vec::new();
                        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
                        json!({"data": base64::engine::general_purpose::STANDARD.encode(png)})
                    }
                    "DOM.resolveNode" => json!({"object": {"objectId": "O1"}}),
                    "Runtime.callFunctionOn" => json!({"result": {"type": "boolean", "value": true}}),
                    _ => json!({}),
                };
                let mut reply = json!({"id": m["id"], "result": result});
                if let Some(s) = m.get("sessionId") {
                    reply["sessionId"] = s.clone();
                }
                if ws.send(Message::text(reply.to_string())).is_err() {
                    break;
                }
            }
        });
        format!("ws://127.0.0.1:{port}/devtools/browser/fake")
    }

    #[test]
    fn walks_screenshots_and_presses_through_a_fake_browser() {
        use actions::Desktop as _;
        use perception::Desktop as _;
        let log = Arc::new(Mutex::new(Vec::new()));
        let url = fake_browser(log.clone());
        let d = HeadlessDesktop::connect(&url, (-1920, 200), 4242).unwrap();

        let m = d.monitors();
        assert_eq!((m[0].left, m[0].top, m[0].right), (-1920, 200, 0));
        let w = d.open_windows();
        assert_eq!((w[0].app.as_str(), w[0].pid, w[0].rect.0), ("chrome", 4242, -1920));
        assert_eq!(d.process_name(4242), "chrome");

        let shot = d.screenshot(None).unwrap();
        assert_eq!((shot.width, shot.height, shot.origin), (4, 2, (-1920, 200)));
        assert_eq!(&shot.bgra[..4], &[30, 20, 10, 255], "BGRA, not RGBA");

        let walked = d.walk(WINDOW_HWND, 1920.0, 1080.0, (-1920.0, 200.0));
        let name = walked.on_screen.iter().find(|h| h.label == "Full name: Asha").unwrap();
        // box x=100,y=100 in the viewport, plus the origin
        assert_eq!((name.x, name.y, name.w, name.h), (-1820.0, 300.0, 200.0, 30.0));
        assert_eq!(name.role, "AXTextField");
        assert_eq!(name.handle.remote_id(), 10);
        let text = walked.on_screen.iter().find(|h| h.label == "Apply now").unwrap();
        assert_eq!((text.x, text.w), (-1915.0, 50.0));

        assert!(d.ax_press(&name.handle));
        let seen = log.lock().unwrap().clone();
        assert!(seen.contains(&"DOM.resolveNode".to_string()));
        assert!(seen.contains(&"Runtime.callFunctionOn".to_string()));

        d.click_at((-1820.0, 300.0));
        assert!(log.lock().unwrap().iter().filter(|m| *m == "Input.dispatchMouseEvent").count() >= 3);
    }
}
