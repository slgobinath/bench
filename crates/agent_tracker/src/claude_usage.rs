//! How much of your Claude plan is used, in the status bar.
//!
//! # Where the numbers come from
//!
//! Claude Code already has them: every reply from Anthropic carries how much
//! of the five-hour session and the weekly limit is used, and when each
//! resets. Claude Code passes those to its status line command as
//! `rate_limits`, a documented part of the status line's input. Bench installs
//! a status line command that saves that input to a file, and the tracker's
//! sweep reads the file — the same arrangement as the hooks in [`crate::hooks`].
//!
//! So Bench signs in to nothing, reads no credentials and makes no requests.
//! The cost is that the numbers are as fresh as the last reply any Claude
//! session got, which is why the status bar says how old they are.
//!
//! Claude Code has one status line command. Installing replaces whatever was
//! there, and keeps it, so uninstalling puts it back as it was.

use std::f32::consts::{FRAC_PI_2, TAU};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use gpui::{
    Entity, Hsla, PathBuilder, Pixels, Point, PromptLevel, Subscription, Task, actions, canvas,
    point,
};
use serde::Deserialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, UtcOffset};
use ui::{ButtonLike, Tooltip, prelude::*};
use workspace::{
    HideStatusItem, StatusItemView, Toast, Workspace, item::ItemHandle,
    notifications::NotificationId,
};

use crate::hooks::{claude_settings_path, read_settings, shell_quote, write_settings};
use crate::{AgentTracker, AgentsChanged};

actions!(
    agents,
    [
        /// Sets Claude Code's status line to record your plan usage, so that
        /// Bench can show it in the status bar.
        InstallClaudeStatusLine,
        /// Puts back the status line Claude Code had before Bench's.
        UninstallClaudeStatusLine,
    ]
);

/// Where Bench's status line command saves Claude Code's input.
pub fn usage_path() -> PathBuf {
    paths::data_dir().join("claude-usage.json")
}

/// Where the status line that Bench's replaced is kept, to put back.
fn previous_path() -> PathBuf {
    paths::data_dir().join("claude-status-line-before-bench.json")
}

/// Where the numbers can be read in full, from the status bar item.
const USAGE_PAGE: &str = "https://claude.ai/settings/usage";

/// How often the status bar item redraws on its own, so that "as of 3 min
/// ago" and a window that has since reset stay true between reports.
const REDRAW_EVERY: Duration = Duration::from_secs(30);

/// One of the plan's limits: how much of it is used, and when it starts over.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsageWindow {
    /// 0 to 100.
    pub used_percentage: f64,
    /// Seconds since the Unix epoch.
    pub resets_at: Option<i64>,
}

