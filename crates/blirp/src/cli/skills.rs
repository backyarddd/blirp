//! `blirp skills install|uninstall|list`: blirp's Agent Skills (`skills/` in
//! the repository, embedded into the binary) in the folders agents load
//! `SKILL.md` skills from.
//!
//! Two folders cover every agent that loads skills: `~/.claude/skills`
//! (Claude Code; Amp, opencode and Cursor read it too) and `~/.agents/skills`
//! (Codex, Gemini CLI, opencode, Cursor, pi). `--project <dir>` uses the same two under that
//! folder. Every skill folder blirp writes holds `.blirp-skill`, the SHA-256
//! of the `SKILL.md` blirp wrote: it marks the folder as blirp's and tells an
//! edited file apart. Edited or foreign skills are never overwritten without
//! `--force` (which backs the file up once) and never removed.

use crate::hooks::install::Homes;
use anyhow::{Context as _, bail};
use clap::Subcommand;
use sha2::{Digest as _, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub struct Skill {
    pub name: &'static str,
    pub body: &'static str,
}

macro_rules! skill {
    ($name:literal) => {
        Skill {
            name: $name,
            body: include_str!(concat!("../../../../skills/", $name, "/SKILL.md")),
        }
    };
}

pub const SKILLS: &[Skill] = &[
    skill!("blirp-status"),
    skill!("blirp-link-machines"),
    skill!("blirp-update"),
    skill!("blirp-memory"),
    skill!("blirp-sessions"),
];

const SKILL_FILE: &str = "SKILL.md";
const MARKER: &str = ".blirp-skill";

#[derive(Subcommand)]
pub enum SkillsCommand {
    /// Install blirp's skills for agents (default: every supported agent
    /// found on PATH). Skills you edited are kept unless --force.
    Install {
        /// Only for this agent: claude, amp, codex, gemini, opencode, cursor or pi.
        #[arg(long)]
        agent: Option<String>,
        /// Install into this project folder instead of your home directory.
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
        /// Also replace skills that were edited or not written by blirp
        /// (the old SKILL.md is backed up once to SKILL.md.blirp-backup).
        #[arg(long)]
        force: bool,
    },
    /// Remove the skills `install` wrote; edited ones stay.
    Uninstall {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
    },
    /// Refresh installed, unedited blirp skills in your home folders to
    /// this version's text (run by `blirp update` with the new binary).
    #[command(hide = true)]
    Refresh,
    /// Show which blirp skills are installed where.
    List {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
    },
}

/// The two skill folders agents read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Loc {
    /// `.claude/skills`
    Claude,
    /// `.agents/skills`
    Agents,
}

/// Agents that load `SKILL.md` skills, and the folder blirp installs them in.
/// Amp's own user folder is `~/.config/agents/skills`, but it also reads
/// `~/.claude/skills`, so the two folders cover it.
const AGENTS: &[(&str, Loc)] = &[
    ("claude", Loc::Claude),
    ("amp", Loc::Claude),
    ("codex", Loc::Agents),
    ("gemini", Loc::Agents),
    ("opencode", Loc::Agents),
    ("cursor", Loc::Agents),
    ("pi", Loc::Agents),
];

#[derive(Debug)]
pub struct Target {
    pub label: &'static str,
    pub dir: PathBuf,
}

fn target(loc: Loc, project: Option<&Path>) -> anyhow::Result<Target> {
    let home_err = "cannot determine the home directory";
    Ok(match loc {
        Loc::Claude => Target {
            label: "claude",
            dir: match project {
                Some(p) => p.join(".claude"),
                None => Homes::from_env().context(home_err)?.claude_dir,
            }
            .join("skills"),
        },
        Loc::Agents => Target {
            label: "agents",
            dir: match project {
                Some(p) => p.to_path_buf(),
                None => blirp_core::paths::user_home().context(home_err)?,
            }
            .join(".agents")
            .join("skills"),
        },
    })
}

