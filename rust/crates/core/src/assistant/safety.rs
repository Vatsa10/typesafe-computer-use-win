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


// ---------------------------------------------------------------- destructive commands

/// One parsed shell command line: its statements as words with quotes removed.
struct Parsed {
    statements: Vec<Vec<String>>,
    /// A redirect, an unterminated quote, an expansion in double quotes or a dynamic call: the
    /// command cannot be judged by its words, so it counts as destructive.
    opaque: bool,
}

/// Split a PowerShell or cmd line into statements the way `readonly` lexes it: quotes group
/// words, and `; | & ( ) { }` and line breaks end a statement, so a destructive cmdlet inside a
/// script block, a subexpression or a pipeline stage is found as the head of its own statement.
fn parse_statements(src: &str) -> Parsed {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Parsed {
        statements: Vec::new(),
        opaque: false,
    };
    let mut cur: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut has_word = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => {
                if has_word {
                    cur.push(std::mem::take(&mut word));
                    has_word = false;
                }
                i += 1;
            }
            '\'' | '"' => {
                has_word = true;
                i += 1;
                loop {
                    match chars.get(i) {
                        None => {
                            out.opaque = true;
                            break;
                        }
                        Some(&q) if q == c => {
                            if c == '\'' && chars.get(i + 1) == Some(&'\'') {
                                word.push('\'');
                                i += 2;
                                continue;
                            }
                            i += 1;
                            break;
                        }
                        Some(&x) => {
                            if c == '"' && (x == '$' || x == '`') {
                                out.opaque = true;
                            }
                            word.push(x);
                            i += 1;
                        }
                    }
                }
            }
            '`' => i += 1, // an escape: Rem`ove-Item is Remove-Item
            '>' => {
                out.opaque = true;
                i += 1;
            }
            ';' | '|' | '&' | '(' | ')' | '{' | '}' | '\n' | '\r' => {
                if has_word {
                    cur.push(std::mem::take(&mut word));
                    has_word = false;
                }
                if c == '&' {
                    // The call operator on something not spelled out cannot be judged.
                    let next = chars[i + 1..]
                        .iter()
                        .find(|x| **x != ' ' && **x != '\t' && **x != '&');
                    if matches!(next, Some('$' | '(' | '{')) {
                        out.opaque = true;
                    }
                }
                if !cur.is_empty() {
                    out.statements.push(std::mem::take(&mut cur));
                }
                i += 1;
            }
            _ => {
                has_word = true;
                word.push(c);
                i += 1;
            }
        }
    }
    if has_word {
        cur.push(word);
    }
    if !cur.is_empty() {
        out.statements.push(cur);
    }
    out
}

const DESTRUCTIVE_HEADS: &[&str] = &[
    // delete
    "remove-item", "ri", "rm", "del", "erase", "rmdir", "rd", "delete",
    // overwrite, empty or write
    "set-content", "sc", "out-file", "clear-content", "clc", "add-content", "ac", "tee-object",
    "tee", "new-item", "ni", "set-item", "si", "clear-item", "cli", "clear-recyclebin",
    // move, rename, copy over a target
    "move-item", "mi", "mv", "move", "rename-item", "ren", "rni", "copy-item", "cp", "copy",
    "cpi", "xcopy", "robocopy", "expand-archive", "compress-archive",
    // registry and machine state
    "set-itemproperty", "sp", "new-itemproperty", "remove-itemproperty", "rp",
    "rename-itemproperty", "clear-itemproperty", "regedit", "stop-computer", "restart-computer",
    "shutdown", "stop-process", "spps", "kill", "taskkill", "format", "diskpart", "cipher",
    "takeown", "icacls", "attrib", "fsutil", "vssadmin", "bcdedit", "sdelete", "wmic", "schtasks",
    // run something else, fetch things
    "invoke-expression", "iex", "start-process", "start", "saps", "invoke-webrequest", "iwr",
    "curl", "wget", "invoke-restmethod", "irm", "invoke-command", "icm",
    // shells and interpreters can run anything
    "cmd", "powershell", "pwsh", "wsl", "bash", "sh", "mshta", "wscript", "cscript", "rundll32",
    "regsvr32", "msiexec", "python", "node",
];

const FORMAT_DISPLAY: &[&str] = &[
    "format-table", "ft", "format-list", "fl", "format-wide", "fw", "format-custom", "format-hex",
];
const VCS_DESTRUCTIVE: &[&str] = &[
    "clean", "reset", "rm", "checkout", "restore", "rebase", "push", "stash", "branch", "gc", "prune",
];
const REG_SAFE: &[&str] = &["query", "compare"];

/// The command's name: lowercase, without its folder or `.exe`.
fn head_name(raw: &str) -> String {
    let l = raw.to_lowercase().replace('\\', "/");
    let base = l.rsplit('/').next().unwrap_or(&l).to_string();
    for ext in [".exe", ".com"] {
        if let Some(b) = base.strip_suffix(ext) {
            return b.to_string();
        }
    }
    base
}