impl UsageWindow {
    /// The share used as of `now`. A window that has reset since it was
    /// reported starts again from nothing; Bench only hears otherwise with the
    /// next reply.
    pub fn used_at(&self, now: i64) -> f64 {
        match self.resets_at {
            Some(resets_at) if resets_at <= now => 0.,
            _ => self.used_percentage.clamp(0., 100.),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeUsage {
    /// The five-hour session limit.
    pub session: Option<UsageWindow>,
    /// The seven-day limit.
    pub weekly: Option<UsageWindow>,
    /// When Claude Code last reported them.
    pub reported_at: SystemTime,
}

/// The part of the status line's input Bench reads. Claude Code sends a good
/// deal more — the model, the workspace, the cost — and none of it is needed.
#[derive(Deserialize)]
struct StatusLineInput {
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct RateLimits {
    five_hour: Option<RawWindow>,
    seven_day: Option<RawWindow>,
}

#[derive(Deserialize)]
struct RawWindow {
    used_percentage: Option<f64>,
    resets_at: Option<f64>,
}

impl RawWindow {
    fn into_window(self) -> Option<UsageWindow> {
        Some(UsageWindow {
            used_percentage: self.used_percentage?,
            resets_at: self.resets_at.map(|resets_at| resets_at as i64),
        })
    }
}

/// Reads what the status line saved, if it has changed since `last_modified`.
///
/// `None` is nothing new — the ordinary answer, since this is asked every
/// sweep. `Some((modified, None))` is a file that could not be read as usage,
/// which leaves what was known in place rather than blanking the status bar.
pub fn read_if_changed(
    last_modified: Option<SystemTime>,
) -> Option<(SystemTime, Option<ClaudeUsage>)> {
    let path = usage_path();
    let modified = std::fs::metadata(&path).and_then(|it| it.modified()).ok()?;
    if Some(modified) == last_modified {
        return None;
    }
    let usage = match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text, modified),
        Err(error) => {
            log::warn!("reading {}: {error:#}", path.display());
            None
        }
    };
    Some((modified, usage))
}

fn parse(text: &str, reported_at: SystemTime) -> Option<ClaudeUsage> {
    let input: StatusLineInput = serde_json::from_str(text).ok()?;
    let limits = input.rate_limits?;
    let usage = ClaudeUsage {
        session: limits.five_hour.and_then(RawWindow::into_window),
        weekly: limits.seven_day.and_then(RawWindow::into_window),
        reported_at,
    };
    (usage.session.is_some() || usage.weekly.is_some()).then_some(usage)
}

/// Whether Claude Code's status line is Bench's.
pub fn installed() -> bool {
    read_settings(&claude_settings_path())
        .ok()
        .and_then(|settings| settings.get("statusLine").map(is_ours))
        .unwrap_or(false)
}

/// Whether Claude Code has a status line of its own that installing would
/// replace — the question to ask before doing it.
fn has_other_status_line() -> bool {
    read_settings(&claude_settings_path())
        .ok()
        .and_then(|settings| settings.get("statusLine").map(|it| !is_ours(it)))
        .unwrap_or(false)
}

fn install() -> Result<PathBuf> {
    let path = claude_settings_path();
    install_at(&path, &previous_path(), &usage_path())?;
    Ok(path)
}

fn install_at(settings_path: &Path, previous_path: &Path, usage_path: &Path) -> Result<()> {
    let mut settings = read_settings(settings_path)?;
    let current = settings.get("statusLine").cloned();
    // Only what was there before Bench's is worth keeping: installing twice
    // must not save Bench's own command as the one to put back.
    if !current.as_ref().is_some_and(is_ours_at(usage_path)) {
        let previous = serde_json::to_string_pretty(&current.unwrap_or(Value::Null))?;
        if let Some(directory) = previous_path.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("creating {}", directory.display()))?;
        }
        std::fs::write(previous_path, previous)
            .with_context(|| format!("writing {}", previous_path.display()))?;
    }
    settings.insert(
        "statusLine".into(),
        json!({ "type": "command", "command": command_for(usage_path) }),
    );
    write_settings(settings_path, &settings)
}

enum Uninstalled {
    PutBack,
    Removed,
    /// Someone else's status line is there now; it is left alone.
    NotOurs,
}

fn uninstall() -> Result<Uninstalled> {
    uninstall_at(&claude_settings_path(), &previous_path(), &usage_path())
}

fn uninstall_at(
    settings_path: &Path,
    previous_path: &Path,
    usage_path: &Path,
) -> Result<Uninstalled> {
    let mut settings = read_settings(settings_path)?;
    if !settings
        .get("statusLine")
        .is_some_and(is_ours_at(usage_path))
    {
        return Ok(Uninstalled::NotOurs);
    }
    let previous = match std::fs::read_to_string(previous_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", previous_path.display()));
        }
    };
    let outcome = if previous.is_null() {
        settings.remove("statusLine");
        Uninstalled::Removed
    } else {
        settings.insert("statusLine".into(), previous);
        Uninstalled::PutBack
    };
    write_settings(settings_path, &settings)?;
    if let Err(error) = std::fs::remove_file(previous_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("removing {}: {error:#}", previous_path.display());
    }
    Ok(outcome)
}