/// Both skill folders in the home directory (`blirp doctor`, `blirp uninstall`).
pub fn home_targets() -> anyhow::Result<Vec<Target>> {
    [Loc::Claude, Loc::Agents]
        .into_iter()
        .map(|l| target(l, None))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Written by blirp, same as this version's.
    Installed,
    /// Written by blirp, unedited, from another blirp version.
    Outdated,
    /// Written by blirp, then edited.
    Modified,
    /// A skill of that name blirp did not write.
    NotOurs,
    NotInstalled,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            State::Installed => "installed",
            State::Outdated => "outdated",
            State::Modified => "modified",
            State::NotOurs => "not_ours",
            State::NotInstalled => "not_installed",
        })
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn read_opt(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Is the `SKILL.md` in `folder` the one blirp wrote there (unedited)?
/// `None` when the folder has no blirp marker.
fn ours_unedited(folder: &Path) -> anyhow::Result<Option<bool>> {
    let Some(marker) = read_opt(&folder.join(MARKER))? else {
        return Ok(None);
    };
    let file = read_opt(&folder.join(SKILL_FILE))?;
    let marker = String::from_utf8_lossy(&marker);
    Ok(Some(file.is_some_and(|f| digest(&f) == marker.trim())))
}

pub fn state(dir: &Path, skill: &Skill) -> anyhow::Result<State> {
    let folder = dir.join(skill.name);
    let Some(file) = read_opt(&folder.join(SKILL_FILE))? else {
        return Ok(State::NotInstalled);
    };
    Ok(match ours_unedited(&folder)? {
        None => State::NotOurs,
        Some(false) => State::Modified,
        Some(true) if file == skill.body.as_bytes() => State::Installed,
        Some(true) => State::Outdated,
    })
}

/// Temp file + rename, so an agent never reads a half-written skill.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = PathBuf::from(format!("{}.blirp-tmp", path.display()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// Install `skill` into `dir`; returns what happened, one word.
pub fn install_one(dir: &Path, skill: &Skill, force: bool) -> anyhow::Result<&'static str> {
    let before = state(dir, skill)?;
    let folder = dir.join(skill.name);
    let file = folder.join(SKILL_FILE);
    match before {
        State::Installed => return Ok("unchanged"),
        State::Modified | State::NotOurs if !force => return Ok("skipped"),
        State::Modified | State::NotOurs => {
            let backup = folder.join(format!("{SKILL_FILE}.blirp-backup"));
            if !backup.exists() {
                std::fs::copy(&file, &backup).with_context(|| {
                    format!("backing up {} to {}", file.display(), backup.display())
                })?;
            }
        }
        State::Outdated | State::NotInstalled => {}
    }
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    write_atomic(&file, skill.body.as_bytes())?;
    write_atomic(
        &folder.join(MARKER),
        format!("{}\n", digest(skill.body.as_bytes())).as_bytes(),
    )?;
    Ok(match before {
        State::NotInstalled => "installed",
        State::Outdated => "updated",
        _ => "replaced",
    })
}

