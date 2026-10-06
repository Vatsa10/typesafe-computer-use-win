//! Catalogue the installed applications, so the loop can launch one by name instead of only
//! opening URLs. A port of `apps.py`.
//!
//! This is `catalog`'s sibling: same shape, same instincts. It walks the two Start Menu `Programs`
//! trees Windows keeps -- one per user, one per machine -- turns every `.lnk` into an `App` row, and
//! caches the result as JSON under `%LOCALAPPDATA%/winclicker/apps.json`. Nothing here calls a
//! model, draws a UI, or touches the decision loop.
//!
//! Two rules keep this safe to hand to a model:
//!
//! * **The model picks a key; code owns the path.** `launch` takes an `App` that came out of this
//!   catalog and nothing else. A command line is never built out of model text, so there is no path
//!   from "the model said something odd" to "an arbitrary program ran". Anything not in the catalog
//!   cannot be launched at all.
//! * **The catalog is filtered before the model ever sees it.** The Start Menu is mostly not launch
//!   targets: uninstallers, readmes, licence files, admin consoles and -- worst for a computer-use
//!   agent -- the accessibility tools. See `DROP_NAMES` / `DROP_FOLDERS` for the reasoning.
//!
//! Tolerance is the whole job, as in `catalog`. No Start Menu, a folder that denies listing, a
//! `.lnk` that cannot be stat-ed, a profile that does not exist -- each degrades to fewer rows or an
//! empty list. Nothing in this module panics.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::catalog::slug;

pub const SOURCE: &str = "app";
pub const CACHE_SECONDS: f64 = 24.0 * 60.0 * 60.0;

// Weight is a prior on "is this a thing someone asks to open", and depth is the only signal a
// shortcut file carries. A shortcut sitting directly in `Programs` was put there by an installer
// that expected it to be launched (WhatsApp, Chrome); one three folders down is a sub-tool of a
// suite. So: 1.0 at the root, minus a step per folder, floored so nothing disappears entirely.
pub const TOP_WEIGHT: f64 = 1.0;
pub const DEPTH_STEP: f64 = 0.15;
pub const MIN_WEIGHT: f64 = 0.3;

/// A Start Menu is never this deep; the bound just stops a symlink loop.
const MAX_DEPTH: usize = 8;

/// Names that are never launch targets. The Start Menu is full of these and every one of them, if
/// offered to the classifier, is a chance for a goal like "open word" to resolve to a readme.
pub const DROP_NAMES: &str = r"(?i)(?:^|\W)(?:uninstall\w*|remove|setup|readme|release\s*notes?|documentation|help|license|licence|website|home\s*page|homepage)(?:\W|$)";

/// Whole subtrees to skip, by folder name anywhere in the path under a Programs root.
///
/// `Administrative Tools`, `Windows System`, `Windows PowerShell` and `Maintenance` hold consoles
/// that can reconfigure or wipe the machine -- never something to hand a model as a launch option.
/// `StartUp` is not a menu of apps at all, it is what Windows runs at logon.
///
/// The accessibility subtrees matter most, and for a reason specific to this project: `Magnify`,
/// `Narrator`, `On-Screen Keyboard`, `VoiceAccess` and `LiveCaptions` all seize the keyboard, the
/// focus or the screen. A computer-use agent that launched Narrator or Voice Access would then be
/// fighting it for control of the very machine it is driving, and the run could not recover.
pub const DROP_FOLDERS: [&str; 7] = [
    "administrative tools",
    "accessibility",
    "windows accessories",
    "windows system",
    "windows powershell",
    "maintenance",
    "startup",
];

/// The same accessibility tools also ship as loose shortcuts outside an `Accessibility` folder.
pub const DROP_EXACT: [&str; 8] = [
    "magnify",
    "magnifier",
    "narrator",
    "on-screen keyboard",
    "voiceaccess",
    "voice access",
    "livecaptions",
    "live captions",
];

const SHORTCUT_SUFFIX: &str = " - Shortcut";

fn drop_names() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| Regex::new(DROP_NAMES).unwrap())
}

