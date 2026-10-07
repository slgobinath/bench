//! Which Claude model an agent is using, as a small badge.
//!
//! `/model` switches are easy to forget: a session moved to Sonnet for one
//! task goes on being Sonnet for every task after it, and nothing on screen
//! says so. The badge says so, on the terminal's tab and on the worktree's
//! card.
//!
//! # Where the model comes from
//!
//! Claude Code keeps a transcript of every session in
//! `~/.claude/projects/<working directory>/<session>.jsonl`, and every reply in
//! it records the model that wrote it. So the answer is read off the end of the
//! newest transcript in a terminal's working directory: no hook, no status line
//! command, nothing to install. `/model` is recorded too, as a line saying what
//! it was set to, so a switch shows before the next reply does.
//!
//! The newest transcript is a good guess rather than the session itself: two
//! agents in one directory share it.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gpui::{Div, Hsla, SharedString, Stateful, hsla};
use serde_json::Value;
use ui::{Tooltip, prelude::*};

/// How much of a badge is left when its agent is idle.
const IDLE_OPACITY: f32 = 0.45;

/// How much of the end of a transcript is searched for the model. A reply
/// with a long file in it is large; anything past this is older than the
/// model anyone is using.
const TAIL_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFamily {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Other,
}

impl ModelFamily {
    fn of(name: &str) -> Self {
        let name = name.to_lowercase();
        if name.contains("opus") {
            Self::Opus
        } else if name.contains("sonnet") {
            Self::Sonnet
        } else if name.contains("haiku") {
            Self::Haiku
        } else if name.contains("fable") {
            Self::Fable
        } else {
            Self::Other
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Sonnet => "Sonnet",
            Self::Haiku => "Haiku",
            Self::Fable => "Fable",
            Self::Other => "Claude",
        }
    }

