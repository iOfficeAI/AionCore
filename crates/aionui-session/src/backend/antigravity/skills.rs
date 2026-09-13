//! Slash commands derived from this session's agy skills.
//!
//! agy exposes no way to LIST its commands: `agy commands` / `agy skills` drop
//! into the TUI, and headless mode has no equivalent. But `-p "/<skill-name>"`
//! does invoke a skill (verified), so the command list is read off the skill
//! files rather than asked for.
//!
//! The files are the session's RESOLVED skill directories. This used to scan
//! `{workspace}/.agents/skills`, which AionUi no longer creates. The resolved
//! dirs are a strictly better source anyway: they are exactly this
//! conversation's enabled skills, so a stale residue left in the workspace by
//! an older build can no longer show up in the picker.

use std::path::Path;

use crate::backend::SkillDirSpec;
use crate::capability::SlashCommandInfo;

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

/// Read `{skill_dir}/SKILL.md` for each of the session's skills.
///
/// Never fails: an unreadable or malformed skill is skipped, because one bad
/// file must not cost the user their whole command list.
pub(crate) fn skill_commands_from_dirs(skills: &[SkillDirSpec]) -> Vec<SlashCommandInfo> {
    let mut out: Vec<SlashCommandInfo> = skills
        .iter()
        .filter_map(|skill| std::fs::read_to_string(Path::new(&skill.path).join("SKILL.md")).ok())
        .filter_map(|md| parse_frontmatter(&md))
        .collect();
    // A stable list keeps the picker from reshuffling between sessions.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a skill as a standalone SOURCE directory (what the resolver hands
    /// us), not under a workspace `.agents/skills` tree.
    fn write_skill(root: &Path, name: &str, body: &str) -> SkillDirSpec {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
        SkillDirSpec {
            name: name.to_owned(),
            path: dir.to_string_lossy().into_owned(),
        }
    }

    #[test]
    fn reads_name_and_description_from_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill = write_skill(
            dir.path(),
            "aionui-probe",
            "---\nname: aionui-probe\ndescription: Probe skill.\n---\n\n# body\n",
        );

        let cmds = skill_commands_from_dirs(&[skill]);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "aionui-probe");
        assert_eq!(cmds[0].description.as_deref(), Some("Probe skill."));
    }

    /// The command list must come from the session's resolved skills, NOT from
    /// the workspace: AionUi no longer writes there, and a residue left by an
    /// older build must not reappear in the picker.
    #[test]
    fn a_stale_workspace_residue_is_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let cron = write_skill(dir.path(), "cron", "---\nname: cron\ndescription: Schedule.\n---\n");

        let workspace = dir.path().join("workspace");
        let residue = workspace.join(".agents").join("skills").join("residue");
        std::fs::create_dir_all(&residue).unwrap();
        std::fs::write(residue.join("SKILL.md"), "---\nname: residue\n---\n").unwrap();

        let cmds = skill_commands_from_dirs(&[cron]);
        assert_eq!(cmds.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["cron"]);
    }

    #[test]
    fn supports_the_folded_description_agys_own_skills_use() {
        // agy's builtin skills (e.g. agy-customizations) write `description: >-`
        // followed by indented lines.
        let dir = tempfile::tempdir().unwrap();
        let skill = write_skill(
            dir.path(),
            "folded",
            "---\nname: folded\ndescription: >-\n  First part\n  second part.\n---\n\nbody\n",
        );

        let cmds = skill_commands_from_dirs(&[skill]);
        assert_eq!(cmds[0].description.as_deref(), Some("First part second part."));
    }

    #[test]
    fn a_malformed_or_missing_skill_is_skipped_not_fatal() {
        // One broken file must not cost the user the whole command list.
        let dir = tempfile::tempdir().unwrap();
        let broken = write_skill(dir.path(), "broken", "no frontmatter here");
        let good = write_skill(dir.path(), "good", "---\nname: good\n---\n");
        let missing = SkillDirSpec {
            name: "ghost".to_owned(),
            path: dir.path().join("ghost").to_string_lossy().into_owned(),
        };

        let cmds = skill_commands_from_dirs(&[broken, good, missing]);
        assert_eq!(cmds.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["good"]);
    }

    #[test]
    fn results_are_sorted_so_the_picker_does_not_reshuffle() {
        let dir = tempfile::tempdir().unwrap();
        let zeta = write_skill(dir.path(), "zeta", "---\nname: zeta\n---\n");
        let alpha = write_skill(dir.path(), "alpha", "---\nname: alpha\n---\n");

        let cmds = skill_commands_from_dirs(&[zeta, alpha]);
        assert_eq!(
            cmds.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
    }

    #[test]
    fn a_session_without_skills_yields_nothing() {
        assert!(skill_commands_from_dirs(&[]).is_empty());
    }
}
