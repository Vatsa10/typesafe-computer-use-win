//! Personal document awareness: a metadata-only index of the user's documents, a ranked
//! `find_documents`, in-memory text extraction, a resume `Profile` and `form_answer`.
//!
//! Only metadata (path, name, size, modified time, and a few derived section flags) is ever
//! written to disk by the index. Document text lives in memory for the length of one call. A
//! profile is persisted only when the caller opts in.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use api::openai::problem;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::writer::StructuredWriter;

pub const EXTENSIONS: [&str; 5] = ["pdf", "docx", "txt", "md", "rtf"];
const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
const MAX_TEXT_CHARS: usize = 200_000;
const PROFILE_TEXT_CHARS: usize = 24_000;
const SKIP_DIRS: [&str; 14] = [
    "node_modules",
    ".git",
    ".ssh",
    ".gnupg",
    "appdata",
    "$recycle.bin",
    "system volume information",
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "target",
    "venv",
    "site-packages",
];
const BROWSER_DIRS: [&str; 6] = [
    "user data",
    "mozilla",
    "firefox",
    "bitwarden",
    "keepass",
    "1password",
];
const UNSAFE_NAME_PARTS: [&str; 22] = [
    "password",
    "passwd",
    "secret",
    "credential",
    "id_rsa",
    "id_ed25519",
    "private key",
    "private_key",
    "privatekey",
    "api_key",
    "apikey",
    "api key",
    "token",
    "recovery code",
    "backup code",
    "seed phrase",
    "wallet",
    "bitwarden",
    "lastpass",
    "1password",
    "keepass",
    "login data",
];

// ---------------------------------------------------------------- limits and index types

#[derive(Debug, Clone)]
pub struct IndexLimits {
    pub max_depth: usize,
    pub max_entries: usize,
    pub time_budget: Duration,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_depth: 6,
            max_entries: 20_000,
            time_budget: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocEntry {
    pub path: String,
    pub name: String,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub modified: i64,
    /// Derived section flags from the last read: 1 experience, 2 education, 4 skills, 8 contact,
    /// 16 no text layer. Not content. None until a resume search has looked.
    #[serde(default)]
    pub signals: Option<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocIndex {
    pub version: u32,
    pub built_at: i64,
    pub roots: Vec<String>,
    pub entries: Vec<DocEntry>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexReport {
    pub files: usize,
    pub added_or_changed: usize,
    pub skipped_unsafe: usize,
    pub timed_out: bool,
    pub elapsed_ms: u128,
}

pub fn epoch(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_epoch() -> i64 {
    epoch(SystemTime::now())
}

/// Windows known folders (Documents, Desktop, Downloads) and OneDrive, deduplicated, with
/// folders nested inside another root dropped.
pub fn known_roots() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        use windows::Win32::System::Com::CoTaskMemFree;
        use windows::Win32::UI::Shell::{
            FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, SHGetKnownFolderPath,
            KF_FLAG_DEFAULT,
        };
        for id in [&FOLDERID_Documents, &FOLDERID_Desktop, &FOLDERID_Downloads] {
            // SAFETY: the returned buffer is freed once, after it is copied out.
            unsafe {
                if let Ok(p) = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) {
                    if let Ok(s) = p.to_string() {
                        found.push(PathBuf::from(s));
                    }
                    CoTaskMemFree(Some(p.0 as *const _));
                }
            }
        }
    }
    for var in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
        if let Some(v) = std::env::var_os(var) {
            found.push(PathBuf::from(v));
        }
    }
    if found.is_empty() {
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            let home = PathBuf::from(home);
            for d in ["Documents", "Desktop", "Downloads"] {
                found.push(home.join(d));
            }
        }
    }
    dedupe_roots(found)
}

fn dedupe_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut canon: Vec<PathBuf> = roots
        .into_iter()
        .filter(|p| p.is_dir())
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
        .collect();
    canon.sort();
    canon.dedup();
    let all = canon.clone();
    canon.retain(|p| !all.iter().any(|q| q != p && p.starts_with(q)));
    canon
}

// ---------------------------------------------------------------- safety rules

/// True for a file name that must never be indexed or read.
pub fn unsafe_file_name(name: &str) -> bool {
    let n = name.to_lowercase();
    if n.starts_with('.') {
        return true;
    }
    if matches!(
        n.rsplit('.').next(),
        Some("pem" | "key" | "pfx" | "p12" | "ppk" | "kdbx" | "kdb" | "keychain" | "gpg" | "asc")
    ) {
        return true;
    }
    UNSAFE_NAME_PARTS.iter().any(|p| n.contains(p))
}

pub fn unsafe_dir_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.starts_with('.') || SKIP_DIRS.contains(&n.as_str()) || BROWSER_DIRS.contains(&n.as_str())
}

#[cfg(windows)]
fn hidden_or_system(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    // hidden, system, reparse point (junctions loop)
    meta.file_attributes() & (0x2 | 0x4 | 0x400) != 0
}

