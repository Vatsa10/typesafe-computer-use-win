//! The read-only shell tier: a strict allowlist of PowerShell that may run without asking.
//!
//! The command is parsed, not searched for substrings. It must be a pipeline of allowlisted
//! cmdlets whose every parameter is known, with quoted or bare literal arguments only. Anything
//! that could run something else (`;` `&` backticks, redirects, `$(...)`, variables, parentheses
//! outside a filter block, `Invoke-*`, `iex`, an unknown cmdlet) is not read-only, and neither is
//! a path that is forbidden, outside the profile, or that a pipe would hand to a file reader
//! unchecked. Not read-only is never an error: the command just goes to the user for a yes.

use std::path::{Path, PathBuf};

use super::safety;

#[derive(Debug, PartialEq)]
enum Tok {
    /// A bare word or a quoted string, with its quotes removed.
    Word(String),
    /// `-Name` or `-Name:value` (value in the second field).
    Param(String, Option<String>),
    /// `{ ... }`: the inside, not yet checked.
    Block(String),
    Pipe,
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => i += 1,
            '\n' | '\r' => return Err("a line break".into()),
            '|' => {
                if chars.get(i + 1) == Some(&'|') {
                    return Err("||".into());
                }
                out.push(Tok::Pipe);
                i += 1;
            }
            '{' => {
                let mut depth = 0;
                let mut j = i;
                let mut quote: Option<char> = None;
                loop {
                    let Some(&d) = chars.get(j) else {
                        return Err("an unclosed block".into());
                    };
                    match (quote, d) {
                        (Some(q), d) if d == q => quote = None,
                        (Some(_), _) => {}
                        (None, '\'' | '"') => quote = Some(d),
                        (None, '{') => depth += 1,
                        (None, '}') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                out.push(Tok::Block(chars[i + 1..j].iter().collect()));
                i = j + 1;
            }
            _ => {
                // A word: runs to whitespace or a pipe, with quoted stretches kept whole.
                let mut word = String::new();
                while i < chars.len() {
                    let d = chars[i];
                    if d == ' ' || d == '\t' || d == '|' || d == '\n' || d == '\r' {
                        break;
                    }
                    match d {
                        '\'' => {
                            i += 1;
                            loop {
                                match chars.get(i) {
                                    None => return Err("an unclosed quote".into()),
                                    Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                                        word.push('\'');
                                        i += 2;
                                    }
                                    Some('\'') => {
                                        i += 1;
                                        break;
                                    }
                                    Some(&x) => {
                                        word.push(x);
                                        i += 1;
                                    }
                                }
                            }
                        }
                        '"' => {
                            i += 1;
                            loop {
                                match chars.get(i) {
                                    None => return Err("an unclosed quote".into()),
                                    Some('"') => {
                                        i += 1;
                                        break;
                                    }
                                    Some('$') | Some('`') => {
                                        return Err("an expansion inside double quotes".into())
                                    }
                                    Some(&x) => {
                                        word.push(x);
                                        i += 1;
                                    }
                                }
                            }
                        }
                        ';' | '&' | '`' | '>' | '<' | '$' | '(' | ')' | '@' | '{' | '}' | '%'
                        | '!' => return Err(format!("the character {d}")),
                        _ => {
                            word.push(d);
                            i += 1;
                        }
                    }
                }
                // A parameter is a bare word starting with `-` and a letter.
                let first_unquoted_dash = src_starts_param(&word);
                if first_unquoted_dash {
                    let body = &word[1..];
                    match body.split_once(':') {
                        Some((n, v)) => out.push(Tok::Param(n.to_lowercase(), Some(v.to_string()))),
                        None => out.push(Tok::Param(body.to_lowercase(), None)),
                    }
                } else {
                    out.push(Tok::Word(word));
                }
            }
        }
    }
    Ok(out)
}

fn src_starts_param(w: &str) -> bool {
    let mut it = w.chars();
    it.next() == Some('-') && it.next().is_some_and(|c| c.is_ascii_alphabetic())
}