/// The status line command: save Claude Code's input, when it carries the
/// plan's limits, to `path`.
///
/// Only input with `rate_limits` is kept, because a session on an API key, or
/// one that has not had a reply yet, sends none, and saving it would wipe the
/// numbers another session reported. It is written beside the file and moved
/// into place, so the sweep never reads half of it; the name carries the
/// shell's pid because two sessions can report at once. It prints nothing, so
/// Claude Code's own status line is empty.
fn command_for(path: &Path) -> String {
    format!(
        r#"f={}; d=$(cat); case "$d" in *'"rate_limits"'*) printf '%s' "$d" > "$f.$$" && mv -f "$f.$$" "$f";; esac"#,
        shell_quote(path)
    )
}

fn is_ours(status_line: &Value) -> bool {
    is_ours_at(&usage_path())(status_line)
}

/// Bench's status line is recognised by the file it saves to, as the hooks
/// are by the file they append to.
fn is_ours_at(usage_path: &Path) -> impl Fn(&Value) -> bool + '_ {
    move |status_line| {
        status_line
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| command.contains(&*usage_path.to_string_lossy()))
    }
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_, _: &InstallClaudeStatusLine, window, cx| {
            confirm_and_install(window, cx);
        });
        workspace.register_action(|workspace, _: &UninstallClaudeStatusLine, _, cx| {
            let message = match uninstall() {
                Ok(Uninstalled::PutBack) => {
                    "Claude Code's previous status line is back.".to_owned()
                }
                Ok(Uninstalled::Removed) => {
                    "Bench's status line is out of Claude Code's settings.".to_owned()
                }
                Ok(Uninstalled::NotOurs) => {
                    "Claude Code's status line is not Bench's, so it was left alone.".to_owned()
                }
                Err(error) => {
                    log::error!("uninstalling Bench's Claude status line: {error:#}");
                    format!("Could not change Claude Code's settings: {error}")
                }
            };
            set_installed(installed(), cx);
            toast(message, workspace, cx);
        });
    })
    .detach();
}

/// Installs, asking first when that would replace a status line of the
/// user's own.
fn confirm_and_install(window: &mut Window, cx: &mut Context<Workspace>) {
    let answer = has_other_status_line().then(|| {
        window.prompt(
            PromptLevel::Info,
            "Replace Claude Code's status line?",
            Some(
                "Bench shows your Claude usage by reading what Claude Code passes to its \
                 status line. Your current status line command is kept, and \
                 `agents: uninstall claude status line` puts it back.",
            ),
            &["Replace", "Cancel"],
            cx,
        )
    });
    cx.spawn_in(window, async move |workspace, cx| {
        if let Some(answer) = answer
            && answer.await? != 0
        {
            return anyhow::Ok(());
        }
        let message = match install() {
            Ok(path) => format!(
                "Claude Code will now report your usage to Bench ({}). It shows up after \
                 the next reply in any Claude session.",
                path.display()
            ),
            Err(error) => {
                log::error!("installing Bench's Claude status line: {error:#}");
                format!("Could not change Claude Code's settings: {error}")
            }
        };
        workspace.update(cx, |workspace, cx| {
            set_installed(installed(), cx);
            toast(message, workspace, cx);
        })
    })
    .detach_and_log_err(cx);
}

fn set_installed(installed: bool, cx: &mut App) {
    if let Some(tracker) = AgentTracker::try_global(cx) {
        tracker.update(cx, |tracker, cx| {
            tracker.set_claude_status_line_installed(installed, cx)
        });
    }
}

fn toast(message: String, workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<InstallClaudeStatusLine>(), message),
        cx,
    );
}

/// The status bar's view of your Claude plan: a ring filling up with the
/// session's usage, and the percentage beside it.
pub struct ClaudeUsageButton {
    tracker: Option<Entity<AgentTracker>>,
    _subscription: Option<Subscription>,
    _redraw: Task<()>,
}

impl ClaudeUsageButton {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let tracker = AgentTracker::try_global(cx);
        let subscription = tracker
            .as_ref()
            .map(|tracker| cx.subscribe(tracker, |_, _, _: &AgentsChanged, cx| cx.notify()));
        let redraw = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REDRAW_EVERY).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        });
        Self {
            tracker,
            _subscription: subscription,
            _redraw: redraw,
        }
    }
}

