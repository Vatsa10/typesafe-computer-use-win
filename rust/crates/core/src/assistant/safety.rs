//! What the file tools may touch. Reads are free inside the user's profile; the sensitive things
//! are never readable at all: `.env` files, browser password and cookie stores, SSH keys.

use std::path::{Component, Path, PathBuf};

/// A path as one lowercase string with forward slashes and no `\\?\` prefix.
pub fn normal(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/").to_lowercase();
    s.strip_prefix("//?/").unwrap_or(&s).to_string()
}

const BROWSER_DIRS: [&str; 8] = [
    "/google/chrome/user data",
    "/microsoft/edge/user data",
    "/bravesoftware/brave-browser/user data",
    "/vivaldi/user data",
    "/chromium/user data",
    "/opera software",
    "/mozilla/firefox/profiles",
    "/.mozilla/firefox",
];

const BROWSER_FILES: [&str; 8] = [
    "login data",
    "login data for account",
    "cookies",
    "web data",
    "local state",
    "logins.json",
    "key4.db",
    "cookies.sqlite",
];

const KEY_PREFIXES: [&str; 4] = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];

/// Why this path must never be read, or None when it is fine to.
pub fn forbidden(p: &Path) -> Option<&'static str> {
    let norm = normal(p);
    let parts: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
    let name = parts.last().copied().unwrap_or("");
    if name == ".env" || name.starts_with(".env.") {
        return Some("a .env file holds secrets");
    }
    if parts.contains(&".ssh") {
        return Some("SSH keys are never read");
    }
    if KEY_PREFIXES
        .iter()
        .any(|k| name.starts_with(k) && !name.ends_with(".pub"))
    {
        return Some("private keys are never read");
    }
    let with_slash = format!("/{}/", parts.join("/"));
    if BROWSER_DIRS
        .iter()
        .any(|d| with_slash.contains(&format!("{d}/")))
        || BROWSER_FILES.contains(&name)
    {
        return Some("browser password and cookie stores are never read");
    }
    None
}

/// Does a shell command mention something that must not be read? A coarse text check: the command
/// still needs the user's yes, and this keeps the model from even asking for these.
pub fn command_mentions_forbidden(command: &str) -> Option<&'static str> {
    let c = command.replace('\\', "/").to_lowercase();
    let hit = |needle: &str| c.contains(needle);
    if c.split(|ch: char| !(ch.is_alphanumeric() || ch == '.' || ch == '_'))
        .any(|w| w == ".env" || w.starts_with(".env."))
    {
        return Some("a .env file holds secrets");
    }
    if hit("/.ssh") || hit("id_rsa") || hit("id_ed25519") || hit("id_ecdsa") {
        return Some("SSH keys are never read");
    }
    if BROWSER_DIRS.iter().any(|d| hit(d))
        || hit("login data")
        || hit("cookies.sqlite")
        || hit("logins.json")
        || hit("key4.db")
    {
        return Some("browser password and cookie stores are never read");
    }
    None
}

/// Resolve `raw` (relative paths are under `profile`) to a real absolute path that lies inside the
/// profile and is not forbidden. The path need not exist yet (for a write): the nearest existing
/// ancestor is resolved, which follows symlinks, and the rest must have no `..`.
pub fn resolve(raw: &str, profile: &Path) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("no path given".into());
    }
    let profile = profile
        .canonicalize()
        .map_err(|e| format!("the user profile is not readable: {e}"))?;
    let given = PathBuf::from(raw);
    let full = if given.is_absolute() {
        given
    } else {
        profile.join(given)
    };
    // A `..` after a part that does not exist cannot be resolved without guessing.
    let mut existing = full.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
            return Err("that path does not exist".into());
        };
        rest.push(name);
        existing = match existing.parent() {
            Some(p) => p.to_path_buf(),
            None => return Err("that path does not exist".into()),
        };
    }
    if full.components().any(|c| c == Component::ParentDir) && !rest.is_empty() {
        return Err("paths with .. must exist".into());
    }
    let mut real = existing
        .canonicalize()
        .map_err(|e| format!("cannot resolve the path: {e}"))?;
    for part in rest.into_iter().rev() {
        real.push(part);
    }
    let (r, p) = (normal(&real), normal(&profile));
    if r != p && !r.starts_with(&format!("{}/", p.trim_end_matches('/'))) {
        return Err("that path is outside your user folder".into());
    }
    if let Some(why) = forbidden(&real) {
        return Err(format!("refused: {why}"));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("assist-safety-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_sensitive_things_are_forbidden() {
        for p in [
            "C:/Users/me/project/.env",
            "C:/Users/me/project/.env.local",
            "C:\\Users\\me\\.ssh\\config",
            "C:/Users/me/.ssh/id_ed25519",
            "C:/Users/me/keys/id_rsa",
            "C:\\Users\\me\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Login Data",
            "C:\\Users\\me\\AppData\\Local\\Microsoft\\Edge\\User Data\\Default\\Network\\Cookies",
            "C:\\Users\\me\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\x.default\\key4.db",
            "C:/Users/me/AppData/Local/Google/Chrome/User Data/Local State",
        ] {
            assert!(forbidden(Path::new(p)).is_some(), "{p}");
        }
        for p in [
            "C:/Users/me/notes.txt",
            "C:/Users/me/env.md",
            "C:/Users/me/.envrc-notes/readme.md",
            "C:/Users/me/keys/id_ed25519.pub",
        ] {
            assert!(forbidden(Path::new(p)).is_none(), "{p}");
        }
    }

    #[test]
    fn commands_naming_them_are_caught() {
        for c in [
            "type .env",
            "cat C:\\Users\\me\\.ssh\\id_rsa",
            "copy \"C:\\Users\\me\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Login Data\" x",
            "Get-Content ./app/.env.production",
        ] {
            assert!(command_mentions_forbidden(c).is_some(), "{c}");
        }
        assert!(command_mentions_forbidden("dir C:\\Users\\me\\Documents").is_none());
        assert!(command_mentions_forbidden("echo environment").is_none());
    }

    #[test]
    fn resolve_keeps_to_the_profile_and_refuses_secrets() {
        let home = tmp("home");
        std::fs::write(home.join("notes.txt"), "hi").unwrap();
        std::fs::write(home.join(".env"), "KEY=1").unwrap();
        std::fs::create_dir_all(home.join("sub")).unwrap();
        assert!(resolve("notes.txt", &home).is_ok());
        assert!(resolve(&home.join("notes.txt").to_string_lossy(), &home).is_ok());
        assert!(
            resolve("sub/new.txt", &home).is_ok(),
            "a path to be written"
        );
        assert!(resolve(".env", &home).unwrap_err().contains("secrets"));
        assert!(resolve("sub/../.env", &home)
            .unwrap_err()
            .contains("secrets"));
        let outside = tmp("outside");
        std::fs::write(outside.join("x.txt"), "x").unwrap();
        assert!(resolve(&outside.join("x.txt").to_string_lossy(), &home)
            .unwrap_err()
            .contains("outside"));
        assert!(resolve("../x.txt", &home).is_err());
        // Whatever Windows makes of `missing/..`, the answer is canonical and inside the profile.
        if let Ok(p) = resolve("sub/missing/../../notes.txt", &home) {
            assert!(normal(&p).starts_with(&normal(&home.canonicalize().unwrap())));
        }
        assert!(resolve("", &home).is_err());
    }
}