/// Remove every unedited skill blirp wrote into `dir`, including skills
/// an older version installed. Returns (skill folder name, what happened).
pub fn uninstall_dir(dir: &Path) -> anyhow::Result<Vec<(String, &'static str)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let folder = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        if !folder.is_dir() {
            continue;
        }
        let name = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match ours_unedited(&folder)? {
            None => {}
            Some(false) if folder.join(SKILL_FILE).exists() => out.push((name, "kept_modified")),
            Some(_) => {
                for f in [SKILL_FILE, MARKER] {
                    let p = folder.join(f);
                    match std::fs::remove_file(&p) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(e).with_context(|| format!("removing {}", p.display()));
                        }
                    }
                }
                // Only when empty: a backup or the user's own files stay.
                let _ = std::fs::remove_dir(&folder);
                out.push((name, "removed"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Rewrite the `outdated` skills in `dir` (blirp's, unedited, another
/// version's text) with this version's; nothing else is touched.
pub fn refresh_dir(dir: &Path) -> Vec<(&'static str, anyhow::Result<&'static str>)> {
    SKILLS
        .iter()
        .filter_map(|s| match state(dir, s) {
            Ok(State::Outdated) => Some((s.name, install_one(dir, s, false))),
            Ok(_) => None,
            Err(e) => Some((s.name, Err(e))),
        })
        .collect()
}

/// After `blirp update` replaced the binary: let the new `exe` refresh the
/// skills to its own texts. Never fails the update; problems are printed.
pub fn refresh_after_update(exe: &Path) {
    match blirp_core::process::command(exe)
        .args(["skills", "refresh"])
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(out) => {
            print!("{}", String::from_utf8_lossy(&out.stdout));
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
            if !out.status.success() {
                eprintln!(
                    "blirp: some skills were not refreshed; run `blirp skills install` to retry"
                );
            }
        }
        Err(e) => {
            eprintln!("blirp: skills were not refreshed ({e}); run `blirp skills install` to retry")
        }
    }
}

/// One line for `blirp doctor`: the state counts of `t`.
pub fn summary(t: &Target) -> String {
    let mut counts: Vec<(State, usize)> = Vec::new();
    for s in SKILLS {
        let st = match state(&t.dir, s) {
            Ok(st) => st,
            Err(e) => return format!("{}: {e:#}", t.dir.display()),
        };
        match counts.iter_mut().find(|(k, _)| *k == st) {
            Some((_, n)) => *n += 1,
            None => counts.push((st, 1)),
        }
    }
    if counts == [(State::NotInstalled, SKILLS.len())] {
        return format!(
            "{} none installed (optional: `blirp skills install`)",
            t.dir.display()
        );
    }
    let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
    let hint = if counts.iter().any(|(k, _)| *k == State::Outdated) {
        "; `blirp skills install` refreshes them"
    } else {
        ""
    };
    format!("{} {}{hint}", t.dir.display(), parts.join(", "))
}

fn locs(agent: Option<&str>, detect: bool) -> anyhow::Result<Vec<Loc>> {
    let mut out: Vec<Loc> = match agent {
        Some(a) => match AGENTS.iter().find(|(id, _)| *id == a) {
            Some((_, l)) => vec![*l],
            None => bail!(
                "{a} does not load skills (supported: {})",
                AGENTS
                    .iter()
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        None if detect => {
            let config = blirp_core::config::Config::default();
            AGENTS
                .iter()
                .filter(|(id, _)| {
                    crate::agents::Agent::resolve(id, &config).is_ok_and(|x| x.path.is_some())
                })
                .map(|(_, l)| *l)
                .collect()
        }
        None => vec![Loc::Claude, Loc::Agents],
    };
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn run(cmd: SkillsCommand) -> anyhow::Result<ExitCode> {
    let mut failed = false;
    match cmd {
        SkillsCommand::Install {
            agent,
            project,
            force,
        } => {
            let locs = locs(agent.as_deref(), true)?;
            if locs.is_empty() {
                bail!(
                    "no agent that loads skills was found on PATH; pass --agent <{}>",
                    AGENTS
                        .iter()
                        .map(|(id, _)| *id)
                        .collect::<Vec<_>>()
                        .join("|")
                );
            }
            let mut skipped = false;
            for loc in locs {
                let t = target(loc, project.as_deref())?;
                for s in SKILLS {
                    match install_one(&t.dir, s, force) {
                        Ok(what) => {
                            skipped |= what == "skipped";
                            println!(
                                "{:<7} {:<20} {what:<9} {}",
                                t.label,
                                s.name,
                                t.dir.join(s.name).display()
                            );
                        }
                        Err(e) => {
                            failed = true;
                            eprintln!("{:<7} {:<20} error: {e:#}", t.label, s.name);
                        }
                    }
                }
            }
            if skipped {
                failed = true;
                eprintln!(
                    "Skipped skills were edited or not written by blirp; `blirp skills install --force` \
                     replaces them (backing the old SKILL.md up once)."
                );
            }
        }
        SkillsCommand::Uninstall { agent, project } => {
            for loc in locs(agent.as_deref(), false)? {
                let t = target(loc, project.as_deref())?;
                match uninstall_dir(&t.dir) {
                    Ok(list) => {
                        for (name, what) in list {
                            println!(
                                "{:<7} {name:<20} {what:<13} {}",
                                t.label,
                                t.dir.join(&name).display()
                            );
                        }
                    }
                    Err(e) => {
                        failed = true;
                        eprintln!("{:<7} error: {e:#}", t.label);
                    }
                }
            }
        }
        SkillsCommand::Refresh => {
            for t in home_targets()? {
                for (name, r) in refresh_dir(&t.dir) {
                    match r {
                        Ok(_) => println!("Refreshed skill {name} in {}", t.dir.display()),
                        Err(e) => {
                            failed = true;
                            eprintln!("skill {name} in {}: {e:#}", t.dir.display());
                        }
                    }
                }
            }
        }
        SkillsCommand::List { agent, project } => {
            for loc in locs(agent.as_deref(), false)? {
                let t = target(loc, project.as_deref())?;
                for s in SKILLS {
                    match state(&t.dir, s) {
                        Ok(st) => println!(
                            "{:<7} {:<20} {st:<13} {}",
                            t.label,
                            s.name,
                            t.dir.join(s.name).display()
                        ),
                        Err(e) => {
                            failed = true;
                            eprintln!("{:<7} {:<20} error: {e:#}", t.label, s.name);
                        }
                    }
                }
            }
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use clap::Parser as _;

    fn all(dir: &Path, force: bool) -> Vec<&'static str> {
        SKILLS
            .iter()
            .map(|s| install_one(dir, s, force).unwrap())
            .collect()
    }

    #[test]
    fn install_is_idempotent_and_uninstall_is_exact() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("skills");
        // A skill of the user's own next to ours.
        std::fs::create_dir_all(dir.join("mine")).unwrap();
        std::fs::write(dir.join("mine").join(SKILL_FILE), "mine").unwrap();

        assert!(all(&dir, false).iter().all(|w| *w == "installed"));
        for s in SKILLS {
            assert_eq!(state(&dir, s).unwrap(), State::Installed);
            assert_eq!(
                std::fs::read_to_string(dir.join(s.name).join(SKILL_FILE)).unwrap(),
                s.body
            );
        }
        assert!(all(&dir, false).iter().all(|w| *w == "unchanged"));

        let removed = uninstall_dir(&dir).unwrap();
        assert_eq!(removed.len(), SKILLS.len());
        assert!(removed.iter().all(|(_, w)| *w == "removed"));
        for s in SKILLS {
            assert!(!dir.join(s.name).exists());
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("mine").join(SKILL_FILE)).unwrap(),
            "mine"
        );
        assert!(uninstall_dir(&dir).unwrap().is_empty());
        assert!(uninstall_dir(&d.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn edited_and_foreign_skills_are_kept_unless_forced() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        let s = &SKILLS[0];
        let file = dir.join(s.name).join(SKILL_FILE);
        install_one(dir, s, false).unwrap();
        std::fs::write(&file, "edited").unwrap();
        assert_eq!(state(dir, s).unwrap(), State::Modified);
        assert_eq!(install_one(dir, s, false).unwrap(), "skipped");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited");
        assert_eq!(
            uninstall_dir(dir).unwrap(),
            vec![(s.name.to_string(), "kept_modified")]
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited");

        assert_eq!(install_one(dir, s, true).unwrap(), "replaced");
        assert_eq!(state(dir, s).unwrap(), State::Installed);
        let backup = dir.join(s.name).join("SKILL.md.blirp-backup");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "edited");
        // Uninstall removes our files; the backup keeps the folder.
        uninstall_dir(dir).unwrap();
        assert!(!file.exists());
        assert!(backup.exists());

        // Same name, not written by blirp.
        let other = &SKILLS[1];
        std::fs::create_dir_all(dir.join(other.name)).unwrap();
        std::fs::write(dir.join(other.name).join(SKILL_FILE), "theirs").unwrap();
        assert_eq!(state(dir, other).unwrap(), State::NotOurs);
        assert_eq!(install_one(dir, other, false).unwrap(), "skipped");
        assert!(uninstall_dir(dir).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.join(other.name).join(SKILL_FILE)).unwrap(),
            "theirs"
        );
    }

    #[test]
    fn outdated_skills_are_refreshed_and_old_ones_removed() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        // What an older blirp would have written, including a retired skill.
        for name in [SKILLS[0].name, "blirp-retired"] {
            let old = format!("---\nname: {name}\ndescription: old\n---\n");
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join(SKILL_FILE), &old).unwrap();
            std::fs::write(dir.join(name).join(MARKER), digest(old.as_bytes()) + "\n").unwrap();
        }
        assert_eq!(state(dir, &SKILLS[0]).unwrap(), State::Outdated);
        assert!(
            summary(&Target {
                label: "t",
                dir: dir.to_path_buf()
            })
            .contains("1 outdated")
        );
        assert_eq!(install_one(dir, &SKILLS[0], false).unwrap(), "updated");
        assert_eq!(state(dir, &SKILLS[0]).unwrap(), State::Installed);
        let removed = uninstall_dir(dir).unwrap();
        assert_eq!(
            removed,
            vec![
                ("blirp-retired".to_string(), "removed"),
                (SKILLS[0].name.to_string(), "removed")
            ]
        );
        assert!(!dir.join("blirp-retired").exists());
    }

    #[test]
    fn refresh_touches_only_outdated_skills() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        let write = |name: &str, text: &str, marker: Option<&str>| {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join(SKILL_FILE), text).unwrap();
            if let Some(m) = marker {
                std::fs::write(
                    dir.join(name).join(MARKER),
                    digest(m.as_bytes())
                        + "
",
                )
                .unwrap();
            }
        };
        write(SKILLS[0].name, "old", Some("old")); // outdated
        write(SKILLS[1].name, "edited", Some("old")); // modified
        write(SKILLS[2].name, "theirs", None); // not ours
        // SKILLS[3] not installed; SKILLS[4] current.
        install_one(dir, &SKILLS[4], false).unwrap();

        let done = refresh_dir(dir);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, SKILLS[0].name);
        assert_eq!(*done[0].1.as_ref().unwrap(), "updated");
        assert_eq!(state(dir, &SKILLS[0]).unwrap(), State::Installed);
        assert_eq!(state(dir, &SKILLS[1]).unwrap(), State::Modified);
        assert_eq!(state(dir, &SKILLS[2]).unwrap(), State::NotOurs);
        assert_eq!(state(dir, &SKILLS[3]).unwrap(), State::NotInstalled);
        assert!(refresh_dir(dir).is_empty());
        assert!(refresh_dir(&dir.join("missing")).is_empty());
        // A binary that cannot run must not panic or fail the update.
        refresh_after_update(&dir.join("no-such-blirp"));
        assert!(super::super::Cli::try_parse_from(["blirp", "skills", "refresh"]).is_ok());
    }

    #[test]
    fn project_targets_and_agent_mapping() {
        let p = Path::new("proj");
        assert_eq!(
            target(Loc::Claude, Some(p)).unwrap().dir,
            p.join(".claude").join("skills")
        );
        assert_eq!(
            target(Loc::Agents, Some(p)).unwrap().dir,
            p.join(".agents").join("skills")
        );
        assert_eq!(locs(Some("codex"), true).unwrap(), vec![Loc::Agents]);
        assert_eq!(locs(Some("claude"), true).unwrap(), vec![Loc::Claude]);
        assert_eq!(locs(None, false).unwrap(), vec![Loc::Claude, Loc::Agents]);
        assert!(locs(Some("aider"), false).is_err());
    }

    /// Frontmatter per the Agent Skills specification (agentskills.io):
    /// `name` 1-64 of `a-z0-9-`, no leading, trailing or double hyphen, equal
    /// to the folder name; `description` 1-1024 characters. Values must be
    /// plain YAML scalars on one line, so every YAML parser reads them the same.
    #[test]
    fn frontmatter_follows_the_agent_skills_spec() {
        for s in SKILLS {
            let rest = s.body.strip_prefix("---\n").unwrap();
            let end = rest.find("\n---\n").unwrap();
            let mut name = None;
            let mut description = None;
            for line in rest[..end].lines() {
                let (k, v) = line.split_once(": ").unwrap();
                assert_eq!(v, v.trim(), "{}: {k}", s.name);
                assert!(!v.contains(": ") && !v.contains(" #"), "{}: {k}", s.name);
                assert!(
                    !v.starts_with(|c: char| "-?:,[]{}#&*!|>'\"%@`".contains(c)),
                    "{}: {k}",
                    s.name
                );
                match k {
                    "name" => name = Some(v),
                    "description" => description = Some(v),
                    _ => panic!("{}: unexpected key {k}", s.name),
                }
            }
            let name = name.unwrap();
            assert_eq!(name, s.name);
            assert!((1..=64).contains(&name.len()));
            assert!(
                name.split('-').all(|p| !p.is_empty()
                    && p.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())),
                "{name}"
            );
            let description = description.unwrap();
            assert!((1..=1024).contains(&description.chars().count()), "{name}");
            assert!(!s.body.contains('\r'), "{name}: CRLF");
        }
    }

    /// Every `blirp ...` command a skill tells an agent to run must parse
    /// with this binary's CLI, so the skills cannot drift from it. Fenced
    /// `sh` lines must be complete commands; inline code in prose may name a
    /// command without its required arguments (`blirp mem show`), but every
    /// subcommand and flag in it must exist.
    #[test]
    fn every_command_in_the_skills_parses() {
        use clap::error::ErrorKind;
        let mut checked = 0;
        for s in SKILLS {
            let mut commands: Vec<(&str, bool)> = Vec::new();
            let mut in_block = false;
            for line in s.body.lines() {
                let t = line.trim();
                if t.starts_with("```") {
                    in_block = !in_block;
                    continue;
                }
                if in_block {
                    if !t.is_empty() {
                        assert!(t.starts_with("blirp "), "{}: {t}", s.name);
                        commands.push((t, true));
                    }
                    continue;
                }
                for (i, span) in line.split('`').enumerate() {
                    if i % 2 == 1 && span.starts_with("blirp ") {
                        commands.push((span, false));
                    }
                }
            }
            for (c, complete) in commands {
                let argv: Vec<&str> = c.split_whitespace().collect();
                if let Err(e) = super::super::Cli::try_parse_from(&argv) {
                    let partial = matches!(
                        e.kind(),
                        ErrorKind::MissingRequiredArgument
                            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    );
                    assert!(
                        !complete && partial,
                        "{}: `{c}` does not parse:
{e}",
                        s.name
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 30, "only {checked} commands found");
    }
}