#[cfg(not(windows))]
fn hidden_or_system(_: &fs::Metadata) -> bool {
    false
}

// ---------------------------------------------------------------- indexing

fn index_path(home: &Path) -> PathBuf {
    home.join("documents-index.json")
}

pub fn load_index(home: &Path) -> DocIndex {
    fs::read_to_string(index_path(home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_index(home: &Path, index: &DocIndex) -> std::io::Result<()> {
    fs::create_dir_all(home)?;
    let text = serde_json::to_string(index).map_err(std::io::Error::other)?;
    fs::write(index_path(home), text)
}

/// Walk `roots`, reusing `previous` entries whose size and modified time are unchanged.
pub fn build_index(
    roots: &[PathBuf],
    previous: &DocIndex,
    limits: &IndexLimits,
) -> (DocIndex, IndexReport) {
    let start = Instant::now();
    let old: HashMap<&str, &DocEntry> = previous
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();
    let mut report = IndexReport::default();
    let mut entries: Vec<DocEntry> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = roots.iter().map(|r| (r.clone(), 0)).collect();
    'walk: while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for item in rd.flatten() {
            if start.elapsed() > limits.time_budget || entries.len() >= limits.max_entries {
                report.timed_out = true;
                break 'walk;
            }
            let name = item.file_name().to_string_lossy().to_string();
            let Ok(meta) = item.metadata() else { continue };
            if hidden_or_system(&meta) {
                continue;
            }
            let path = item.path();
            if meta.is_dir() {
                if depth < limits.max_depth && !unsafe_dir_name(&name) {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }
            if unsafe_file_name(&name) {
                report.skipped_unsafe += 1;
                continue;
            }
            let modified = meta.modified().map(epoch).unwrap_or(0);
            let key = path.to_string_lossy().to_string();
            let entry = match old.get(key.as_str()) {
                Some(e) if e.size == meta.len() && e.modified == modified => (*e).clone(),
                _ => {
                    report.added_or_changed += 1;
                    DocEntry {
                        path: key,
                        name,
                        size: meta.len(),
                        modified,
                        signals: None,
                    }
                }
            };
            entries.push(entry);
        }
    }
    report.files = entries.len();
    report.elapsed_ms = start.elapsed().as_millis();
    let index = DocIndex {
        version: 1,
        built_at: now_epoch(),
        roots: roots
            .iter()
            .map(|r| r.to_string_lossy().to_string())
            .collect(),
        entries,
    };
    (index, report)
}

// ---------------------------------------------------------------- text extraction

#[derive(Debug, Clone, PartialEq)]
pub enum ExtractError {
    /// A PDF with no text layer: a scan or an image. Nothing was read.
    Scanned,
    TooLarge(u64),
    Timeout,
    Unsupported(String),
    Failed(String),
    Unsafe,
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scanned => f.write_str(
                "this PDF has no text layer (it looks scanned), so its contents cannot be read without OCR",
            ),
            Self::TooLarge(n) => write!(
                f,
                "the file is {} MB, over the {} MB limit",
                n / 1_048_576,
                MAX_FILE_BYTES / 1_048_576
            ),
            Self::Timeout => f.write_str("reading the file took too long"),
            Self::Unsupported(e) => write!(f, "cannot read .{e} files"),
            Self::Failed(why) => write!(f, "could not read the file: {why}"),
            Self::Unsafe => f.write_str("this file is excluded by the safety rules"),
        }
    }
}

impl std::error::Error for ExtractError {}

/// Plain text of a document, in memory, bounded in size and time.
pub fn extract_text(path: &Path, timeout: Duration) -> Result<String, ExtractError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if unsafe_file_name(&name) {
        return Err(ExtractError::Unsafe);
    }
    let size = fs::metadata(path)
        .map_err(|e| ExtractError::Failed(e.kind().to_string()))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(ExtractError::TooLarge(size));
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !EXTENSIONS.contains(&ext.as_str()) {
        return Err(ExtractError::Unsupported(ext));
    }
    let path = path.to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(|| extract_blocking(&path, &ext)).unwrap_or_else(|_| {
            Err(ExtractError::Failed(
                "the reader crashed on this file".into(),
            ))
        });
        let _ = tx.send(r);
    });
    match rx.recv_timeout(timeout) {
        Ok(r) => r.map(|t| t.chars().take(MAX_TEXT_CHARS).collect()),
        Err(_) => Err(ExtractError::Timeout),
    }
}

fn extract_blocking(path: &Path, ext: &str) -> Result<String, ExtractError> {
    let bytes = fs::read(path).map_err(|e| ExtractError::Failed(e.kind().to_string()))?;
    match ext {
        "pdf" => {
            let text = pdf_extract::extract_text_from_mem(&bytes)
                .map_err(|e| ExtractError::Failed(format!("pdf: {e}")))?;
            if text.chars().filter(|c| c.is_alphanumeric()).count() < 20 {
                return Err(ExtractError::Scanned);
            }
            Ok(text)
        }
        "docx" => docx_text(&bytes),
        "rtf" => Ok(rtf_text(&String::from_utf8_lossy(&bytes))),
        _ => Ok(String::from_utf8_lossy(&bytes).to_string()),
    }
}