/// One installed application the model may be asked to launch.
///
/// The field is named `url`, not `path`, on purpose: `shortlist::shortlist` ranks anything carrying
/// `key`/`label`/`url`/`weight`/`source`, and naming the `.lnk` path `url` means apps and sites go
/// through the same ranker with no change to it at all. It holds a filesystem path to a shortcut.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct App {
    /// Unique slug the model answers with: "whatsapp", "visual_studio_code".
    pub key: String,
    /// The shortcut's own name: "WhatsApp", "Visual Studio Code".
    pub label: String,
    /// The `.lnk` path -- see above for why it is not called `path`.
    pub url: String,
    /// Prior: higher is more likely to be what "open X" means.
    pub weight: f64,
    /// Always "app".
    pub source: String,
}

/// The user and machine `Start Menu/Programs` directories. Neither need exist.
pub fn start_menu_roots() -> Vec<PathBuf> {
    start_menu_roots_from(
        std::env::var_os("APPDATA").map(PathBuf::from),
        std::env::var_os("PROGRAMDATA").map(PathBuf::from),
    )
}

/// `start_menu_roots` from explicit `APPDATA` / `PROGRAMDATA` values.
pub fn start_menu_roots_from(
    appdata: Option<PathBuf>,
    programdata: Option<PathBuf>,
) -> Vec<PathBuf> {
    [appdata, programdata]
        .into_iter()
        .flatten()
        .filter(|base| !base.as_os_str().is_empty())
        .map(|base| {
            base.join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
        })
        .collect()
}

/// The shortcut's display name: its stem, with a trailing ` - Shortcut` stripped.
fn label_for(path: &Path) -> String {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().trim().to_string())
        .unwrap_or_default();
    if name
        .to_lowercase()
        .ends_with(&SHORTCUT_SUFFIX.to_lowercase())
    {
        let cut = name.chars().count() - SHORTCUT_SUFFIX.chars().count();
        return name
            .chars()
            .take(cut)
            .collect::<String>()
            .trim()
            .to_string();
    }
    name
}

/// True when this shortcut is noise, an admin console, or an accessibility tool.
fn dropped(label: &str, relative: &Path) -> bool {
    if label.is_empty() {
        return true;
    }
    let lowered = label.to_lowercase();
    // `Administrative Tools` and friends also appear as a single loose shortcut to the folder.
    if DROP_EXACT.contains(&lowered.as_str())
        || DROP_FOLDERS.contains(&lowered.as_str())
        || drop_names().is_match(label)
    {
        return true;
    }
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    let folders = &parts[..parts.len().saturating_sub(1)];
    folders
        .iter()
        .any(|part| DROP_FOLDERS.contains(&part.as_str()))
}

/// `1.0` at the Programs root, one `DEPTH_STEP` less per folder, never below `MIN_WEIGHT`.
fn weight_for(depth: usize) -> f64 {
    let w = (TOP_WEIGHT - DEPTH_STEP * depth as f64).max(MIN_WEIGHT);
    format!("{w:.6}").parse().unwrap_or(w)
}

/// Every `.lnk` under `root` with its folder depth, skipping whatever cannot be listed.
fn walk(root: &Path) -> Vec<(PathBuf, usize)> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((folder, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let Ok(read) = std::fs::read_dir(&folder) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for entry in entries {
            // `is_dir` follows links, as Python's `Path.is_dir` does.
            if entry.is_dir() {
                stack.push((entry, depth + 1));
            } else if entry
                .extension()
                .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("lnk"))
            {
                out.push((entry, depth));
            }
        }
    }
    out
}

/// Assign each row a unique slug key, suffixing a collision with a readable ordinal.
fn unique_keys(apps: Vec<App>) -> Vec<App> {
    let mut taken: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(apps.len());
    for app in apps {
        let base = slug(&app.label);
        let base = if base.is_empty() {
            "app".to_string()
        } else {
            base
        };
        let mut key = base.clone();
        let mut index = 2;
        while taken.contains(&key) {
            key = format!("{base}_{index}");
            index += 1;
        }
        taken.insert(key.clone());
        out.push(App {
            key,
            source: SOURCE.to_string(),
            ..app
        });
    }
    out
}