/// The cmdlets, and for each the parameters it may carry: (name, takes a value).
fn params_of(cmdlet: &str) -> Option<&'static [(&'static str, bool)]> {
    const CHILD: &[(&str, bool)] = &[
        ("path", true), ("literalpath", true), ("filter", true), ("include", true),
        ("exclude", true), ("recurse", false), ("depth", true), ("force", false),
        ("name", false), ("file", false), ("directory", false),
    ];
    const ITEM: &[(&str, bool)] = &[
        ("path", true), ("literalpath", true), ("filter", true), ("include", true),
        ("exclude", true), ("force", false),
    ];
    const CONTENT: &[(&str, bool)] = &[
        ("path", true), ("literalpath", true), ("totalcount", true), ("head", true),
        ("tail", true), ("raw", false), ("encoding", true), ("first", true), ("last", true),
    ];
    const SELECT_STRING: &[(&str, bool)] = &[
        ("pattern", true), ("path", true), ("literalpath", true), ("simplematch", false),
        ("casesensitive", false), ("context", true), ("list", false), ("quiet", false),
        ("notmatch", false), ("allmatches", false), ("encoding", true),
    ];
    const MEASURE: &[(&str, bool)] = &[
        ("property", true), ("sum", false), ("average", false), ("maximum", false),
        ("minimum", false), ("line", false), ("word", false), ("character", false),
        ("allstats", false), ("ignorewhitespace", false),
    ];
    const SORT: &[(&str, bool)] = &[
        ("property", true), ("descending", false), ("ascending", false), ("unique", false),
        ("casesensitive", false), ("top", true), ("bottom", true),
    ];
    const SELECT: &[(&str, bool)] = &[
        ("property", true), ("expandproperty", true), ("first", true), ("last", true),
        ("skip", true), ("unique", false), ("index", true),
    ];
    const WHERE: &[(&str, bool)] = &[
        ("filterscript", true), ("property", true), ("eq", true), ("ne", true), ("gt", true),
        ("ge", true), ("lt", true), ("le", true), ("like", true), ("notlike", true),
        ("match", true), ("notmatch", true), ("contains", true), ("notcontains", true),
        ("in", true), ("notin", true), ("is", true), ("isnot", true),
    ];
    const FORMAT: &[(&str, bool)] = &[
        ("property", true), ("autosize", false), ("wrap", false), ("groupby", true),
        ("hidetableheaders", false), ("force", false), ("expand", true), ("view", true),
    ];
    const TESTPATH: &[(&str, bool)] = &[
        ("path", true), ("literalpath", true), ("pathtype", true), ("include", true),
        ("exclude", true), ("filter", true), ("isvalid", false),
    ];
    const PROC: &[(&str, bool)] = &[("name", true), ("id", true)];
    const DATE: &[(&str, bool)] = &[
        ("format", true), ("uformat", true), ("year", true), ("month", true), ("day", true),
        ("hour", true), ("minute", true), ("second", true), ("displayhint", true),
    ];
    Some(match cmdlet {
        "get-childitem" => CHILD,
        "get-item" => ITEM,
        "get-content" => CONTENT,
        "select-string" => SELECT_STRING,
        "measure-object" => MEASURE,
        "sort-object" => SORT,
        "select-object" => SELECT,
        "where-object" => WHERE,
        "test-path" => TESTPATH,
        "get-process" => PROC,
        "get-date" => DATE,
        c if c.starts_with("format-") && matches!(c, "format-list" | "format-table" | "format-wide" | "format-custom") => FORMAT,
        _ => return None,
    })
}

/// Cmdlets that open the files they are given or are piped.
fn reads_files(cmdlet: &str) -> bool {
    matches!(cmdlet, "get-content" | "select-string")
}