/// Whether a command deletes, overwrites, moves over a target, formats, writes the registry,
/// changes machine state, or cannot be judged. Those ask even in full autonomy. Parsed by
/// statement head (so a script block, a pipeline stage, `cmd /c del x` and `& 'rm' x` are
/// caught), not by finding a word anywhere (so `Get-ChildItem -Filter move` is not).
pub fn destructive_command(command: &str) -> bool {
    let low = command.to_lowercase();
    // .NET and COM routes to the same effects.
    if [
        "::delete", "::move", "::write", "::copy", "::create", "::replace", "::setattr", ".delete(",
        ".deletesubkey", ".moveto(", ".remove(", ".kill(", "-membername",
    ]
    .iter()
    .any(|n| low.contains(n))
    {
        return true;
    }
    let parsed = parse_statements(command);
    if parsed.opaque {
        return true;
    }
    for st in &parsed.statements {
        let head = head_name(&st[0]);
        let rest: Vec<String> = st[1..].iter().map(|w| w.to_lowercase()).collect();
        let raw = st[0].to_lowercase();
        // A script file or a variable head runs unknown things.
        if [".ps1", ".bat", ".cmd", ".vbs", ".js", ".wsf", ".msi"]
            .iter()
            .any(|e| raw.ends_with(e))
            || head.starts_with('$')
        {
            return true;
        }
        if DESTRUCTIVE_HEADS.contains(&head.as_str()) {
            return true;
        }
        if ["remove-", "set-", "clear-", "invoke-", "new-", "rename-"]
            .iter()
            .any(|p| head.starts_with(p))
        {
            return true;
        }
        if head.starts_with("format-") && !FORMAT_DISPLAY.contains(&head.as_str()) {
            return true;
        }
        if head == "reg" && !rest.first().is_some_and(|s| REG_SAFE.contains(&s.as_str())) {
            return true;
        }
        if head == "git"
            && rest
                .iter()
                .find(|w| !w.starts_with('-'))
                .is_some_and(|s| VCS_DESTRUCTIVE.contains(&s.as_str()))
        {
            return true;
        }
        // A registry drive named as an argument is a write target for most commands.
        if rest
            .iter()
            .any(|w| w.starts_with("hklm:") || w.starts_with("hkcu:") || w.starts_with("registry::"))
            && !matches!(
                head.as_str(),
                "get-itemproperty" | "gp" | "get-childitem" | "gci" | "ls" | "dir" | "get-item" | "gi" | "test-path"
            )
        {
            return true;
        }
        if rest.iter().any(|w| matches!(w.as_str(), "delete" | "remove" | "kill")) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------- passwords and payments

/// The text as lowercase words with a space on each side, for whole-phrase matching.
fn words(s: &str) -> String {
    format!(
        " {} ",
        s.to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn has_phrase(text: &str, phrases: &[&str]) -> bool {
    let w = words(text);
    phrases.iter().any(|p| w.contains(&format!(" {p} ")))
}

/// A form field that takes card or bank details.
pub fn payment_field(label: &str) -> bool {
    has_phrase(
        label,
        &[
            "card number", "credit card", "debit card", "card no", "cc number", "cardnumber", "cvv",
            "cvc", "cvv2", "security code", "expiry", "expiry date", "expiration", "expiration date",
            "exp date", "card expiry", "cardholder", "card holder", "name on card", "iban",
            "routing number", "account number", "sort code", "swift", "bic",
        ],
    )
}

/// Page text that marks a checkout or payment page.
pub fn checkout_text(text: &str) -> bool {
    has_phrase(
        text,
        &[
            "checkout", "check out", "order summary", "payment method", "payment details",
            "billing address", "order total", "your cart", "shopping cart", "place your order",
        ],
    )
}

/// A button that spends money, given whether the page reads as a checkout. Unambiguous phrases
/// count anywhere; a bare "Pay", "Buy", "Purchase" or "Order" only on a checkout page (a button
/// that starts with the word "Pay" counts anywhere).
pub fn payment_button(label: &str, checkout_page: bool) -> bool {
    if has_phrase(
        label,
        &[
            "place order", "place your order", "buy now", "pay now", "complete purchase",
            "complete order", "confirm order", "confirm payment", "submit payment", "make payment",
            "complete payment", "pay securely", "proceed to payment", "purchase now", "order now",
            "subscribe and pay",
        ],
    ) {
        return true;
    }
    let w = words(label);
    let weak = w
        .split_whitespace()
        .any(|t| matches!(t, "pay" | "buy" | "purchase" | "order" | "checkout" | "donate"));
    weak && (checkout_page || w.starts_with(" pay "))
}

/// Whether a page view is a payment context: card-type fields, or checkout wording.
pub fn payment_context(fields: &[String], texts: &[String], buttons: &[String]) -> bool {
    fields.iter().any(|f| payment_field(f))
        || texts.iter().any(|t| checkout_text(t))
        || buttons.iter().any(|b| checkout_text(b))
}

/// A `pointer_run` goal that spends money or enters card details.
pub fn payment_goal(goal: &str) -> bool {
    has_phrase(
        goal,
        &[
            "pay", "pay for", "buy", "purchase", "checkout", "check out", "place order",
            "place the order", "place an order", "order now", "card number", "credit card",
            "debit card", "cvv", "cvc", "billing", "subscribe", "donate", "payment",
            "complete the order", "complete the purchase", "bank account", "transfer money",
            "wire money", "send money",
        ],
    )
}

/// A goal that asks for a password or code to be typed: refused outright.
pub fn password_goal(goal: &str) -> bool {
    let typing = has_phrase(
        goal,
        &["type", "enter", "fill", "input", "paste", "put", "write", "log in", "login", "sign in", "use"],
    );
    typing
        && has_phrase(
            goal,
            &["password", "passwords", "passcode", "passphrase", "pin", "one time code", "otp", "2fa code", "cvv"],
        )
        && !has_phrase(goal, &["password manager"])
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

    #[test]
    fn hostile_destructive_commands_are_caught() {
        for c in [
            "Remove-Item a",
            "REMOVE-ITEM -Recurse -Force C:\\x",
            "Rem`ove-Item a",
            "ri a",
            "rm -r x",
            "del /q *.txt",
            "erase a.txt",
            "rd /s /q C:\\x",
            "rmdir x",
            "Clear-Content a.txt",
            "clc a.txt",
            "Move-Item a b -Force",
            "mv a b",
            "Format-Volume -DriveLetter D",
            "format D: /q",
            "Get-ChildItem | Remove-Item",
            "Get-ChildItem | ForEach-Object { Remove-Item $_ }",
            "Get-ChildItem | % { rm $_.FullName }",
            "Get-ChildItem | ForEach-Object { $_.Delete() }",
            "Get-ChildItem | ForEach-Object -MemberName Delete",
            "Write-Output hi; Remove-Item a",
            "Write-Output hi && del a",
            "Get-Date | Out-File x.txt",
            "Get-Date > x.txt",
            "echo hi >> x.txt",
            "& 'Remove-Item' a",
            "& \"rm\" a",
            "& $cmd a",
            "& (Get-Command rm) a",
            "echo $(Remove-Item a)",
            "Write-Output \"$(Remove-Item a)\"",
            "cmd /c del a",
            "C:\\Windows\\System32\\cmd.exe /c rd /s x",
            "powershell -c \"rm a\"",
            "iex 'rm a'",
            "Invoke-Expression 'rm a'",
            "reg add HKCU\\Software\\X /v a /d 1",
            "reg.exe delete HKLM\\Software\\X /f",
            "Set-ItemProperty -Path HKCU:\\Software\\X -Name a -Value 1",
            "New-ItemProperty HKLM:\\Software\\X -Name a",
            "Remove-ItemProperty HKCU:\\Software\\X a",
            "[System.IO.File]::Delete('a')",
            "[IO.Directory]::Move('a','b')",
            ".\\clean.ps1",
            "git clean -fdx",
            "git reset --hard",
            "Stop-Process -Name x",
            "Copy-Item a b",
            "Rename-Item a b",
            "unterminated 'quote",
        ] {
            assert!(destructive_command(c), "{c}");
        }
        for c in [
            "Get-ChildItem",
            "Get-Date",
            "Get-ChildItem | Sort-Object Length",
            "Get-ChildItem -Filter move",
            "Get-Process | Sort-Object CPU | Format-Table Name, CPU",
            "Start-Sleep 0; Write-Output hi",
            "Get-Content a.txt -TotalCount 5",
            "reg query HKCU\\Software\\X",
            "git status",
            "Get-ItemProperty HKCU:\\Software\\X",
        ] {
            assert!(!destructive_command(c), "{c}");
        }
    }

    #[test]
    fn payment_and_password_detection() {
        for l in ["Card number", "CVV", "Security code", "Expiry date (MM/YY)", "Name on card", "IBAN"] {
            assert!(payment_field(l), "{l}");
        }
        for l in ["Full name", "Email", "Expected salary", "Cover letter"] {
            assert!(!payment_field(l), "{l}");
        }
        assert!(payment_button("Place order", false) && payment_button("Buy now", false));
        assert!(payment_button("Pay $20.00", false));
        assert!(payment_button("Order", true) && !payment_button("Order", false));
        assert!(!payment_button("Next", true) && !payment_button("Submit application", false));
        assert!(!payment_button("Paypal help", false));
        assert!(payment_goal("buy the cheapest laptop") && payment_goal("go to checkout and place order"));
        assert!(!payment_goal("open notepad") && !payment_goal("scroll down"));
        assert!(password_goal("type my password into the login box"));
        assert!(!password_goal("open the password manager"));
        assert!(checkout_text("Order summary") && !checkout_text("Thanks for applying"));
    }
}