/// Every launchable shortcut under `roots`, flattened, filtered and deduplicated by label.
///
/// Nested folders collapse -- `Programs/Google/Chrome/Chrome.lnk` is just "Chrome" -- with the
/// nesting surviving only as a lower weight. Two roots usually offer the same app twice; the
/// shorter path wins, which is an arbitrary but stable tiebreak (the alternative, preferring the
/// user Start Menu, is no more principled and changes with the profile).
///
/// A missing root, an unreadable folder, or no Start Menu at all yields fewer rows, never an error.
pub fn read_start_menu(roots: &[PathBuf]) -> Vec<App> {
    let mut best: HashMap<String, ((usize, String), App)> = HashMap::new();
    for root in roots {
        for (file, depth) in walk(root) {
            let relative = file
                .strip_prefix(root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| PathBuf::from(file.file_name().unwrap_or_default()));
            let label = label_for(&file);
            if dropped(&label, &relative) {
                continue;
            }
            let text = file.to_string_lossy().into_owned();
            let rank = (text.chars().count(), text.clone());
            let app = App {
                key: String::new(),
                label: label.clone(),
                url: text,
                weight: weight_for(depth),
                source: SOURCE.to_string(),
            };
            let lowered = label.to_lowercase();
            let replace = match best.get(&lowered) {
                None => true,
                Some((existing, _)) => rank < *existing,
            };
            if replace {
                best.insert(lowered, (rank, app));
            }
        }
    }
    let mut ordered: Vec<App> = best.into_values().map(|(_, app)| app).collect();
    ordered.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
            .then_with(|| a.url.cmp(&b.url))
    });
    unique_keys(ordered)
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: isize,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
    ) -> isize;
}

const SW_SHOWNORMAL: i32 = 1;