impl Render for ClaudeUsageButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(tracker) = self.tracker.as_ref().map(|tracker| tracker.read(cx)) else {
            return div().into_any_element();
        };
        let usage = tracker.claude_usage().cloned();
        let installed = tracker.claude_status_line_installed();
        let now = unix_now();
        let track = cx.theme().colors().border;

        let Some(usage) = usage else {
            // Nothing reported yet. Until the status line is installed, the
            // item is how you find out that it can be.
            let tooltip = if installed {
                "Claude usage shows up after the next reply in any Claude session"
            } else {
                "Show your Claude usage here"
            };
            return ButtonLike::new("claude-usage")
                .child(render_ring(0., track, track))
                .tooltip(Tooltip::text(tooltip))
                .on_click(move |_, window, cx| {
                    if !installed {
                        window.dispatch_action(Box::new(InstallClaudeStatusLine), cx);
                    }
                })
                .into_any_element();
        };

        let session = usage.session.map(|window| window.used_at(now));
        let shown = session
            .or_else(|| usage.weekly.map(|window| window.used_at(now)))
            .unwrap_or(0.);
        let fill = usage_color(shown).color(cx);
        let details = usage;
        ButtonLike::new("claude-usage")
            .child(
                h_flex()
                    .gap_1()
                    .child(render_ring(shown as f32 / 100., fill, track))
                    .child(
                        Label::new(format!("{shown:.0}%"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .tooltip(Tooltip::element(move |_, _| {
                render_details(&details, unix_now()).into_any_element()
            }))
            .on_click(|_, _, cx| cx.open_url(USAGE_PAGE))
            .into_any_element()
    }
}

fn render_details(usage: &ClaudeUsage, now: i64) -> impl IntoElement {
    let row = |name: &'static str, window: Option<UsageWindow>| {
        h_flex()
            .gap_3()
            .justify_between()
            .child(Label::new(name).size(LabelSize::Small))
            .child(Label::new(match window {
                Some(window) => format!(
                    "{:.0}%{}",
                    window.used_at(now),
                    window
                        .resets_at
                        .filter(|resets_at| *resets_at > now)
                        .map(|resets_at| format!(" · resets {}", format_reset(resets_at, now)))
                        .unwrap_or_default()
                ),
                None => "–".to_owned(),
            })
            .size(LabelSize::Small)
            .color(Color::Muted))
    };
    let age = usage
        .reported_at
        .elapsed()
        .map(format_age)
        .unwrap_or_else(|_| "just now".to_owned());
    v_flex()
        .gap_1()
        .child(Label::new("Claude usage"))
        .child(row("Session", usage.session))
        .child(row("Weekly", usage.weekly))
        .child(
            Label::new(format!("As of {age} · click for details"))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
}

/// The ring beside the percentage: the track all the way round, and the used
/// share over it, clockwise from the top. Drawn in pieces of at most a quarter
/// turn, as the dashboard's donut is.
fn render_ring(fraction: f32, fill: Hsla, track: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let center = bounds.center();
            let outer = bounds.size.width.min(bounds.size.height) / 2.;
            let inner = outer * 0.55;
            for (from, sweep, color) in [
                (0., 1., track),
                (0., fraction.clamp(0., 1.), fill),
            ] {
                if sweep <= 0. {
                    continue;
                }
                let start = -FRAC_PI_2 + from * TAU;
                let sweep = sweep * TAU;
                let pieces = (sweep / FRAC_PI_2).ceil().max(1.) as usize;
                let step = sweep / pieces as f32;
                for piece in 0..pieces {
                    let a = start + step * piece as f32;
                    let b = a + step;
                    let mut path = PathBuilder::fill();
                    path.move_to(polar(center, outer, a));
                    path.arc_to(point(outer, outer), px(0.), false, true, polar(center, outer, b));
                    path.line_to(polar(center, inner, b));
                    path.arc_to(point(inner, inner), px(0.), false, false, polar(center, inner, a));
                    path.close();
                    match path.build() {
                        Ok(path) => window.paint_path(path, color),
                        Err(error) => log::debug!("drawing the usage ring: {error}"),
                    }
                }
            }
        },
    )
    .flex_none()
    .size(px(12.))
}