fn docx_text(bytes: &[u8]) -> Result<String, ExtractError> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| ExtractError::Failed(format!("docx: {e}")))?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")
        .map_err(|e| ExtractError::Failed(format!("docx: {e}")))?
        .take(MAX_FILE_BYTES)
        .read_to_string(&mut xml)
        .map_err(|e| ExtractError::Failed(format!("docx: {e}")))?;
    let re = Regex::new(r"<w:t(?:\s[^>]*)?>([^<]*)</w:t>|</w:p>|<w:tab/>|<w:br/>").unwrap();
    let mut out = String::new();
    for c in re.captures_iter(&xml) {
        match c.get(1) {
            Some(t) => out.push_str(&unescape(t.as_str())),
            None if c[0].starts_with("<w:tab") => out.push('\t'),
            None => out.push('\n'),
        }
    }
    Ok(out)
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn rtf_text(s: &str) -> String {
    let par = Regex::new(r"\\par[d]?\b").unwrap();
    let ctl = Regex::new(r"\\[a-zA-Z]+-?\d* ?|\\'[0-9a-fA-F]{2}|[{}]").unwrap();
    ctl.replace_all(&par.replace_all(s, "\n"), "").to_string()
}

// ---------------------------------------------------------------- ranking

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub path: String,
    pub name: String,
    pub modified: i64,
    /// Local calendar date of the modified time, YYYY-MM-DD.
    pub modified_date: String,
    pub score: i32,
    pub why: Vec<String>,
}

fn date_of(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

fn recency(modified: i64, now: i64) -> i32 {
    let days = ((now - modified).max(0) as f64) / 86_400.0;
    (20.0 * (-days / 180.0).exp()).round() as i32
}

fn name_tokens(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect()
}

/// Score from the file name alone for a resume, with the reasons.
fn resume_name_score(name: &str, user_name: &str) -> (i32, Vec<String>) {
    let lower = name_tokens(name);
    let compact: String = lower.split_whitespace().collect();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let mut score = 0;
    let mut why = Vec::new();
    if compact.contains("resume") || compact.contains("curriculumvitae") || words.contains(&"cv") {
        score += 50;
        why.push("filename says resume/CV".to_string());
    }
    if !user_name.trim().is_empty() {
        let hit = name_tokens(user_name)
            .split_whitespace()
            .any(|t| t.len() >= 3 && compact.contains(t));
        if hit {
            score += 20;
            why.push("filename has your name".to_string());
        }
    }
    (score, why)
}

/// Section flags of a document's text. Not the text itself.
pub fn resume_signals(text: &str) -> u8 {
    let section = |alts: &str| {
        Regex::new(&format!(r"(?im)^\s*(?:{alts})\b[^\n]{{0,30}}$"))
            .unwrap()
            .is_match(text)
    };
    let mut s = 0u8;
    if section("work experience|professional experience|experience|employment|work history") {
        s |= 1;
    }
    if section("education|academic") {
        s |= 2;
    }
    if section("skills|technical skills|core competencies") {
        s |= 4;
    }
    if Regex::new(r"[\w.+-]+@[\w-]+\.[\w.-]+")
        .unwrap()
        .is_match(text)
    {
        s |= 8;
    }
    s
}

fn signal_score(signals: u8) -> (i32, Vec<String>) {
    let mut why = Vec::new();
    let mut score = 0;
    for (bit, label) in [(1u8, "experience"), (2, "education"), (4, "skills")] {
        if signals & bit != 0 {
            score += 10;
            why.push(format!("has a {label} section"));
        }
    }
    if signals & 8 != 0 && score > 0 {
        score += 3;
    }
    (score, why)
}

/// Find documents. `kind` is "resume" or "any". `user_name` improves resume matching.
/// Resume search reads the text of a bounded number of candidates and caches only section flags
/// in `index`.
pub fn find_documents(
    index: &mut DocIndex,
    query: &str,
    kind: &str,
    limit: usize,
    user_name: &str,
    now: i64,
) -> Vec<Hit> {
    let limit = limit.clamp(1, 20);
    let mut hits: Vec<Hit> = if kind.eq_ignore_ascii_case("resume") {
        resume_hits(index, query, user_name, now)
    } else {
        let toks: Vec<String> = name_tokens(query)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        index
            .entries
            .iter()
            .filter_map(|e| {
                let nm = name_tokens(&e.name);
                let matched: Vec<&String> =
                    toks.iter().filter(|t| nm.contains(t.as_str())).collect();
                if !toks.is_empty() && matched.is_empty() {
                    return None;
                }
                let mut why: Vec<String> = matched
                    .iter()
                    .map(|t| format!("filename has {t:?}"))
                    .collect();
                why.push(format!("modified {}", date_of(e.modified)));
                let score = matched.len() as i32 * 25 + recency(e.modified, now);
                Some(hit(e, score, why))
            })
            .collect()
    };
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(b.modified.cmp(&a.modified)));
    hits.truncate(limit);
    hits
}

