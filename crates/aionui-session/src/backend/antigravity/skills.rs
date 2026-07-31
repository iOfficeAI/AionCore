//! Slash commands derived from the workspace's agy skills.
//!
//! agy exposes no way to LIST its commands: `agy commands` / `agy skills` drop
//! into the TUI, and headless mode has no equivalent. But `-p "/<skill-name>"`
//! does invoke a skill (verified), and AionUi provisions those skills itself
//! under `.agents/skills/`. So the command list is read off the skill files
//! rather than asked for.

use std::path::Path;

use crate::capability::SlashCommandInfo;

/// Where agy looks for workspace skills, and where AionUi provisions them.
const SKILLS_DIR: &str = ".agents/skills";

/// Pull `name` / `description` out of a `SKILL.md` YAML frontmatter block.
///
/// Deliberately minimal: only the two scalar keys we need, so a skill file
/// using richer YAML elsewhere cannot break discovery. `description` supports
/// the folded (`>-`) form agy's own builtin skills use.
fn parse_frontmatter(md: &str) -> Option<SlashCommandInfo> {
    let rest = md.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let block = &rest[..end];

    let mut name = None;
    let mut description: Option<String> = None;
    let mut folding_description = false;

    for line in block.lines() {
        if folding_description {
            // Folded scalars continue while the line is indented.
            if line.starts_with(' ') || line.starts_with('\t') {
                let piece = line.trim();
                match description.as_mut() {
                    Some(d) if !piece.is_empty() => {
                        d.push(' ');
                        d.push_str(piece);
                    }
                    Some(_) => {}
                    None => description = Some(piece.to_owned()),
                }
                continue;
            }
            folding_description = false;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "name" => name = Some(value.trim_matches(['"', '\'']).to_owned()),
            "description" => {
                if value == ">-" || value == ">" || value == "|" {
                    folding_description = true;
                    description = None;
                } else {
                    description = Some(value.trim_matches(['"', '\'']).to_owned());
                }
            }
            _ => {}
        }
    }

    let name = name.filter(|n| !n.is_empty())?;
    Some(SlashCommandInfo {
        name,
        description: description.filter(|d| !d.is_empty()),
    })
}

/// Scan `<workspace>/.agents/skills/*/SKILL.md` for invokable skills.
///
/// Never fails: an unreadable or malformed skill is skipped, because a bad
/// file must not cost the user their whole command list.
pub(crate) fn scan_skill_commands(workspace: &Path) -> Vec<SlashCommandInfo> {
    let Ok(entries) = std::fs::read_dir(workspace.join(SKILLS_DIR)) else {
        return Vec::new();
    };
    let mut out: Vec<SlashCommandInfo> = entries
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("SKILL.md")).ok())
        .filter_map(|md| parse_frontmatter(&md))
        .collect();
    // Directory order is filesystem-dependent; a stable list keeps the picker
    // from reshuffling between sessions.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, body: &str) {
        let d = root.join(SKILLS_DIR).join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn reads_name_and_description_from_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "aionui-probe",
            "---\nname: aionui-probe\ndescription: Probe skill.\n---\n\n# body\n",
        );

        let cmds = scan_skill_commands(dir.path());
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "aionui-probe");
        assert_eq!(cmds[0].description.as_deref(), Some("Probe skill."));
    }

    #[test]
    fn supports_the_folded_description_agys_own_skills_use() {
        // agy's builtin skills (e.g. agy-customizations) write `description: >-`
        // followed by indented lines.
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "folded",
            "---\nname: folded\ndescription: >-\n  First part\n  second part.\n---\n\nbody\n",
        );

        let cmds = scan_skill_commands(dir.path());
        assert_eq!(cmds[0].description.as_deref(), Some("First part second part."));
    }

    #[test]
    fn a_malformed_skill_is_skipped_not_fatal() {
        // One broken file must not cost the user the whole command list.
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "broken", "no frontmatter here");
        write_skill(dir.path(), "good", "---\nname: good\n---\n");

        let cmds = scan_skill_commands(dir.path());
        assert_eq!(cmds.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["good"]);
    }

    #[test]
    fn results_are_sorted_so_the_picker_does_not_reshuffle() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "zeta", "---\nname: zeta\n---\n");
        write_skill(dir.path(), "alpha", "---\nname: alpha\n---\n");

        let cmds = scan_skill_commands(dir.path());
        assert_eq!(
            cmds.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
    }

    #[test]
    fn a_workspace_without_skills_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_skill_commands(dir.path()).is_empty());
    }
}