/// Hand a path to the shell's default verb, as Python's `os.startfile` does.
fn shell_open(path: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call; every other pointer
    // argument is null, which ShellExecuteW documents as "use the default".
    let code = unsafe {
        ShellExecuteW(
            0,
            std::ptr::null(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW reports success as any value above 32.
    code > 32
}

/// Open `app` through the shell, returning false instead of failing when it cannot be opened.
///
/// `ShellExecuteW` on the `.lnk` lets the shell resolve the shortcut, which is what makes this work
/// for Store apps and installers whose real target is not an exe path at all. The argument is an
/// `App` from this catalog: no command line is ever assembled from a model's text.
pub fn launch(app: &App) -> bool {
    launch_with(app, shell_open)
}

/// `launch` with the shell call injected, so a test never opens a real application.
pub fn launch_with(app: &App, opener: impl FnOnce(&str) -> bool) -> bool {
    if app.url.is_empty() {
        return false;
    }
    opener(&app.url)
}

// ---------------------------------------------------------------------- caching

/// Where the built app list is cached.
pub fn cache_path() -> PathBuf {
    crate::catalog::local_dir().join("apps.json")
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One cached row back into an `App`, or `None` when the row is not usable.
fn to_app(entry: &Value) -> Option<App> {
    let obj = entry.as_object()?;
    let text = |name: &str| -> Option<String> {
        obj.get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(App {
        key: text("key")?,
        label: text("label")?,
        url: text("url")?,
        source: text("source")?,
        weight: obj.get("weight").and_then(Value::as_f64)?,
    })
}

/// The cached apps if the file is fresh and parses; otherwise `None` and the caller rebuilds.
fn read_cache(path: &Path) -> Option<Vec<App>> {
    let bytes = std::fs::read(path).ok()?;
    let data: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).ok()?;
    let obj = data.as_object()?;
    let built = obj.get("built").and_then(Value::as_f64)?;
    if now_seconds() - built > CACHE_SECONDS {
        return None;
    }
    let apps: Vec<App> = obj
        .get("apps")?
        .as_array()?
        .iter()
        .filter_map(to_app)
        .collect();
    if apps.is_empty() {
        None
    } else {
        Some(apps)
    }
}

/// Best-effort cache write. A read-only or missing directory is not worth an exception.
fn write_cache(path: &Path, apps: &[App]) {
    let payload = json!({"built": now_seconds(), "apps": apps});
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(text) = serde_json::to_string_pretty(&payload) {
        let _ = std::fs::write(path, text);
    }
}

/// The app catalog, from cache when it is fresh and otherwise rebuilt and re-cached.
///
/// Rebuilds on `refresh=true`, when the cache is absent, corrupt, or more than 24 hours old.
/// Walking two Start Menu trees is far too slow to repeat on every step of a run.
pub fn list_apps(refresh: bool) -> Vec<App> {
    list_apps_with(refresh, &start_menu_roots(), &cache_path())
}

/// `list_apps` with explicit roots and cache file.
pub fn list_apps_with(refresh: bool, roots: &[PathBuf], cache: &Path) -> Vec<App> {
    if !refresh {
        if let Some(cached) = read_cache(cache) {
            return cached;
        }
    }
    let apps = read_start_menu(roots);
    write_cache(cache, &apps);
    apps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::tests::TempDir;
    use crate::shortlist::shortlist;

    fn slug_shape() -> Regex {
        Regex::new(r"^[a-z0-9]+(?:_[a-z0-9]+)*$").unwrap()
    }

    /// Create each `.lnk` (given as a relative path) under `root`.
    fn make(root: &Path, relative: &[&str]) {
        for item in relative {
            let path = root.join(item);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "fake shortcut").unwrap();
        }
    }

    fn labels(found: &[App]) -> HashSet<String> {
        found.iter().map(|a| a.label.clone()).collect()
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn read(root: &Path) -> Vec<App> {
        read_start_menu(&[root.to_path_buf()])
    }

    /// Python's `str.title()` for the folder names used below.
    fn title(text: &str) -> String {
        let mut out = String::new();
        let mut prev = false;
        for ch in text.chars() {
            if ch.is_alphabetic() {
                if prev {
                    out.extend(ch.to_lowercase());
                } else {
                    out.extend(ch.to_uppercase());
                }
                prev = true;
            } else {
                out.push(ch);
                prev = false;
            }
        }
        out
    }

    #[test]
    fn nested_folders_are_flattened() {
        let t = TempDir::new("apps");
        make(
            t.path(),
            &[
                "WhatsApp.lnk",
                "Google/Chrome/Chrome.lnk",
                "A/B/C/Deep Tool.lnk",
            ],
        );
        assert_eq!(
            labels(&read(t.path())),
            set(&["WhatsApp", "Chrome", "Deep Tool"])
        );
    }

    #[test]
    fn shortcut_suffix_is_stripped() {
        let t = TempDir::new("apps");
        make(t.path(), &["Visual Studio Code - Shortcut.lnk"]);
        let found: Vec<(String, String)> = read(t.path())
            .into_iter()
            .map(|a| (a.label, a.key))
            .collect();
        assert_eq!(
            found,
            vec![("Visual Studio Code".into(), "visual_studio_code".into())]
        );
    }

    #[test]
    fn only_lnk_files_are_read() {
        let t = TempDir::new("apps");
        make(t.path(), &["Real.lnk"]);
        std::fs::write(t.path().join("Notes.txt"), "hi").unwrap();
        std::fs::write(t.path().join("Thing.url"), "hi").unwrap();
        assert_eq!(labels(&read(t.path())), set(&["Real"]));
    }

    #[test]
    fn noise_names_are_rejected() {
        for name in [
            "Uninstall Steam.lnk",
            "Uninstaller.lnk",
            "Remove Python.lnk",
            "Setup.lnk",
            "Readme.lnk",
            "Release Notes.lnk",
            "Documentation.lnk",
            "Help.lnk",
            "License.lnk",
            "Node.js Website.lnk",
            "Homepage.lnk",
            "Home Page.lnk",
        ] {
            let t = TempDir::new("apps");
            make(t.path(), &[name, "Keeper.lnk"]);
            assert_eq!(labels(&read(t.path())), set(&["Keeper"]), "{name}");
        }
    }

    #[test]
    fn dropped_subtrees_are_rejected() {
        for folder in DROP_FOLDERS {
            let t = TempDir::new("apps");
            let f = title(folder);
            make(
                t.path(),
                &[
                    &format!("{f}/Something.lnk"),
                    &format!("{f}/Nested/Deeper.lnk"),
                    "Keeper.lnk",
                ],
            );
            assert_eq!(labels(&read(t.path())), set(&["Keeper"]), "{folder}");
        }
    }

    #[test]
    fn a_loose_shortcut_to_a_dropped_folder_is_rejected() {
        for folder in DROP_FOLDERS {
            let t = TempDir::new("apps");
            make(t.path(), &[&format!("{}.lnk", title(folder)), "Keeper.lnk"]);
            assert_eq!(labels(&read(t.path())), set(&["Keeper"]), "{folder}");
        }
    }

    /// These fight the agent for the keyboard, the focus or the screen, so they must never be
    /// offered.
    #[test]
    fn accessibility_tools_are_never_launch_targets() {
        for name in [
            "Magnify",
            "Narrator",
            "On-Screen Keyboard",
            "VoiceAccess",
            "LiveCaptions",
        ] {
            let t = TempDir::new("apps");
            make(
                t.path(),
                &[
                    &format!("Accessibility/{name}.lnk"),
                    &format!("{name}.lnk"),
                    "Keeper.lnk",
                ],
            );
            assert_eq!(labels(&read(t.path())), set(&["Keeper"]), "{name}");
        }
    }

    #[test]
    fn dedup_by_label_keeps_the_shorter_path() {
        let t = TempDir::new("apps");
        let user = t.path().join("user");
        let machine = t.path().join("machine-programs-folder");
        make(&user, &["Chrome.lnk"]);
        make(&machine, &["Google/Chrome/Chrome.lnk"]);
        let found = read_start_menu(&[machine, user.clone()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].url, user.join("Chrome.lnk").to_string_lossy());
    }

    #[test]
    fn depth_lowers_weight() {
        let t = TempDir::new("apps");
        make(
            t.path(),
            &["Top.lnk", "One/Mid.lnk", "One/Two/Three/Low.lnk"],
        );
        let weights: HashMap<String, f64> = read(t.path())
            .into_iter()
            .map(|a| (a.label, a.weight))
            .collect();
        assert!(weights["Top"] > weights["Mid"] && weights["Mid"] > weights["Low"]);
        assert_eq!(weights["Top"], TOP_WEIGHT);
        assert!(weights["Low"] >= MIN_WEIGHT);
    }

    #[test]
    fn weight_is_bounded_at_extreme_depth() {
        let t = TempDir::new("apps");
        make(t.path(), &["a/b/c/d/e/f/Buried.lnk"]);
        let found = read(t.path());
        assert!(!found.is_empty() && found[0].weight == MIN_WEIGHT);
    }

    #[test]
    fn keys_are_unique_and_slug_shaped() {
        let t = TempDir::new("apps");
        make(
            t.path(),
            &[
                "Visual Studio Code.lnk",
                "One/Visual Studio Code!.lnk",
                "WhatsApp.lnk",
                "Two/Micro$oft Edge.lnk",
            ],
        );
        let found = read(t.path());
        let keys: Vec<&str> = found.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys.iter().collect::<HashSet<_>>().len(), found.len());
        let shape = slug_shape();
        assert!(keys.iter().all(|k| shape.is_match(k)), "{keys:?}");
        assert!(keys.contains(&"whatsapp"));
        assert!(keys.contains(&"visual_studio_code"));
    }

    #[test]
    fn missing_root_is_empty() {
        let t = TempDir::new("apps");
        assert!(read(&t.path().join("nope")).is_empty());
        assert!(read_start_menu(&[]).is_empty());
    }

    #[test]
    fn missing_root_among_real_ones_still_yields_the_others() {
        let t = TempDir::new("apps");
        make(t.path(), &["Keeper.lnk"]);
        let found = read_start_menu(&[t.path().join("nope"), t.path().to_path_buf()]);
        assert_eq!(labels(&found), set(&["Keeper"]));
    }

    #[test]
    fn start_menu_roots_without_environment() {
        assert!(start_menu_roots_from(None, None).is_empty());
    }

    #[test]
    fn start_menu_roots_from_environment() {
        let t = TempDir::new("apps");
        let roots =
            start_menu_roots_from(Some(t.path().join("roaming")), Some(t.path().join("pd")));
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().all(|r| r.file_name().unwrap() == "Programs"));
    }

    // ------------------------------------------------------------- launching

    #[test]
    fn launch_calls_the_shell_with_the_catalog_path() {
        let mut seen: Vec<String> = Vec::new();
        let app = App {
            key: "whatsapp".into(),
            label: "WhatsApp".into(),
            url: r"C:\fake\WhatsApp.lnk".into(),
            weight: 1.0,
            source: "app".into(),
        };
        assert!(launch_with(&app, |p| {
            seen.push(p.to_string());
            true
        }));
        assert_eq!(seen, vec![r"C:\fake\WhatsApp.lnk".to_string()]);
    }

    #[test]
    fn launch_returns_false_when_the_shell_fails() {
        let app = App {
            key: "gone".into(),
            label: "Gone".into(),
            url: r"C:\fake\Gone.lnk".into(),
            weight: 1.0,
            source: "app".into(),
        };
        assert!(!launch_with(&app, |_| false));
    }

    #[test]
    fn launch_returns_false_for_an_empty_path() {
        let app = App {
            key: "x".into(),
            label: "X".into(),
            url: String::new(),
            weight: 1.0,
            source: "app".into(),
        };
        assert!(!launch_with(&app, |_| panic!("must not be called")));
        // The real shell path short-circuits the same way, so this opens nothing.
        assert!(!launch(&app));
    }

    // --------------------------------------------------------------- caching

    struct Env {
        tmp: TempDir,
    }

    impl Env {
        fn new() -> Self {
            Env {
                tmp: TempDir::new("appcache"),
            }
        }
        fn menu(&self) -> PathBuf {
            self.tmp.path().join("menu")
        }
        fn cache(&self) -> PathBuf {
            self.tmp
                .path()
                .join("local")
                .join("winclicker")
                .join("apps.json")
        }
        fn list(&self, refresh: bool) -> Vec<App> {
            list_apps_with(refresh, &[self.menu()], &self.cache())
        }
    }

    #[test]
    fn list_apps_caches_and_reuses() {
        let env = Env::new();
        make(&env.menu(), &["WhatsApp.lnk"]);
        assert_eq!(labels(&env.list(true)), set(&["WhatsApp"]));
        assert!(env.cache().is_file());
        // A changed Start Menu is not seen until a refresh, which is the point of the cache.
        make(&env.menu(), &["Spotify.lnk"]);
        assert_eq!(labels(&env.list(false)), set(&["WhatsApp"]));
        assert_eq!(labels(&env.list(true)), set(&["WhatsApp", "Spotify"]));
    }

    #[test]
    fn corrupt_cache_rebuilds() {
        let env = Env::new();
        make(&env.menu(), &["WhatsApp.lnk"]);
        std::fs::create_dir_all(env.cache().parent().unwrap()).unwrap();
        std::fs::write(env.cache(), "{ not json at all").unwrap();
        assert_eq!(labels(&env.list(false)), set(&["WhatsApp"]));
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(env.cache()).unwrap()).unwrap();
        assert!(!written["apps"].as_array().unwrap().is_empty());
    }

    #[test]
    fn stale_cache_rebuilds() {
        let env = Env::new();
        make(&env.menu(), &["Spotify.lnk"]);
        std::fs::create_dir_all(env.cache().parent().unwrap()).unwrap();
        let stale = json!({
            "built": now_seconds() - CACHE_SECONDS - 60.0,
            "apps": [{"key": "old", "label": "Old", "url": "x.lnk", "weight": 1.0, "source": "app"}],
        });
        std::fs::write(env.cache(), stale.to_string()).unwrap();
        assert_eq!(labels(&env.list(false)), set(&["Spotify"]));
    }

    #[test]
    fn missing_cache_rebuilds() {
        let env = Env::new();
        make(&env.menu(), &["Steam.lnk"]);
        assert!(!env.cache().exists());
        assert_eq!(labels(&env.list(false)), set(&["Steam"]));
    }

    #[test]
    fn cache_rows_of_the_wrong_shape_are_ignored() {
        let env = Env::new();
        make(&env.menu(), &["Steam.lnk"]);
        std::fs::create_dir_all(env.cache().parent().unwrap()).unwrap();
        let junk = json!({"built": now_seconds(), "apps": ["nope", 3, {}]});
        std::fs::write(env.cache(), junk.to_string()).unwrap();
        assert_eq!(labels(&env.list(false)), set(&["Steam"]));
    }

    #[test]
    fn empty_start_menu_yields_no_apps() {
        let env = Env::new();
        assert!(
            list_apps_with(true, &[env.tmp.path().join("does-not-exist")], &env.cache()).is_empty()
        );
    }

    // ---------------------------------------------------------- the protocol

    /// `App` satisfies what `shortlist` needs, so the site ranker works on apps with no change.
    #[test]
    fn apps_can_be_ranked_by_shortlist_unchanged() {
        let t = TempDir::new("apps");
        make(
            t.path(),
            &[
                "WhatsApp.lnk",
                "Spotify.lnk",
                "Visual Studio Code.lnk",
                "Deep/Calculator.lnk",
            ],
        );
        let found = read(t.path());
        let ranked: Vec<App> = shortlist("open whatsapp", &found, 3);
        assert!(!ranked.is_empty() && ranked[0].label == "WhatsApp");
        assert!(!labels(&ranked).contains("Calculator"));
    }
}