fn hit(e: &DocEntry, score: i32, why: Vec<String>) -> Hit {
    Hit {
        path: e.path.clone(),
        name: e.name.clone(),
        modified: e.modified,
        modified_date: date_of(e.modified),
        score,
        why,
    }
}

fn resume_hits(index: &mut DocIndex, query: &str, user_name: &str, now: i64) -> Vec<Hit> {
    // Read text for the likeliest candidates only: name matches first, then the newest.
    let mut order: Vec<usize> = (0..index.entries.len())
        .filter(|&i| {
            let e = &index.entries[i];
            e.signals.is_none() && e.size <= 3 * 1_048_576
        })
        .collect();
    order.sort_by_key(|&i| {
        let e = &index.entries[i];
        let named = resume_name_score(&e.name, user_name).0 > 0;
        (std::cmp::Reverse(named), std::cmp::Reverse(e.modified))
    });
    let started = Instant::now();
    for &i in order.iter().take(20) {
        if started.elapsed() > Duration::from_secs(12) {
            break;
        }
        let path = PathBuf::from(&index.entries[i].path);
        index.entries[i].signals = Some(match extract_text(&path, Duration::from_secs(4)) {
            Ok(t) => resume_signals(&t),
            Err(ExtractError::Scanned) => 16,
            Err(_) => 0,
        });
    }
    let qtoks: Vec<String> = name_tokens(query)
        .split_whitespace()
        .filter(|t| {
            !matches!(
                *t,
                "my" | "latest" | "resume" | "cv" | "the" | "a" | "newest"
            )
        })
        .map(str::to_string)
        .collect();
    index
        .entries
        .iter()
        .filter_map(|e| {
            let (ns, mut why) = resume_name_score(&e.name, user_name);
            let signals = e.signals.unwrap_or(0);
            let (cs, cwhy) = signal_score(signals);
            why.extend(cwhy);
            if signals & 16 != 0 {
                why.push("PDF has no text layer; ranked on filename only".to_string());
            }
            if ns < 40 && cs < 20 {
                return None;
            }
            let nm = name_tokens(&e.name);
            let q = qtoks.iter().filter(|t| nm.contains(t.as_str())).count() as i32 * 10;
            if q > 0 {
                why.push("filename matches your query".to_string());
            }
            why.push(format!("modified {}", date_of(e.modified)));
            Some(hit(e, ns + cs + q + recency(e.modified, now), why))
        })
        .collect()
}

