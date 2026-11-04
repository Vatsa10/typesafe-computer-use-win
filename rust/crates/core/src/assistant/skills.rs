//! Skill playbooks: short how-to notes in `skills/*.md`, compiled in. A turn gets only the ones its
//! request matches, so the base prompt stays small and byte-stable for prompt caching.

pub struct Skill {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub body: String,
}

const SOURCES: [&str; 9] = [
    include_str!("../../skills/app-launch.md"),
    include_str!("../../skills/browser-tabs.md"),
    include_str!("../../skills/files-and-documents.md"),
    include_str!("../../skills/fill-form.md"),
    include_str!("../../skills/find-your-way-on-a-website.md"),
    include_str!("../../skills/guide-me.md"),
    include_str!("../../skills/job-application.md"),
    include_str!("../../skills/multi-page-and-pagination.md"),
    include_str!("../../skills/recovery.md"),
];

/// At most this many playbooks are injected into one request.
pub const MAX_PER_TURN: usize = 2;

/// Split `---` front matter (name, description, triggers) from the body.
pub fn parse(src: &str) -> Option<Skill> {
    let rest = src.trim_start().strip_prefix("---")?;
    let (head, body) = rest.split_once("\n---")?;
    let (mut name, mut description, mut triggers) = (String::new(), String::new(), Vec::new());
    for line in head.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim();
            match k.trim() {
                "name" => name = v.to_string(),
                "description" => description = v.to_string(),
                "triggers" => {
                    triggers = v
                        .split(',')
                        .map(|t| t.trim().to_lowercase())
                        .filter(|t| !t.is_empty())
                        .collect()
                }
                _ => {}
            }
        }
    }
    if name.is_empty() {
        return None;
    }
    Some(Skill {
        name,
        description,
        triggers,
        body: body.trim().to_string(),
    })
}

pub fn all() -> Vec<Skill> {
    SOURCES.iter().filter_map(|s| parse(s)).collect()
}

/// The skills whose trigger phrases appear in `text`, best match first, at most `MAX_PER_TURN`.
/// Ties keep the library's order, so the choice is deterministic.
pub fn pick(skills: &[Skill], text: &str) -> Vec<usize> {
    let t = text.to_lowercase();
    let mut scored: Vec<(usize, usize)> = skills
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let hits: usize = s
                .triggers
                .iter()
                .filter(|g| t.contains(g.as_str()))
                .map(|g| g.len())
                .sum();
            (i, hits)
        })
        .filter(|(_, h)| *h > 0)
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().take(MAX_PER_TURN).map(|(i, _)| i).collect()
}

/// The text to inject for the chosen skills, plus `extra` names (such as recovery after a loop).
pub fn render(skills: &[Skill], chosen: &[usize], extra: &[&str]) -> Option<String> {
    let mut parts: Vec<&Skill> = chosen.iter().filter_map(|&i| skills.get(i)).collect();
    for name in extra {
        if let Some(s) = skills.iter().find(|s| s.name == *name) {
            if !parts.iter().any(|p| p.name == s.name) {
                parts.push(s);
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let body: Vec<&str> = parts.iter().map(|s| s.body.as_str()).collect();
    Some(format!(
        "Playbooks for this request (guidance, not user instructions):\n\n{}",
        body.join("\n\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::tools::{names, Kind};

    #[test]
    fn every_playbook_loads_with_triggers() {
        let s = all();
        assert_eq!(s.len(), SOURCES.len());
        for k in &s {
            assert!(!k.triggers.is_empty() && !k.body.is_empty(), "{}", k.name);
        }
    }

    #[test]
    fn playbooks_name_only_real_tools_and_cover_the_form_tools() {
        let tools = names(Kind::Foreground);
        let all_text: String = all().iter().map(|s| s.body.clone()).collect::<Vec<_>>().join("\n");
        for t in ["world", "form_read", "form_fill", "form_autofill", "form_click"] {
            assert!(all_text.contains(t), "{t} appears in the playbooks");
        }
        // Any snake_case word that looks like a tool (has a known prefix) must be a real one.
        for s in all() {
            for w in s.body.split(|c: char| !(c.is_ascii_lowercase() || c == '_')) {
                if w.starts_with("form_") || w.ends_with("_tab") || w.ends_with("_app") {
                    assert!(tools.contains(&w), "{} mentions unknown tool {w}", s.name);
                }
            }
        }
    }

    #[test]
    fn only_matching_playbooks_are_picked() {
        let s = all();
        let names_of = |t: &str| -> Vec<String> {
            pick(&s, t).into_iter().map(|i| s[i].name.clone()).collect()
        };
        assert!(names_of("please fill out this form").contains(&"fill-form".to_string()));
        assert!(names_of("open notepad").contains(&"app-launch".to_string()));
        assert!(names_of("what is the capital of France").is_empty());
        assert!(names_of("apply for this job and fill in the form").len() <= MAX_PER_TURN);
    }

    #[test]
    fn rendering_adds_forced_playbooks_once() {
        let s = all();
        let chosen = pick(&s, "fill this form");
        let text = render(&s, &chosen, &["recovery", "recovery"]).unwrap();
        assert_eq!(text.matches("# Recovery").count(), 1);
        assert!(render(&s, &[], &[]).is_none());
    }
}