fn polar(center: Point<Pixels>, radius: Pixels, angle: f32) -> Point<Pixels> {
    point(
        center.x + radius * angle.cos(),
        center.y + radius * angle.sin(),
    )
}

/// Calm until it matters: the accent colour most of the time, a warning past
/// three quarters, an error once there is little left.
fn usage_color(used: f64) -> Color {
    if used >= 90. {
        Color::Error
    } else if used >= 75. {
        Color::Warning
    } else {
        Color::Accent
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// When a limit resets, where you are: the time alone if that is today, the
/// weekday with it otherwise.
fn format_reset(resets_at: i64, now: i64) -> String {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let (Ok(at), Ok(today)) = (
        OffsetDateTime::from_unix_timestamp(resets_at),
        OffsetDateTime::from_unix_timestamp(now),
    ) else {
        return String::new();
    };
    let at = at.to_offset(offset);
    let (hour, minute) = (at.hour(), at.minute());
    let hour12 = match hour % 12 {
        0 => 12,
        hour => hour,
    };
    let time = format!(
        "{hour12}:{minute:02} {}",
        if hour < 12 { "AM" } else { "PM" }
    );
    if at.date() == today.to_offset(offset).date() {
        time
    } else {
        let weekday = at.weekday().to_string();
        format!("{} {time}", weekday.get(..3).unwrap_or(&weekday))
    }
}

fn format_age(age: Duration) -> String {
    let minutes = age.as_secs() / 60;
    match minutes {
        0 => "just now".to_owned(),
        1..=59 => format!("{minutes} min ago"),
        _ => format!("{} h ago", minutes / 60),
    }
}

impl StatusItemView for ClaudeUsageButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    #[test]
    fn reads_the_limits_claude_code_passes_its_status_line() {
        let text = r#"{
            "model": { "display_name": "Opus" },
            "rate_limits": {
                "five_hour": { "used_percentage": 23.5, "resets_at": 1738425600 },
                "seven_day": { "used_percentage": 41, "resets_at": 1746456789 }
            }
        }"#;
        let usage = parse(text, UNIX_EPOCH).expect("usage");
        assert_eq!(
            usage.session,
            Some(UsageWindow {
                used_percentage: 23.5,
                resets_at: Some(1738425600)
            })
        );
        assert_eq!(usage.weekly.map(|window| window.used_percentage), Some(41.));
    }

    #[test]
    fn input_without_limits_is_not_usage() {
        assert!(parse(r#"{ "model": { "display_name": "Opus" } }"#, UNIX_EPOCH).is_none());
        assert!(parse(r#"{ "rate_limits": {} }"#, UNIX_EPOCH).is_none());
        assert!(parse("not json", UNIX_EPOCH).is_none());
    }

    #[test]
    fn a_window_that_has_reset_starts_again_from_nothing() {
        let window = UsageWindow {
            used_percentage: 80.,
            resets_at: Some(1_000),
        };
        assert_eq!(window.used_at(999), 80.);
        assert_eq!(window.used_at(1_000), 0.);
    }

    /// The command runs in `sh` with Claude Code's input on stdin. Input with
    /// the limits is saved; input without is dropped, so it cannot wipe what
    /// another session reported.
    // Blocking is what a test wants here: it waits for `sh` to finish.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn the_status_line_command_saves_only_input_with_limits() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let directory = tempfile::tempdir().expect("a temp dir");
        let path = directory.path().join("a dir with spaces").join("usage.json");
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the dir");

        let run = |input: &str| {
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg(command_for(&path))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("sh");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(input.as_bytes())
                .expect("the input");
            let output = child.wait_with_output().expect("sh to finish");
            assert!(output.status.success());
            assert!(output.stdout.is_empty(), "the status line prints nothing");
        };

        let with_limits = r#"{"rate_limits":{"five_hour":{"used_percentage":12,"resets_at":1}}}"#;
        run(with_limits);
        assert_eq!(std::fs::read_to_string(&path).expect("saved"), with_limits);

        run(r#"{"model":{"display_name":"Opus"}}"#);
        assert_eq!(
            std::fs::read_to_string(&path).expect("still saved"),
            with_limits,
            "input without limits leaves the last report alone"
        );
    }

    /// Installing keeps the status line that was there, installing twice does
    /// not lose it, and uninstalling puts it back.
    #[test]
    fn installing_keeps_the_previous_status_line_to_put_back() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let settings_path = directory.path().join("settings.json");
        let previous_path = directory.path().join("previous.json");
        let usage_path = directory.path().join("usage.json");
        let theirs = json!({ "type": "command", "command": "~/my-status-line.sh" });
        std::fs::write(
            &settings_path,
            json!({ "model": "opus", "statusLine": theirs }).to_string(),
        )
        .expect("their settings");

        install_at(&settings_path, &previous_path, &usage_path).expect("install");
        install_at(&settings_path, &previous_path, &usage_path).expect("install again");

        let settings: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(is_ours_at(&usage_path)(&settings["statusLine"]));
        assert_eq!(settings["model"], "opus", "other settings are left alone");

        let outcome =
            uninstall_at(&settings_path, &previous_path, &usage_path).expect("uninstall");
        assert!(matches!(outcome, Uninstalled::PutBack));
        let settings: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["statusLine"], theirs, "theirs is back as it was");
        assert!(!previous_path.exists());
    }

    #[test]
    fn uninstalling_with_nothing_before_removes_the_status_line() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let settings_path = directory.path().join("settings.json");
        let previous_path = directory.path().join("previous.json");
        let usage_path = directory.path().join("usage.json");

        install_at(&settings_path, &previous_path, &usage_path).expect("install");
        let outcome =
            uninstall_at(&settings_path, &previous_path, &usage_path).expect("uninstall");

        assert!(matches!(outcome, Uninstalled::Removed));
        let settings: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(settings.get("statusLine").is_none());
    }

    /// A status line someone set after Bench's is theirs, not Bench's to
    /// remove.
    #[test]
    fn uninstalling_leaves_a_status_line_that_is_not_ours() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let settings_path = directory.path().join("settings.json");
        let theirs = json!({ "type": "command", "command": "echo hi" });
        std::fs::write(&settings_path, json!({ "statusLine": theirs }).to_string())
            .expect("their settings");

        let outcome = uninstall_at(
            &settings_path,
            &directory.path().join("previous.json"),
            &directory.path().join("usage.json"),
        )
        .expect("uninstall");

        assert!(matches!(outcome, Uninstalled::NotOurs));
        let settings: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["statusLine"], theirs);
    }

    /// The status bar item draws with nothing reported and with a report —
    /// the dashboard once crashed on its first draw, and a status bar item is
    /// drawn in every window.
    #[gpui::test]
    async fn the_status_bar_item_draws(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::init(cx);
        });
        let (_button, cx) = cx.add_window_view(|_, cx| ClaudeUsageButton::new(cx));
        cx.run_until_parked();

        cx.update(|_, cx| {
            let tracker = AgentTracker::try_global(cx).expect("the tracker");
            tracker.update(cx, |tracker, cx| {
                tracker.take_claude_usage(
                    Some((
                        SystemTime::now(),
                        parse(
                            r#"{"rate_limits":{"five_hour":{"used_percentage":82,"resets_at":4102444800}}}"#,
                            SystemTime::now(),
                        ),
                    )),
                    cx,
                );
            });
            assert!(tracker.read(cx).claude_usage().is_some());
        });
        cx.run_until_parked();
    }

    #[test]
    fn ages_read_as_people_say_them() {
        assert_eq!(format_age(Duration::from_secs(20)), "just now");
        assert_eq!(format_age(Duration::from_secs(180)), "3 min ago");
        assert_eq!(format_age(Duration::from_secs(7_300)), "2 h ago");
    }
}