// ---------------------------------------------------------------- the profile

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Work {
    pub company: String,
    pub title: String,
    pub start: String,
    pub end: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Education {
    pub school: String,
    pub degree: String,
    pub field: String,
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub name: String,
    pub email: String,
    pub phone: String,
    pub location: String,
    pub links: Vec<String>,
    pub summary: String,
    pub work: Vec<Work>,
    pub education: Vec<Education>,
    pub skills: Vec<String>,
    pub source_path: String,
    pub source_modified: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProfileError {
    Extract(ExtractError),
    Writer(String),
    BadReply(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Extract(e) => e.fmt(f),
            Self::Writer(w) => write!(f, "could not structure the resume: {w}"),
            Self::BadReply(w) => write!(f, "the structuring reply did not match its schema: {w}"),
        }
    }
}

impl std::error::Error for ProfileError {}

pub const PROFILE_SYSTEM: &str = "You receive the text of one resume. Fill the schema by copying what the document says. Copy every value exactly as written; never invent, infer, normalize or complete anything. If the document does not state a field, leave it as an empty string or empty list. Do not guess a location from a school or employer. Dates stay as written.";

fn profile_properties() -> Value {
    let s = json!({"type": "string"});
    let obj = |keys: &[&str]| {
        let props: serde_json::Map<String, Value> =
            keys.iter().map(|k| ((*k).to_string(), s.clone())).collect();
        json!({"type": "object", "additionalProperties": false, "required": keys, "properties": props})
    };
    json!({
        "name": s, "email": s, "phone": s, "location": s, "summary": s,
        "links": {"type": "array", "items": s},
        "work": {"type": "array", "items": obj(&["company", "title", "start", "end", "description"])},
        "education": {"type": "array", "items": obj(&["school", "degree", "field", "start", "end"])},
        "skills": {"type": "array", "items": s},
    })
}

fn squash(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

fn strs(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Turn the writer's reply into a Profile, dropping anything the document text does not contain.
pub fn profile_from_reply(data: &Value, text: &str, path: &str, modified: i64) -> Profile {
    let flat = squash(text);
    let in_text = |v: &str| {
        let n = squash(v);
        !n.is_empty() && flat.contains(&n)
    };
    // Keep a value only when the document holds it, else leave the field empty.
    let keep = |v: &Value, k: &str| {
        let x = field(v, k);
        if in_text(&x) {
            x
        } else {
            String::new()
        }
    };
    let mut p = Profile {
        source_path: path.to_string(),
        source_modified: modified,
        ..Default::default()
    };
    let name = field(data, "name");
    if !name.is_empty() && name.split_whitespace().all(in_text) {
        p.name = name;
    }
    p.email = keep(data, "email");
    let phone = field(data, "phone");
    let pd = digits(&phone);
    if pd.len() >= 7 && digits(text).contains(&pd) {
        p.phone = phone;
    }
    p.location = keep(data, "location");
    p.summary = keep(data, "summary");
    p.links = strs(data, "links")
        .into_iter()
        .filter(|l| in_text(l))
        .collect();
    p.skills = strs(data, "skills")
        .into_iter()
        .filter(|l| in_text(l))
        .collect();
    for w in data
        .get("work")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (company, title) = (keep(w, "company"), keep(w, "title"));
        if company.is_empty() && title.is_empty() {
            continue;
        }
        p.work.push(Work {
            company,
            title,
            start: keep(w, "start"),
            end: keep(w, "end"),
            description: keep(w, "description"),
        });
    }
    for e in data
        .get("education")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (school, degree) = (keep(e, "school"), keep(e, "degree"));
        if school.is_empty() && degree.is_empty() {
            continue;
        }
        p.education.push(Education {
            school,
            degree,
            field: keep(e, "field"),
            start: keep(e, "start"),
            end: keep(e, "end"),
        });
    }
    p
}

/// Read one resume and structure it through the writer.
pub fn build_profile(
    writer: &dyn StructuredWriter,
    model: &str,
    path: &Path,
) -> Result<Profile, ProfileError> {
    let modified = fs::metadata(path)
        .and_then(|m| m.modified())
        .map(epoch)
        .map_err(|e| ProfileError::Extract(ExtractError::Failed(e.kind().to_string())))?;
    let text = extract_text(path, Duration::from_secs(15)).map_err(ProfileError::Extract)?;
    let excerpt: String = text.chars().take(PROFILE_TEXT_CHARS).collect();
    let data = writer
        .structured(
            model,
            PROFILE_SYSTEM,
            &json!({"resume_text": excerpt}),
            &profile_properties(),
            3000,
            None,
        )
        .map_err(|e| ProfileError::Writer(problem(&e.message)))?;
    if !data.is_object() {
        return Err(ProfileError::BadReply("not an object".into()));
    }
    Ok(profile_from_reply(
        &data,
        &text,
        &path.to_string_lossy(),
        modified,
    ))
}

/// True when the source file changed (or vanished) since the profile was read.
pub fn profile_is_stale(p: &Profile) -> bool {
    match fs::metadata(&p.source_path).and_then(|m| m.modified()) {
        Ok(t) => epoch(t) != p.source_modified,
        Err(_) => true,
    }
}

fn profile_file(home: &Path, source: &str) -> PathBuf {
    let h = source.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    });
    home.join(format!("profile-{h:016x}.json"))
}

// ---------------------------------------------------------------- the service

/// One per session: index, user name and the in-memory profile cache.
pub struct Documents {
    pub home: PathBuf,
    pub roots: Vec<PathBuf>,
    pub user_name: String,
    pub limits: IndexLimits,
    index: Mutex<DocIndex>,
    profiles: Mutex<HashMap<String, Profile>>,
}

impl Documents {
    pub fn new(home: PathBuf, roots: Vec<PathBuf>, user_name: &str) -> Self {
        let index = load_index(&home);
        Self {
            home,
            roots,
            user_name: user_name.to_string(),
            limits: IndexLimits::default(),
            index: Mutex::new(index),
            profiles: Mutex::new(HashMap::new()),
        }
    }

    /// Re-walk the roots (incremental) and persist the metadata index.
    pub fn refresh(&self) -> IndexReport {
        let mut guard = self.index.lock().unwrap();
        let (fresh, report) = build_index(&self.roots, &guard, &self.limits);
        *guard = fresh;
        let _ = save_index(&self.home, &guard);
        report
    }

    /// Search. Refreshes the index first when it is older than ten minutes or empty.
    pub fn find(&self, query: &str, kind: &str, limit: usize) -> Vec<Hit> {
        let stale = {
            let g = self.index.lock().unwrap();
            g.entries.is_empty() || now_epoch() - g.built_at > 600
        };
        if stale {
            self.refresh();
        }
        let mut g = self.index.lock().unwrap();
        let hits = find_documents(&mut g, query, kind, limit, &self.user_name, now_epoch());
        let _ = save_index(&self.home, &g);
        hits
    }

    fn save_profile(&self, key: &str, p: &Profile) {
        let _ = fs::create_dir_all(&self.home);
        if let Ok(t) = serde_json::to_string(p) {
            let _ = fs::write(profile_file(&self.home, key), t);
        }
    }

