//! Skill metadata for the system prompt.
//!
//! Discovers `SKILL.md` one level under `<agent_dir>/skills/<name>/` and
//! `<cwd>/.titi/skills/<name>/`. Only `name` and `description` are injected.
//! Bodies and scripts are not read into the prompt and are not executed.
//! A duplicate name keeps the project skill. The list is capped so a huge
//! directory cannot fill the prompt.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// How many skills a prompt will name. Past this, the rest are omitted.
pub const SKILL_LIST_CAP: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillMeta {
    name: String,
    description: String,
}

/// A short list, or `None` when nothing qualified.
pub fn render(cwd: Option<&Path>, agent_dir: Option<&Path>) -> Option<String> {
    let mut by_name: BTreeMap<String, SkillMeta> = BTreeMap::new();
    if let Some(agent_dir) = agent_dir {
        for skill in discover(&agent_dir.join("skills")) {
            by_name.insert(skill.name.clone(), skill);
        }
    }
    // Project skills replace an agent skill of the same name.
    if let Some(cwd) = cwd {
        for skill in discover(&cwd.join(".titi").join("skills")) {
            by_name.insert(skill.name.clone(), skill);
        }
    }
    if by_name.is_empty() {
        return None;
    }
    let mut out = String::from("# Skills\n");
    for skill in by_name.into_values().take(SKILL_LIST_CAP) {
        out.push_str("- ");
        out.push_str(&skill.name);
        out.push_str(": ");
        out.push_str(&skill.description);
        out.push('\n');
    }
    Some(out)
}

fn discover(root: &Path) -> Vec<SkillMeta> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(skill) = parse_skill(&path.join("SKILL.md")) {
            found.push(skill);
        }
    }
    found
}

fn parse_skill(path: &Path) -> Option<SkillMeta> {
    let text = fs::read_to_string(path).ok()?;
    let fields = frontmatter(&text)?;
    let name = fields.get("name").map(|value| unquote(value))?;
    let description = fields.get("description").map(|value| unquote(value))?;
    if name.is_empty() || description.is_empty() {
        return None;
    }
    Some(SkillMeta { name, description })
}

/// `name` and `description` from a leading `---` block. Other keys are ignored.
fn frontmatter(text: &str) -> Option<BTreeMap<String, String>> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---").or_else(|| rest.find("\r\n---"))?;
    let mut fields = BTreeMap::new();
    for line in rest[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.contains(' ') {
            continue;
        }
        fields.insert(key.to_string(), value.trim().to_string());
    }
    Some(fields)
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn two_skills_are_listed_and_a_blank_description_is_ignored() {
        let agent = tempfile::tempdir().unwrap();
        write(
            &agent.path().join("skills/review/SKILL.md"),
            "---\nname: review\ndescription: Check a diff\n---\nrun the secret script\n",
        );
        write(
            &agent.path().join("skills/blank/SKILL.md"),
            "---\nname: blank\n---\nshould not appear\n",
        );
        let project = tempfile::tempdir().unwrap();
        write(
            &project.path().join(".titi/skills/ship/SKILL.md"),
            "---\nname: ship\ndescription: Cut a release\n---\n#!/bin/sh\necho no\n",
        );

        let block = render(Some(project.path()), Some(agent.path())).unwrap();
        assert!(block.contains("- review: Check a diff"), "{block}");
        assert!(block.contains("- ship: Cut a release"), "{block}");
        assert!(!block.contains("blank"), "{block}");
        assert!(!block.contains("secret script"), "{block}");
        assert!(!block.contains("#!/bin/sh"), "{block}");
    }

    #[test]
    fn a_duplicate_name_keeps_the_project_skill() {
        let agent = tempfile::tempdir().unwrap();
        write(
            &agent.path().join("skills/review/SKILL.md"),
            "---\nname: review\ndescription: Agent copy\n---\n",
        );
        let project = tempfile::tempdir().unwrap();
        write(
            &project.path().join(".titi/skills/review/SKILL.md"),
            "---\nname: review\ndescription: Project copy\n---\n",
        );

        let block = render(Some(project.path()), Some(agent.path())).unwrap();
        assert!(block.contains("- review: Project copy"), "{block}");
        assert!(!block.contains("Agent copy"), "{block}");
    }

    #[test]
    fn a_missing_directory_contributes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(render(Some(dir.path()), None), None);
    }

    #[test]
    fn the_list_is_capped() {
        let project = tempfile::tempdir().unwrap();
        for index in 0..40 {
            write(
                &project
                    .path()
                    .join(format!(".titi/skills/skill-{index:02}/SKILL.md")),
                &format!("---\nname: skill-{index:02}\ndescription: n{index}\n---\n"),
            );
        }
        let block = render(Some(project.path()), None).unwrap();
        let lines = block.lines().filter(|line| line.starts_with("- ")).count();
        assert_eq!(lines, SKILL_LIST_CAP);
        assert!(block.contains("skill-00"), "{block}");
        assert!(!block.contains("skill-32"), "{block}");
    }
}