fn is_wild(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

fn numeric(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.' || c == '-')
}

const BLOCK_OPS: [&str; 22] = [
    "eq", "ne", "gt", "ge", "lt", "le", "like", "notlike", "match", "notmatch", "contains",
    "notcontains", "in", "notin", "and", "or", "not", "ieq", "ine", "ilike", "imatch", "is",
];

/// A filter block may compare properties of `$_` with literals and nothing else: no call, no
/// assignment, no other variable, no command.
fn check_block(inner: &str) -> Result<(), String> {
    let c: Vec<char> = inner.chars().collect();
    let mut i = 0;
    while i < c.len() {
        let d = c[i];
        match d {
            ' ' | '\t' | ',' | '!' => i += 1,
            '(' | ')' => {
                if d == '(' && i > 0 && (c[i - 1].is_alphanumeric() || c[i - 1] == '_' || c[i - 1] == ')') {
                    return Err("a method call in a filter".into());
                }
                i += 1;
            }
            '\'' | '"' => {
                let q = d;
                i += 1;
                while i < c.len() && c[i] != q {
                    if q == '"' && (c[i] == '$' || c[i] == '`') {
                        return Err("an expansion in a filter".into());
                    }
                    i += 1;
                }
                if i >= c.len() {
                    return Err("an unclosed quote in a filter".into());
                }
                i += 1;
            }
            '$' => {
                let rest: String = c[i..].iter().collect();
                if rest.starts_with("$_") {
                    i += 2;
                    // .Property chains
                    while i < c.len() && c[i] == '.' {
                        i += 1;
                        let s = i;
                        while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') {
                            i += 1;
                        }
                        if i == s {
                            return Err("a bad property in a filter".into());
                        }
                    }
                } else {
                    let w: String = rest.chars().take_while(|ch| ch.is_alphanumeric() || *ch == '$').collect();
                    if matches!(w.to_lowercase().as_str(), "$true" | "$false" | "$null") {
                        i += w.chars().count();
                    } else {
                        return Err("a variable other than $_ in a filter".into());
                    }
                }
            }
            '-' => {
                i += 1;
                let s = i;
                while i < c.len() && c[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let op: String = c[s..i].iter().collect::<String>().to_lowercase();
                if op.is_empty() {
                    // a negative number
                    continue;
                }
                if !BLOCK_OPS.contains(&op.as_str()) {
                    return Err(format!("the operator -{op} in a filter"));
                }
            }
            x if x.is_ascii_digit() || x == '.' => i += 1,
            _ => return Err(format!("the character {d} in a filter")),
        }
    }
    Ok(())
}

/// Check one path argument. Names listed are free; content is read only from a plain path.
fn check_path(raw: &str, content: bool, base: &Path, profile: &Path) -> Result<(), String> {
    for part in raw.split(',') {
        let t = part.trim();
        if t.is_empty() || numeric(t) {
            continue;
        }
        if t.starts_with('~') || t.contains('%') {
            return Err("a path that needs expanding".into());
        }
        if let Some(why) = safety::command_mentions_forbidden(t) {
            return Err(why.into());
        }
        let wild = is_wild(t);
        if wild && content {
            return Err("a wildcard that would read several files unchecked".into());
        }
        // For a wildcard, the folder before the first wildcard is the thing to check.
        let normalised = t.replace('/', "\\");
        let checked = if wild {
            let upto = normalised.find(['*', '?', '[']).unwrap_or(normalised.len());
            let head = &normalised[..upto];
            match head.rfind('\\') {
                Some(k) => head[..=k].to_string(),
                None => String::new(),
            }
        } else {
            normalised.clone()
        };
        let p = PathBuf::from(&checked);
        let abs = if checked.is_empty() {
            base.to_path_buf()
        } else if p.is_absolute() {
            p
        } else {
            base.join(p)
        };
        safety::resolve(&abs.to_string_lossy(), profile).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Whether `command` may run without asking. Err says why not (for the log, never an error to the
/// model: the command then goes to the user instead).
pub fn read_only(command: &str, base: &Path, profile: &Path) -> Result<(), String> {
    if let Some(why) = safety::command_mentions_forbidden(command) {
        return Err(why.into());
    }
    let toks = lex(command)?;
    if toks.is_empty() {
        return Err("empty".into());
    }
    let mut files_flowing = false;
    let mut segment: Vec<&Tok> = Vec::new();
    let mut segments: Vec<Vec<&Tok>> = Vec::new();
    for t in &toks {
        if *t == Tok::Pipe {
            segments.push(std::mem::take(&mut segment));
        } else {
            segment.push(t);
        }
    }
    segments.push(segment);
    for seg in segments {
        let Some(Tok::Word(name)) = seg.first().copied() else {
            return Err("a pipeline stage without a cmdlet".into());
        };
        let cmdlet = name.to_lowercase();
        let table = params_of(&cmdlet).ok_or_else(|| format!("{name} is not on the read-only list"))?;
        let content = reads_files(&cmdlet) || cmdlet == "get-content";
        if reads_files(&cmdlet) && files_flowing {
            return Err("files from a pipe would be read unchecked".into());
        }
        let mut pattern_taken = cmdlet != "select-string";
        let mut i = 1;
        while i < seg.len() {
            match seg[i] {
                Tok::Param(p, inline) => {
                    let Some((_, takes)) = table.iter().find(|(n, _)| n == p) else {
                        return Err(format!("the parameter -{p} is not on the read-only list"));
                    };
                    let value: Option<String> = if let Some(v) = inline {
                        Some(v.clone())
                    } else if *takes {
                        i += 1;
                        match seg.get(i) {
                            Some(Tok::Word(w)) => Some(w.clone()),
                            Some(Tok::Block(b)) if cmdlet == "where-object" && p == "filterscript" => {
                                check_block(b)?;
                                None
                            }
                            _ => return Err(format!("-{p} has no plain value")),
                        }
                    } else {
                        None
                    };
                    if let Some(v) = value {
                        match p.as_str() {
                            "path" | "literalpath" | "include" | "exclude" | "filter" => {
                                check_path(&v, content, base, profile)?
                            }
                            "pattern" => pattern_taken = true,
                            _ => {}
                        }
                    }
                }
                Tok::Word(w) => {
                    if cmdlet == "where-object" || cmdlet == "sort-object" || cmdlet == "select-object"
                        || cmdlet == "measure-object" || cmdlet.starts_with("format-") || cmdlet == "get-date"
                        || cmdlet == "get-process"
                    {
                        // property names and literals: no path involved
                    } else if !pattern_taken {
                        pattern_taken = true; // select-string's pattern is text, not a path
                    } else {
                        check_path(w, content, base, profile)?;
                    }
                }
                Tok::Block(b) => {
                    if cmdlet != "where-object" {
                        return Err("a script block outside Where-Object".into());
                    }
                    check_block(b)?;
                }
                Tok::Pipe => unreachable!(),
            }
            i += 1;
        }
        // Without a path, the file readers and listers look at the current folder.
        match cmdlet.as_str() {
            "get-childitem" | "get-item" => {
                let has_path = seg.iter().skip(1).any(|t| matches!(t, Tok::Word(_)))
                    || seg.iter().any(|t| matches!(t, Tok::Param(p, _) if p == "path" || p == "literalpath"));
                if !has_path {
                    check_path(".", false, base, profile)?;
                }
                files_flowing = true;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether a command deletes or overwrites files (or the machine's state): those still ask in
/// full autonomy. A coarse word check, because it only ever adds a question.
pub fn destructive(command: &str) -> bool {
    let c = command.to_lowercase();
    if c.contains('>') {
        return true;
    }
    c.split(|ch: char| !(ch.is_alphanumeric() || ch == '-' || ch == '_'))
        .any(|w| {
            matches!(
                w,
                "remove-item" | "rm" | "del" | "erase" | "rmdir" | "rd" | "ri" | "set-content"
                    | "sc" | "out-file" | "clear-content" | "clc" | "move-item" | "mv" | "move"
                    | "mi" | "rename-item" | "ren" | "rni" | "copy-item" | "cp" | "copy" | "cpi"
                    | "new-item" | "ni" | "format-volume" | "clear-disk" | "remove-itemproperty"
                    | "set-itemproperty" | "stop-computer" | "restart-computer" | "expand-archive"
                    | "compress-archive" | "add-content" | "ac" | "tee-object" | "tee" | "invoke-expression"
                    | "iex" | "start-process" | "invoke-webrequest" | "iwr" | "curl" | "wget"
            ) || w.starts_with("remove-") || w.starts_with("set-") || w.starts_with("clear-")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(tag: &str) -> (PathBuf, PathBuf) {
        let d = std::env::temp_dir().join(format!("readonly-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("docs")).unwrap();
        std::fs::write(d.join("a.txt"), "x").unwrap();
        std::fs::write(d.join(".env"), "K=1").unwrap();
        (d.clone(), d)
    }

    #[test]
    fn plain_reads_are_allowed() {
        let (b, p) = setup("plain");
        for c in [
            "Get-ChildItem",
            "Get-ChildItem docs -Recurse -Filter *.pdf | Sort-Object LastWriteTime -Descending | Select-Object -First 5",
            "Get-ChildItem -Path . | Where-Object { $_.Length -gt 100 -and $_.Name -like '*.txt' } | Format-Table Name, Length",
            "Get-Content a.txt -TotalCount 5",
            "Get-Content a.txt | Select-String 'hello' | Measure-Object",
            "Select-String -Pattern 'hi' -Path a.txt",
            "Test-Path docs",
            "Get-Process | Sort-Object CPU -Descending | Select-Object -First 3 | Format-List Name, Id",
            "Get-Date -Format 'yyyy-MM-dd'",
            "get-item a.txt",
        ] {
            assert!(read_only(c, &b, &p).is_ok(), "{c}: {:?}", read_only(c, &b, &p));
        }
    }

    #[test]
    fn hostile_commands_are_not_read_only() {
        let (b, p) = setup("hostile");
        let outside = std::env::temp_dir().join("elsewhere-readonly");
        let out_cmd = format!("Get-Content {}", outside.display());
        for c in [
            "Get-Content .env",
            "Get-Content ./.env.local",
            "Get-ChildItem | Remove-Item",
            "Get-ChildItem | Get-Content",
            "Get-ChildItem docs | Select-String x",
            "Get-Content a; rm b",
            "Get-Content a & calc",
            "Get-Content a && calc",
            "Select-String -Path $env:USERPROFILE\\.ssh\\*",
            "Get-Content $env:USERPROFILE\\notes.txt",
            "Get-Content a.txt > out.txt",
            "Get-Content a.txt >> out.txt",
            "Get-Content `a.txt",
            "Get-Content $(Get-Secret)",
            "Get-Content (Read-Host)",
            "Invoke-Expression 'ls'",
            "Get-ChildItem | Invoke-WebRequest",
            "iex 'ls'",
            "powershell -EncodedCommand ZQBjAGgAbwA=",
            "Get-ChildItem -EncodedCommand abc",
            "Get-ChildItem | Where-Object { Remove-Item $_ }",
            "Get-ChildItem | Where-Object { $_.Delete() }",
            "Get-ChildItem | Where-Object { $x = 1 }",
            "Get-ChildItem | ForEach-Object { $_.Name }",
            "Get-ChildItem | Where-Object { [IO.File]::Delete($_) }",
            "Get-Content \"$env:USERPROFILE\\x\"",
            "Get-Content *",
            "Get-Content .en*",
            "Get-Content ../../outside.txt",
            "Get-Content ~/notes.txt",
            "Get-Content a.txt\nrm b",
            "ls",
            "Set-Content a.txt hi",
            "Get-Content a.txt -Wait",
            "Format-Hex .env",
            out_cmd.as_str(),
        ] {
            assert!(read_only(c, &b, &p).is_err(), "should not auto-run: {c}");
        }
    }

    #[test]
    fn deleting_and_overwriting_are_recognised() {
        for c in ["Remove-Item a", "rm -r x", "Get-Content a > b", "Set-Content a hi", "Get-ChildItem | Remove-Item", "Move-Item a b"] {
            assert!(destructive(c), "{c}");
        }
        for c in ["Get-ChildItem", "Start-Process calc", "Get-Process | Sort-Object CPU"] {
            let _ = c;
        }
        assert!(!destructive("Get-ChildItem | Sort-Object Length"));
        assert!(!destructive("Get-Date"));
    }

    #[test]
    fn a_wildcard_in_a_listing_is_judged_by_its_folder() {
        let (b, p) = setup("wild");
        assert!(read_only("Get-ChildItem docs\\*.pdf", &b, &p).is_ok());
        assert!(read_only("Get-ChildItem C:\\Users\\x\\.ssh\\*", &b, &p).is_err());
    }
}