    /// The profile for a resume: from the session cache, else the opt-in persisted copy, else
    /// freshly read. A changed file always re-reads. `persist` writes it to disk (opt in).
    pub fn profile(
        &self,
        writer: &dyn StructuredWriter,
        model: &str,
        path: &Path,
        persist: bool,
    ) -> Result<Profile, ProfileError> {
        let key = path.to_string_lossy().to_string();
        if let Some(p) = self.profiles.lock().unwrap().get(&key) {
            if !profile_is_stale(p) {
                let p = p.clone();
                if persist {
                    self.save_profile(&key, &p);
                }
                return Ok(p);
            }
        }
        let disk = fs::read_to_string(profile_file(&self.home, &key))
            .ok()
            .and_then(|t| serde_json::from_str::<Profile>(&t).ok())
            .filter(|p| !profile_is_stale(p));
        let p = match disk {
            Some(p) => p,
            None => build_profile(writer, model, path)?,
        };
        if persist {
            self.save_profile(&key, &p);
        }
        self.profiles.lock().unwrap().insert(key, p.clone());
        Ok(p)
    }
}

// ---------------------------------------------------------------- form answers

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FormAnswer {
    Value { value: String, source: String },
    NeedsUser { reason: String },
}

fn needs(reason: &str) -> FormAnswer {
    FormAnswer::NeedsUser {
        reason: reason.to_string(),
    }
}

/// Fields the agent never answers for the user.
pub fn is_sensitive_field(label: &str) -> bool {
    let re = Regex::new(
        r"(?i)\b(salary|salaries|compensation|ctc|remuneration|pay|wage|rate|visa|sponsor\w*|work authori[sz]\w*|authori[sz]ed|eligib\w*|citizen\w*|immigration|gender|sex|pronouns?|transgender|disabilit\w*|disabled|race|racial|ethnic\w*|hispanic|veteran|criminal|convict\w*|felony|misdemeanou?r|arrest\w*|background check|attest\w*|certify|declar\w*|affirm\w*|i agree|agree to|consent|acknowledg\w*|signature|sign here|ssn|social security|national id|passport|date of birth|dob|birth\w*|marital|religio\w*|sexual|orientation|age|drug|security clearance|relocat\w*|notice period|start date|references?)\b",
    )
    .unwrap();
    re.is_match(label)
}

fn pick_option(value: &str, options: &[String]) -> Option<String> {
    if options.is_empty() {
        return Some(value.to_string());
    }
    let v = value.trim().to_lowercase();
    options
        .iter()
        .find(|o| o.trim().to_lowercase() == v)
        .cloned()
}