    /// A colour per family, the same in every theme: the badge is there to
    /// be told apart at a glance, and a theme's palette is not made for that.
    fn color(self) -> Hsla {
        match self {
            Self::Opus => hsla(0.38, 0.65, 0.5, 1.),
            Self::Sonnet => hsla(0.07, 0.95, 0.58, 1.),
            Self::Haiku => hsla(0.0, 0.8, 0.6, 1.),
            Self::Fable => hsla(0.74, 0.75, 0.68, 1.),
            Self::Other => hsla(0.58, 0.5, 0.65, 1.),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeModel {
    pub family: ModelFamily,
    /// What it is called in full, such as `Sonnet 5.5`.
    pub label: SharedString,
}

impl ClaudeModel {
    /// From a model id such as `claude-sonnet-5-5` or
    /// `claude-3-5-sonnet-20241022`.
    fn from_id(id: &str) -> Option<Self> {
        let family = ModelFamily::of(id);
        if family == ModelFamily::Other {
            return None;
        }
        // The version is the short numbers in the id; the long one is a date.
        let version: Vec<&str> = id
            .split('-')
            .filter(|part| (1..=2).contains(&part.len()) && part.chars().all(|c| c.is_ascii_digit()))
            .collect();
        let label = if version.is_empty() {
            family.name().to_owned()
        } else {
            format!("{} {}", family.name(), version.join("."))
        };
        Some(Self {
            family,
            label: label.into(),
        })
    }

    /// From the name `/model` reports, such as `Sonnet 5.5`.
    fn from_name(name: &str) -> Option<Self> {
        let family = ModelFamily::of(name);
        if family == ModelFamily::Other {
            return None;
        }
        Some(Self {
            family,
            label: name.trim().to_owned().into(),
        })
    }

    /// The badge: the family in its colour, with the full name on hover.
    /// Dimmed when the agent is not `active`, so that the ones that are
    /// working or waiting on you are the ones that stand out.
    pub fn badge(&self, id: impl Into<ElementId>, active: bool) -> Stateful<Div> {
        let color = self.family.color();
        let label = self.label.clone();
        h_flex()
            .id(id)
            .flex_none()
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(color.opacity(0.55))
            .bg(color.opacity(0.18))
            .when(!active, |this| this.opacity(IDLE_OPACITY))
            .child(
                Label::new(self.family.name())
                    .size(LabelSize::XSmall)
                    .color(Color::Custom(color))
                    .single_line(),
            )
            .tooltip(Tooltip::text(format!("Claude {label}")))
    }
}

/// What was found in a directory's newest transcript, kept so that a
/// transcript that has not changed is not read again.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    path: PathBuf,
    modified: SystemTime,
    pub model: Option<ClaudeModel>,
}

/// The model of the newest Claude Code transcript for sessions run in
/// `directory`. `None` where there is no transcript at all.
///
/// Blocking, for a background thread: it lists a directory and reads the end
/// of a file.
pub fn probe(directory: &Path, previous: Option<&Probe>) -> Option<Probe> {
    let projects = paths::home_dir().join(".claude").join("projects");
    probe_in(&projects, directory, previous)
}

fn probe_in(projects: &Path, directory: &Path, previous: Option<&Probe>) -> Option<Probe> {
    let transcripts = projects.join(project_directory_name(directory));
    let (path, modified) = std::fs::read_dir(&transcripts)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| Some((entry.path(), entry.metadata().ok()?.modified().ok()?)))
        .max_by_key(|(_, modified)| *modified)?;

    if let Some(previous) = previous
        && previous.path == path
        && previous.modified == modified
    {
        return Some(previous.clone());
    }
    let model = match read_tail(&path) {
        Ok(tail) => model_in(&tail),
        Err(error) => {
            log::warn!("reading {}: {error:#}", path.display());
            previous.and_then(|previous| previous.model.clone())
        }
    };
    Some(Probe {
        path,
        modified,
        model,
    })
}

/// How Claude Code names the folder of a directory's transcripts: every
/// character that is not a letter or a digit becomes a dash.
fn project_directory_name(directory: &Path) -> String {
    directory
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn read_tail(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The model in force at the end of a transcript's text: that of the last
/// reply, or what the last `/model` set, whichever came later. The first line
/// may be cut in half by where the text starts, and is skipped if it is.
fn model_in(tail: &str) -> Option<ClaudeModel> {
    tail.lines().rev().find_map(|line| {
        if !line.contains("\"model\"") && !line.contains("Set model to") {
            return None;
        }
        let entry: Value = serde_json::from_str(line).ok()?;
        match entry.get("type").and_then(Value::as_str)? {
            "assistant" => {
                let id = entry.pointer("/message/model").and_then(Value::as_str)?;
                ClaudeModel::from_id(id)
            }
            "user" => {
                let content = entry.pointer("/message/content").and_then(Value::as_str)?;
                set_model_name(content).and_then(ClaudeModel::from_name)
            }
            _ => None,
        }
    })
}

/// The name in `<local-command-stdout>Set model to `Sonnet 5.5` and saved …`.
fn set_model_name(content: &str) -> Option<&str> {
    let after = content
        .split_once("<local-command-stdout>")?
        .1
        .strip_prefix("Set model to ")?;
    let name = after
        .split(" and ")
        .next()?
        .split("</local-command-stdout>")
        .next()?;
    Some(name.trim().trim_matches('`').trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(family: ModelFamily, label: &str) -> ClaudeModel {
        ClaudeModel {
            family,
            label: label.to_owned().into(),
        }
    }

    #[test]
    fn model_ids_become_a_family_and_a_version() {
        let cases = [
            ("claude-sonnet-5-5", ModelFamily::Sonnet, "Sonnet 5.5"),
            ("claude-opus-4-1-20250805", ModelFamily::Opus, "Opus 4.1"),
            ("claude-3-5-sonnet-20241022", ModelFamily::Sonnet, "Sonnet 3.5"),
            ("claude-haiku-4-5-20251001", ModelFamily::Haiku, "Haiku 4.5"),
            ("claude-fable-5-1", ModelFamily::Fable, "Fable 5.1"),
        ];
        for (id, family, label) in cases {
            assert_eq!(ClaudeModel::from_id(id), Some(model(family, label)), "{id}");
        }
        assert_eq!(ClaudeModel::from_id("<synthetic>"), None);
    }

    #[test]
    fn directories_are_named_the_way_claude_code_names_them() {
        assert_eq!(
            project_directory_name(Path::new("/Users/me/Developer/bench")),
            "-Users-me-Developer-bench"
        );
        assert_eq!(
            project_directory_name(Path::new("/Users/me/.localflow/work")),
            "-Users-me--localflow-work"
        );
    }

    #[test]
    fn the_last_reply_or_model_switch_wins() {
        let reply = |id: &str| {
            format!(r#"{{"type":"assistant","message":{{"role":"assistant","model":"{id}"}}}}"#)
        };
        let switch = r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>Set model to `Sonnet 5.5` and saved as your default for new sessions</local-command-stdout>"}}"#;
        let synthetic = reply("<synthetic>");

        let tail = [reply("claude-opus-5-5"), synthetic.clone()].join("\n");
        assert_eq!(model_in(&tail), Some(model(ModelFamily::Opus, "Opus 5.5")));

        let tail = [reply("claude-opus-5-5"), switch.to_owned()].join("\n");
        assert_eq!(model_in(&tail), Some(model(ModelFamily::Sonnet, "Sonnet 5.5")));

        let tail = [switch.to_owned(), reply("claude-opus-5-5")].join("\n");
        assert_eq!(model_in(&tail), Some(model(ModelFamily::Opus, "Opus 5.5")));

        let tail = ["not json \"model\"".to_owned(), reply("claude-sonnet-5-5")].join("\n");
        assert_eq!(model_in(&tail), Some(model(ModelFamily::Sonnet, "Sonnet 5.5")));
        assert_eq!(model_in("nothing here"), None);
    }

    #[test]
    fn an_unchanged_transcript_is_not_read_again() {
        let root = std::env::temp_dir().join(format!("bench-claude-model-{}", std::process::id()));
        let directory = Path::new("/work/tree");
        let transcripts = root.join(project_directory_name(directory));
        std::fs::create_dir_all(&transcripts).expect("a directory");
        let transcript = transcripts.join("session.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5"}}"#,
        )
        .expect("a transcript");

        let first = probe_in(&root, directory, None).expect("a probe");
        assert_eq!(first.model, Some(model(ModelFamily::Opus, "Opus 5.5")));

        let mut remembered = first.clone();
        remembered.model = Some(model(ModelFamily::Haiku, "Haiku"));
        let second = probe_in(&root, directory, Some(&remembered)).expect("a probe");
        assert_eq!(second.model, remembered.model, "taken from what was known");

        assert!(probe_in(&root, Path::new("/elsewhere"), None).is_none());
        std::fs::remove_dir_all(&root).expect("cleaning up");
    }
}