/// Map one form field to a value held by the profile, or say the user must answer.
pub fn form_answer(field_label: &str, options: &[String], profile: &Profile) -> FormAnswer {
    if is_sensitive_field(field_label) {
        return needs("this is a personal or legal question that only you should answer");
    }
    let l = field_label.to_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| l.contains(w));
    let (first, last) = match profile.name.trim().rsplit_once(' ') {
        Some((f, la)) => (f.to_string(), la.to_string()),
        None => (profile.name.trim().to_string(), String::new()),
    };
    let link = |needle: &str| {
        profile
            .links
            .iter()
            .find(|x| x.to_lowercase().contains(needle))
            .cloned()
    };
    let latest = profile.work.first();
    let (src, value): (&str, Option<String>) = if has(&["first name", "given name"]) {
        ("name", Some(first))
    } else if has(&["last name", "surname", "family name"]) {
        ("name", Some(last))
    } else if has(&["full name", "your name", "legal name"]) || l.trim() == "name" {
        ("name", Some(profile.name.clone()))
    } else if has(&["email", "e-mail"]) {
        ("email", Some(profile.email.clone()))
    } else if has(&["phone", "mobile", "telephone"]) {
        ("phone", Some(profile.phone.clone()))
    } else if has(&["linkedin"]) {
        ("links", link("linkedin"))
    } else if has(&["github"]) {
        ("links", link("github"))
    } else if has(&["portfolio", "website", "personal site"]) {
        (
            "links",
            profile
                .links
                .iter()
                .find(|x| !x.to_lowercase().contains("linkedin"))
                .cloned(),
        )
    } else if has(&["city", "location", "address", "where are you based"]) {
        ("location", Some(profile.location.clone()))
    } else if has(&["current company", "current employer", "employer"]) {
        ("work[0].company", latest.map(|w| w.company.clone()))
    } else if has(&[
        "current title",
        "job title",
        "current role",
        "current position",
    ]) {
        ("work[0].title", latest.map(|w| w.title.clone()))
    } else if has(&["university", "school", "college", "institution"]) {
        (
            "education[0].school",
            profile.education.first().map(|e| e.school.clone()),
        )
    } else if has(&["degree"]) {
        (
            "education[0].degree",
            profile.education.first().map(|e| e.degree.clone()),
        )
    } else if has(&["skills"]) {
        ("skills", Some(profile.skills.join(", ")))
    } else if has(&["summary", "about you", "about yourself"]) {
        ("summary", Some(profile.summary.clone()))
    } else {
        return needs("the resume has nothing that answers this field");
    };
    let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
        return needs("the resume does not state this");
    };
    match pick_option(&value, options) {
        Some(value) => FormAnswer::Value {
            value,
            source: format!(
                "resume {} ({}), field {src}",
                profile.source_path,
                date_of(profile.source_modified)
            ),
        },
        None => needs("the resume's value is not one of the offered options"),
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use api::openai::ApiError;
    use std::io::Write;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pointer-docs-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn pdf(text: &str) -> Vec<u8> {
        let stream = text
            .lines()
            .enumerate()
            .map(|(i, l)| format!("BT /F1 12 Tf 50 {} Td ({l}) Tj ET", 750 - 20 * i as i32))
            .collect::<Vec<_>>()
            .join("\n");
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
            format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
        for o in offs {
            out.extend(format!("{o:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .bytes(),
        );
        out
    }

    fn docx(paras: &[&str]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut z = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        z.start_file("word/document.xml", opts).unwrap();
        let body: String = paras
            .iter()
            .map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>"))
            .collect();
        z.write_all(format!("<w:document><w:body>{body}</w:body></w:document>").as_bytes())
            .unwrap();
        z.finish().unwrap();
        buf.into_inner()
    }

    const RESUME: &str =
        "Jane Roe\nExperience\nAcme Corp Engineer\nEducation\nState University\nSkills\nRust Python";

    fn set_age(p: &Path, days: u64) {
        let t = SystemTime::now() - Duration::from_secs(days * 86_400);
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn setup(tag: &str) -> (PathBuf, Documents) {
        let d = dir(tag);
        let home = dir(&format!("{tag}-home"));
        let docs = Documents::new(home, vec![d.clone()], "Jane Roe");
        (d, docs)
    }

    #[test]
    fn newest_resume_wins_old_loses_report_loses() {
        let (d, docs) = setup("rank");
        fs::write(d.join("Resume_2021.pdf"), pdf(RESUME)).unwrap();
        set_age(&d.join("Resume_2021.pdf"), 1800);
        fs::write(d.join("Resume_final_v3.pdf"), pdf(RESUME)).unwrap();
        set_age(&d.join("Resume_final_v3.pdf"), 7);
        fs::write(
            d.join("report.pdf"),
            pdf("Quarterly\nExperience of the quarter"),
        )
        .unwrap();
        let hits = docs.find("", "resume", 5);
        assert_eq!(hits[0].name, "Resume_final_v3.pdf");
        assert_eq!(hits[1].name, "Resume_2021.pdf");
        assert!(hits[0].score > hits[1].score);
        assert!(hits.iter().all(|h| h.name != "report.pdf"));
        assert!(hits[0].why.iter().any(|w| w.contains("modified")));
    }

    #[test]
    fn content_alone_finds_unnamed_resume() {
        let (d, docs) = setup("content");
        fs::write(
            d.join("jane.docx"),
            docx(&["Jane Roe", "Experience", "Education", "Skills"]),
        )
        .unwrap();
        let hits = docs.find("", "resume", 3);
        assert_eq!(hits[0].name, "jane.docx");
    }

    #[test]
    fn safety_exclusions() {
        let (d, docs) = setup("safety");
        for sub in ["node_modules", ".git", "AppData", ".ssh", "ok"] {
            fs::create_dir_all(d.join(sub)).unwrap();
            fs::write(d.join(sub).join("resume.txt"), RESUME).unwrap();
        }
        fs::write(d.join(".env.txt"), "K=1").unwrap();
        fs::write(d.join("passwords.txt"), "x").unwrap();
        fs::write(d.join("bitwarden_export.txt"), "x").unwrap();
        fs::write(d.join("notes.png"), "x").unwrap();
        let report = docs.refresh();
        let g = docs.index.lock().unwrap();
        let names: Vec<&str> = g.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].contains("ok"));
        assert!(report.skipped_unsafe >= 2);
        drop(g);
        assert_eq!(
            extract_text(&d.join("passwords.txt"), Duration::from_secs(2)),
            Err(ExtractError::Unsafe)
        );
    }

    #[test]
    fn index_is_metadata_only_and_incremental() {
        let (d, docs) = setup("incr");
        fs::write(d.join("resume.txt"), "Secret-marker-text\nExperience").unwrap();
        let r1 = docs.refresh();
        assert_eq!(r1.added_or_changed, 1);
        docs.find("", "resume", 3);
        let saved = fs::read_to_string(index_path(&docs.home)).unwrap();
        assert!(!saved.contains("Secret-marker"));
        let r2 = docs.refresh();
        assert_eq!(r2.added_or_changed, 0);
        assert_eq!(r2.files, 1);
    }

    #[test]
    fn scanned_pdf_reports_clearly() {
        let d = dir("scan");
        let p = d.join("scan.pdf");
        fs::write(&p, pdf("")).unwrap();
        let e = extract_text(&p, Duration::from_secs(5)).unwrap_err();
        assert_eq!(e, ExtractError::Scanned);
        assert!(e.to_string().contains("no text layer"));
    }

    #[test]
    fn extracts_pdf_docx_txt() {
        let d = dir("extract");
        fs::write(d.join("a.pdf"), pdf(RESUME)).unwrap();
        fs::write(d.join("b.docx"), docx(&["Hello &amp; welcome", "Second"])).unwrap();
        fs::write(d.join("c.md"), "# Title").unwrap();
        let t = Duration::from_secs(10);
        let a = extract_text(&d.join("a.pdf"), t).unwrap();
        assert!(a.contains("Experience") && a.contains("Rust"), "{a:?}");
        let b = extract_text(&d.join("b.docx"), t).unwrap();
        assert_eq!(b, "Hello & welcome\nSecond\n");
        assert_eq!(extract_text(&d.join("c.md"), t).unwrap(), "# Title");
    }

    struct Fake(Value);
    impl StructuredWriter for Fake {
        fn structured(
            &self,
            _: &str,
            _: &str,
            _: &Value,
            _: &Value,
            _: u32,
            _: Option<&[u8]>,
        ) -> Result<Value, ApiError> {
            Ok(self.0.clone())
        }
    }

    fn reply() -> Value {
        json!({
            "name": "Jane Roe", "email": "jane@example.com", "phone": "", "location": "",
            "summary": "", "links": [], "skills": ["Rust", "Cobol"],
            "work": [{"company": "Acme Corp", "title": "Engineer", "start": "", "end": "", "description": ""},
                     {"company": "Invented Inc", "title": "CEO", "start": "", "end": "", "description": ""}],
            "education": [{"school": "State University", "degree": "", "field": "", "start": "", "end": ""}]
        })
    }

    #[test]
    fn profile_drops_invented_and_goes_stale() {
        let (d, docs) = setup("profile");
        let p = d.join("resume.txt");
        fs::write(&p, "Jane Roe\njane@example.com\nExperience\nAcme Corp Engineer\nEducation\nState University\nSkills\nRust").unwrap();
        let prof = docs.profile(&Fake(reply()), "m", &p, false).unwrap();
        assert_eq!(prof.name, "Jane Roe");
        assert_eq!(prof.skills, vec!["Rust"]);
        assert_eq!(prof.work.len(), 1);
        assert_eq!(prof.work[0].company, "Acme Corp");
        assert!(prof.phone.is_empty());
        assert!(!profile_is_stale(&prof));
        // cached: a writer that would answer differently is not asked
        let again = docs.profile(&Fake(json!({})), "m", &p, false).unwrap();
        assert_eq!(again.name, "Jane Roe");
        set_age(&p, 1);
        assert!(profile_is_stale(&prof));
        assert!(docs
            .profile(&Fake(json!({})), "m", &p, false)
            .unwrap()
            .name
            .is_empty());
        assert!(!profile_file(&docs.home, &p.to_string_lossy()).exists());
        docs.profile(&Fake(reply()), "m", &p, true).unwrap();
        assert!(profile_file(&docs.home, &p.to_string_lossy()).exists());
    }

    #[test]
    fn form_answers_and_sensitive_fields() {
        let prof = Profile {
            name: "Jane Q Roe".into(),
            email: "jane@example.com".into(),
            links: vec!["https://linkedin.com/in/jane".into()],
            work: vec![Work {
                company: "Acme".into(),
                ..Default::default()
            }],
            source_path: "r.txt".into(),
            ..Default::default()
        };
        match form_answer("Email address", &[], &prof) {
            FormAnswer::Value { value, source } => {
                assert_eq!(value, "jane@example.com");
                assert!(source.contains("r.txt"));
            }
            o => panic!("{o:?}"),
        }
        assert!(
            matches!(form_answer("Last name", &[], &prof), FormAnswer::Value { value, .. } if value == "Roe")
        );
        assert!(matches!(
            form_answer("LinkedIn URL", &[], &prof),
            FormAnswer::Value { .. }
        ));
        assert!(matches!(
            form_answer("Phone", &[], &prof),
            FormAnswer::NeedsUser { .. }
        ));
        assert!(matches!(
            form_answer("Current company", &["Other".into()], &prof),
            FormAnswer::NeedsUser { .. }
        ));
        for f in [
            "Expected salary",
            "Do you require visa sponsorship?",
            "Gender",
            "Disability status",
            "Race / ethnicity",
            "Have you been convicted of a crime?",
            "I certify the above is true",
            "Are you authorized to work in the US?",
            "Veteran status",
        ] {
            assert!(
                matches!(form_answer(f, &[], &prof), FormAnswer::NeedsUser { .. }),
                "{f}"
            );
        }
    }

    /// Live check on the real machine: `cargo test -p core live_resume -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_resume() {
        let home = dir("live-home");
        let docs = Documents::new(home, known_roots(), "");
        let r = docs.refresh();
        println!(
            "index: {} files in {} ms (timed_out {})",
            r.files, r.elapsed_ms, r.timed_out
        );
        let t = Instant::now();
        let hits = docs.find("", "resume", 3);
        println!("find: {} ms", t.elapsed().as_millis());
        for h in hits {
            println!("{} | {} | {}", h.name, h.modified_date, h.score);
        }
    }
}
