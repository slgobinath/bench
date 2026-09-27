//! A dashboard of Linear issues in a tab: how much got done, week by week or
//! month by month, where the rest stands, and how the current cycle is going.
//!
//! Filtered like the panel, with a filter of its own: the panel is for picking
//! the next thing to do, the dashboard for looking back, and the two want
//! different defaults.

use std::f32::consts::{FRAC_PI_2, TAU};

use gpui::{
    App, Entity, EventEmitter, FocusHandle, Focusable, Hsla, PathBuilder, Pixels, Point,
    SharedString, Task, WeakEntity, Window, actions, canvas, point, prelude::*,
};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};
use ui::{Tooltip, prelude::*};
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};

use crate::linear_panel::{FilterTarget, filters_button};
use crate::{DashboardIssue, IssueFilters, Linear, LinearEvent, StateType};

actions!(
    linear,
    [
        /// Opens the Linear dashboard: issues completed over time, where the
        /// rest stand, and the current cycle's progress.
        OpenDashboard,
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenDashboard, window, cx| {
            open_dashboard(workspace, window, cx);
        });
    })
    .detach();
}

/// Opens the dashboard in the workspace, or goes to it if it is already open.
pub fn open_dashboard(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<DashboardView>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let Some(linear) = Linear::global(cx) else {
        return;
    };
    let view = cx.new(|cx| DashboardView::new(linear, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

/// Opens the dashboard in the workspace behind a weak handle.
pub(crate) fn open_dashboard_in(workspace: &WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    workspace
        .update(cx, |workspace, cx| open_dashboard(workspace, window, cx))
        .ok();
}

/// How finely the completed issues are counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    Weeks,
    Months,
}

/// How many weeks or months the chart shows.
const BUCKETS: usize = 12;

const CHART_HEIGHT: f32 = 140.;
const DONUT_SIZE: f32 = 150.;

/// What the dashboard starts filtered to: your own issues, in any state.
/// Unlike the panel, finished issues are the point here.
fn default_filters() -> IssueFilters {
    IssueFilters {
        states: Vec::new(),
        ..IssueFilters::default()
    }
}

enum Loaded {
    Loading,
    Failed(SharedString),
    Issues {
        issues: Vec<DashboardIssue>,
        truncated: bool,
    },
}

pub struct DashboardView {
    linear: Entity<Linear>,
    filters: IssueFilters,
    period: Period,
    loaded: Loaded,
    focus_handle: FocusHandle,
    _load: Task<()>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl DashboardView {
    fn new(linear: Entity<Linear>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Connecting after the tab was opened — or the projects and labels the
        // filters offer arriving — is a reason to look again.
        let subscriptions = vec![cx.subscribe_in(
            &linear,
            window,
            |this: &mut Self, linear, _: &LinearEvent, _window, cx| {
                if matches!(this.loaded, Loaded::Failed(_)) && linear.read(cx).is_connected() {
                    this.reload(cx);
                }
                cx.notify();
            },
        )];
        let mut this = Self {
            linear,
            filters: default_filters(),
            period: Period::Weeks,
            loaded: Loaded::Loading,
            focus_handle: cx.focus_handle(),
            _load: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let offset = local_offset();
        let today = OffsetDateTime::now_utc().to_offset(offset).date();
        let Some(since) = bucket_starts(self.period, today)
            .first()
            .and_then(|start| format_start(*start, offset))
        else {
            self.loaded = Loaded::Failed("Could not work out the dates to show.".into());
            return;
        };
        let fetched = self
            .linear
            .read_with(cx, |linear, cx| linear.dashboard_issues(&self.filters, since, cx));
        self.loaded = Loaded::Loading;
        self._load = cx.spawn(async move |this, cx| {
            let fetched = fetched.await;
            this.update(cx, |this, cx| {
                this.loaded = match fetched {
                    Ok(fetched) => Loaded::Issues {
                        issues: fetched.issues,
                        truncated: fetched.truncated,
                    },
                    Err(error) => Loaded::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn set_period(&mut self, period: Period, cx: &mut Context<Self>) {
        if self.period != period {
            self.period = period;
            self.reload(cx);
        }
    }

    /// The dashboard's own filters. Only a change to what is fetched asks
    /// Linear again: the status filter narrows what is already here.
    fn filter_target(&self, cx: &Context<Self>) -> FilterTarget {
        let read = cx.entity().downgrade();
        let write = read.clone();
        FilterTarget::new(
            move |cx| read.upgrade().map(|view| view.read(cx).filters.clone()),
            move |change, cx| {
                write
                    .update(cx, |this, cx| {
                        let before = this.filters.clone();
                        change(&mut this.filters);
                        if fetched_by(&this.filters) != fetched_by(&before) {
                            this.reload(cx);
                        } else {
                            cx.notify();
                        }
                    })
                    .ok();
            },
            default_filters(),
        )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let period_button = |id: &'static str, label: &'static str, period: Period| {
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .toggle_state(self.period == period)
                .on_click(cx.listener(move |this, _, _, cx| this.set_period(period, cx)))
        };
        h_flex()
            .w_full()
            .justify_between()
            .gap_2()
            .child(Headline::new("Linear").size(HeadlineSize::Large))
            .child(
                h_flex()
                    .gap_1()
                    .child(period_button("period-weeks", "Weekly", Period::Weeks))
                    .child(period_button("period-months", "Monthly", Period::Months))
                    .child(filters_button(
                        "dashboard-filters",
                        self.filter_target(cx),
                        self.linear.clone(),
                        self.filters != default_filters(),
                    ))
                    .child(
                        IconButton::new("reload-dashboard", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .disabled(matches!(self.loaded, Loaded::Loading))
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            )
    }

    fn render_summary(&self, summary: &Summary, cx: &App) -> impl IntoElement {
        let accent = cx.theme().colors().text_accent;
        let border = cx.theme().colors().border_variant;
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_3()
                    .child(stat(summary.completed_this_week.to_string(), "Completed this week", cx))
                    .child(stat(
                        summary.completed_this_month.to_string(),
                        "Completed this month",
                        cx,
                    ))
                    .child(stat(summary.open.to_string(), "Open", cx))
                    .child(stat(summary.in_progress.to_string(), "In progress", cx))
                    .child(stat(
                        summary
                            .median_cycle_days
                            .map(|days| format!("{days:.1}d"))
                            .unwrap_or_else(|| "–".to_string()),
                        "Median cycle time",
                        cx,
                    )),
            )
            .child(card(
                match self.period {
                    Period::Weeks => "Completed per week",
                    Period::Months => "Completed per month",
                },
                render_bars(&summary.buckets, accent),
                cx,
            ))
            .child(
                h_flex()
                    .flex_wrap()
                    .items_start()
                    .gap_3()
                    .child(card("By status", render_statuses(&summary.statuses, cx), cx))
                    .when_some(summary.cycle, |this, (done, total)| {
                        this.child(card(
                            "Current cycle",
                            render_cycle(done, total, accent, border),
                            cx,
                        ))
                    }),
            )
    }
}

impl Render for DashboardView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if !self.linear.read(cx).is_connected() {
            Label::new("Connect Linear in the Linear panel to see the dashboard.")
                .color(Color::Muted)
                .into_any_element()
        } else {
            match &self.loaded {
                Loaded::Loading => Label::new("Loading…").color(Color::Muted).into_any_element(),
                Loaded::Failed(error) => v_flex()
                    .gap_2()
                    .child(Label::new("Could not load the dashboard"))
                    .child(Label::new(error.clone()).size(LabelSize::Small).color(Color::Error))
                    .child(
                        Button::new("retry-dashboard", "Try Again")
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .into_any_element(),
                Loaded::Issues { issues, truncated } => {
                    let offset = local_offset();
                    let today = OffsetDateTime::now_utc().to_offset(offset).date();
                    let summary =
                        summarize(issues, &self.filters.states, self.period, today, offset);
                    v_flex()
                        .gap_2()
                        .child(self.render_summary(&summary, cx))
                        .when(*truncated, |this| {
                            this.child(
                                Label::new(format!(
                                    "Counting the first {} matching issues. Narrow the filters to count them all.",
                                    crate::DASHBOARD_LIMIT
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                        })
                        .into_any_element()
                }
            }
        };

        div()
            .id("linear-dashboard")
            .key_context("LinearDashboard")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .w_full()
                    .max_w(rems(64.))
                    .mx_auto()
                    .p_6()
                    .gap_4()
                    .child(self.render_header(cx))
                    .child(body),
            )
    }
}

fn card(title: &'static str, content: impl IntoElement, cx: &App) -> impl IntoElement {
    v_flex()
        .flex_1()
        .min_w(rems(18.))
        .p_3()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
        .child(content)
}

fn stat(value: String, label: &'static str, cx: &App) -> impl IntoElement {
    v_flex()
        .flex_1()
        .min_w(rems(9.))
        .p_3()
        .gap_0p5()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .child(Headline::new(value).size(HeadlineSize::Large))
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
}

fn render_bars(buckets: &[Bucket], color: Hsla) -> impl IntoElement {
    let most = buckets
        .iter()
        .map(|bucket| bucket.completed)
        .max()
        .unwrap_or(0)
        .max(1);
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .h(px(CHART_HEIGHT + 16.))
                .items_end()
                .gap_1()
                .children(buckets.iter().map(|bucket| {
                    let height = CHART_HEIGHT * bucket.completed as f32 / most as f32;
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .justify_end()
                        .items_center()
                        .gap_0p5()
                        .when(bucket.completed > 0, |this| {
                            this.child(
                                Label::new(bucket.completed.to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })
                        .child(div().w_full().h(px(height)).rounded_t_sm().bg(color))
                })),
        )
        .child(h_flex().gap_1().children(buckets.iter().map(|bucket| {
            div().flex_1().min_w_0().flex().justify_center().child(
                Label::new(bucket.label.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .single_line(),
            )
        })))
}

fn render_statuses(statuses: &[StatusSlice], cx: &App) -> impl IntoElement {
    let total: usize = statuses.iter().map(|slice| slice.count).sum();
    let muted = Color::Muted.color(cx);
    let slices: Vec<(f32, Hsla)> = if total == 0 {
        vec![(1., cx.theme().colors().border_variant)]
    } else {
        statuses
            .iter()
            .map(|slice| {
                (
                    slice.count as f32 / total as f32,
                    slice.color.unwrap_or(muted),
                )
            })
            .collect()
    };
    h_flex()
        .gap_4()
        .items_center()
        .child(render_donut(slices))
        .child(
            v_flex()
                .gap_1()
                .when(statuses.is_empty(), |this| {
                    this.child(Label::new("No issues").size(LabelSize::Small).color(Color::Muted))
                })
                .children(statuses.iter().map(|slice| {
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .size_2()
                                .rounded_full()
                                .bg(slice.color.unwrap_or(muted)),
                        )
                        .child(Label::new(slice.name.clone()).size(LabelSize::Small))
                        .child(
                            Label::new(format!(
                                "{} · {:.0}%",
                                slice.count,
                                100. * slice.count as f32 / total.max(1) as f32
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                })),
        )
}

/// A ring cut into `slices`, each a fraction of the whole, clockwise from the
/// top. Each slice is drawn in pieces of at most a quarter turn, so no arc is
/// ever the ambiguous half-circle-or-more kind.
fn render_donut(slices: Vec<(f32, Hsla)>) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let center = bounds.center();
            let outer = bounds.size.width.min(bounds.size.height) / 2.;
            let inner = outer * 0.62;
            let mut start = -FRAC_PI_2;
            for (fraction, color) in &slices {
                let sweep = fraction * TAU;
                let pieces = (sweep / FRAC_PI_2).ceil().max(1.) as usize;
                let step = sweep / pieces as f32;
                for piece in 0..pieces {
                    let from = start + step * piece as f32;
                    let to = from + step;
                    let mut path = PathBuilder::fill();
                    path.move_to(polar(center, outer, from));
                    path.arc_to(point(outer, outer), px(0.), false, true, polar(center, outer, to));
                    path.line_to(polar(center, inner, to));
                    path.arc_to(point(inner, inner), px(0.), false, false, polar(center, inner, from));
                    path.close();
                    match path.build() {
                        Ok(path) => window.paint_path(path, *color),
                        Err(error) => log::debug!("drawing a dashboard slice: {error}"),
                    }
                }
                start += sweep;
            }
        },
    )
    .flex_none()
    .size(px(DONUT_SIZE))
}

fn polar(center: Point<Pixels>, radius: Pixels, angle: f32) -> Point<Pixels> {
    point(
        center.x + radius * angle.cos(),
        center.y + radius * angle.sin(),
    )
}

fn render_cycle(done: usize, total: usize, fill: Hsla, track: Hsla) -> impl IntoElement {
    let fraction = done as f32 / total.max(1) as f32;
    v_flex()
        .gap_2()
        .child(Headline::new(format!("{:.0}%", 100. * fraction)).size(HeadlineSize::Large))
        .child(
            div()
                .w_full()
                .h_2()
                .rounded_full()
                .bg(track)
                .child(div().h_full().w(relative(fraction)).rounded_full().bg(fill)),
        )
        .child(
            Label::new(format!("{done} of {total} done"))
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
}

impl Focusable for DashboardView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for DashboardView {}

impl Item for DashboardView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Linear Dashboard".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ChartBar))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}

/// The filters that decide what is fetched: all of them but the status, which
/// the dashboard applies to what it already has.
fn fetched_by(filters: &IssueFilters) -> IssueFilters {
    IssueFilters {
        states: Vec::new(),
        ..filters.clone()
    }
}

fn local_offset() -> UtcOffset {
    UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC)
}

/// Midnight at the start of `date`, where you are, as Linear takes a date.
fn format_start(date: Date, offset: UtcOffset) -> Option<String> {
    PrimitiveDateTime::new(date, Time::MIDNIGHT)
        .assume_offset(offset)
        .format(&Rfc3339)
        .ok()
}

/// What the dashboard shows, worked out from the fetched issues.
#[derive(Debug, PartialEq)]
struct Summary {
    buckets: Vec<Bucket>,
    statuses: Vec<StatusSlice>,
    completed_this_week: usize,
    completed_this_month: usize,
    open: usize,
    in_progress: usize,
    /// From starting an issue to completing it, over the issues completed in
    /// the chart's range that were ever started.
    median_cycle_days: Option<f64>,
    /// Done and total, over the issues in a cycle that is running now.
    cycle: Option<(usize, usize)>,
}

#[derive(Debug, PartialEq)]
struct Bucket {
    start: Date,
    label: SharedString,
    completed: usize,
}

#[derive(Debug, PartialEq)]
struct StatusSlice {
    name: SharedString,
    kind: StateType,
    color: Option<Hsla>,
    count: usize,
}

/// The first day of each week (Mondays) or month the chart shows, oldest
/// first, ending with the one `today` is in.
fn bucket_starts(period: Period, today: Date) -> Vec<Date> {
    let current = match period {
        Period::Weeks => today.checked_sub(Duration::days(i64::from(
            today.weekday().number_days_from_monday(),
        ))),
        Period::Months => today.replace_day(1).ok(),
    };
    let Some(current) = current else {
        return Vec::new();
    };
    let mut starts = vec![current];
    while starts.len() < BUCKETS {
        let Some(&newest_old) = starts.first() else {
            break;
        };
        let earlier = match period {
            Period::Weeks => newest_old.checked_sub(Duration::weeks(1)),
            Period::Months => previous_month(newest_old),
        };
        match earlier {
            Some(earlier) => starts.insert(0, earlier),
            None => break,
        }
    }
    starts
}

fn previous_month(first_of_month: Date) -> Option<Date> {
    let (year, month) = match first_of_month.month() {
        Month::January => (first_of_month.year() - 1, Month::December),
        month => (first_of_month.year(), month.previous()),
    };
    Date::from_calendar_date(year, month, 1).ok()
}

fn bucket_label(period: Period, start: Date) -> SharedString {
    let month = start.month().to_string();
    let month = month.get(..3).unwrap_or(&month);
    match period {
        Period::Weeks => format!("{month} {}", start.day()).into(),
        Period::Months if start.month() == Month::January => {
            format!("{month} {}", start.year() % 100).into()
        }
        Period::Months => month.to_string().into(),
    }
}

fn local_date(timestamp: &str, offset: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(timestamp, &Rfc3339)
        .ok()
        .map(|at| at.to_offset(offset))
}

fn summarize(
    issues: &[DashboardIssue],
    states: &[StateType],
    period: Period,
    today: Date,
    offset: UtcOffset,
) -> Summary {
    let starts = bucket_starts(period, today);
    let mut buckets: Vec<Bucket> = starts
        .iter()
        .map(|start| Bucket {
            start: *start,
            label: bucket_label(period, *start),
            completed: 0,
        })
        .collect();
    let range_start = starts.first().copied();
    let week_start = bucket_starts(Period::Weeks, today).last().copied();
    let month_start = today.replace_day(1).ok();
    let shown = |kind: StateType| states.is_empty() || states.contains(&kind);

    let mut completed_this_week = 0;
    let mut completed_this_month = 0;
    let mut cycle_days = Vec::new();
    let mut statuses: Vec<StatusSlice> = Vec::new();
    let mut open = 0;
    let mut in_progress = 0;
    let mut cycle_done = 0;
    let mut cycle_total = 0;

    for issue in issues {
        let kind = issue.state.kind;
        if let Some(completed) = issue
            .completed_at
            .as_deref()
            .and_then(|at| local_date(at, offset))
        {
            let day = completed.date();
            if week_start.is_some_and(|start| day >= start) {
                completed_this_week += 1;
            }
            if month_start.is_some_and(|start| day >= start) {
                completed_this_month += 1;
            }
            if let Some(bucket) = buckets.iter_mut().rev().find(|bucket| bucket.start <= day) {
                bucket.completed += 1;
            }
            if range_start.is_some_and(|start| day >= start)
                && let Some(started) = issue
                    .started_at
                    .as_deref()
                    .and_then(|at| local_date(at, offset))
            {
                cycle_days.push((completed - started).as_seconds_f64() / 86_400.);
            }
        }

        if issue.cycle.as_ref().is_some_and(|cycle| cycle.is_active) && kind != StateType::Canceled
        {
            cycle_total += 1;
            if kind == StateType::Completed {
                cycle_done += 1;
            }
        }

        if !shown(kind) {
            continue;
        }
        if !matches!(kind, StateType::Completed | StateType::Canceled) {
            open += 1;
            if kind == StateType::Started {
                in_progress += 1;
            }
        }
        match statuses
            .iter_mut()
            .find(|slice| slice.kind == kind && slice.name == issue.state.name)
        {
            Some(slice) => slice.count += 1,
            None => statuses.push(StatusSlice {
                name: issue.state.name.clone(),
                kind,
                color: issue.state.color(),
                count: 1,
            }),
        }
    }

    // In the order work moves through them, as Linear lists them.
    statuses.sort_by(|a, b| {
        let order = |kind: StateType| StateType::ALL.iter().position(|each| *each == kind);
        order(a.kind)
            .cmp(&order(b.kind))
            .then(b.count.cmp(&a.count))
    });

    Summary {
        buckets,
        statuses,
        completed_this_week,
        completed_this_month,
        open,
        in_progress,
        median_cycle_days: median(cycle_days),
        cycle: (cycle_total > 0).then_some((cycle_done, cycle_total)),
    }
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some((values.get(middle - 1)? + values.get(middle)?) / 2.)
    } else {
        values.get(middle).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CycleActivity, WorkflowState};
    use time::macros::date;

    fn issue(
        kind: StateType,
        name: &str,
        started: Option<&str>,
        completed: Option<&str>,
        in_active_cycle: bool,
    ) -> DashboardIssue {
        DashboardIssue {
            id: name.into(),
            identifier: "RB-1".into(),
            state: WorkflowState {
                id: name.into(),
                name: name.into(),
                color: "#5e6ad2".into(),
                kind,
            },
            created_at: "2026-01-01T00:00:00Z".into(),
            started_at: started.map(Into::into),
            completed_at: completed.map(Into::into),
            canceled_at: None,
            cycle: Some(CycleActivity {
                is_active: in_active_cycle,
            }),
        }
    }

    /// Opening the dashboard draws it, filter button and all. That button
    /// once read the dashboard's filters through the dashboard itself while
    /// the dashboard was drawing, which is a double lease and a crash.
    #[gpui::test]
    async fn the_dashboard_draws(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let http_client = http_client::FakeHttpClient::create(|_| async {
            let mut issue = crate::tests::issue_json("RB-1");
            if let Some(fields) = issue.as_object_mut() {
                fields.insert("createdAt".into(), "2026-09-01T00:00:00Z".into());
                fields.insert("startedAt".into(), serde_json::Value::Null);
                fields.insert("completedAt".into(), serde_json::Value::Null);
                fields.insert("canceledAt".into(), serde_json::Value::Null);
                fields.insert("cycle".into(), serde_json::json!({ "isActive": true }));
            }
            let body = serde_json::json!({ "data": { "issues": {
                "nodes": [issue],
                "pageInfo": { "hasNextPage": false, "endCursor": null },
            } } });
            Ok(http_client::Response::builder()
                .status(200)
                .body(http_client::AsyncBody::from(body.to_string()))?)
        });
        let linear = crate::tests::connected_linear(http_client, cx);
        let (view, cx) =
            cx.add_window_view(|window, cx| DashboardView::new(linear.clone(), window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(
                matches!(view.loaded, Loaded::Issues { .. }),
                "the issues were fetched"
            );
        });
        // Opening the filter menu and changing a filter through it, which
        // reads and writes the dashboard from outside its own update, and
        // then draws it with the button lit.
        let target = view.update(cx, |view, cx| view.filter_target(cx));
        cx.update(|window, cx| {
            crate::linear_panel::filters_menu(target.clone(), linear.clone(), window, cx);
            target.change(cx, |filters| {
                filters.assignee = crate::AssigneeFilter::Anyone
            });
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.filters.assignee, crate::AssigneeFilter::Anyone);
            assert!(matches!(view.loaded, Loaded::Issues { .. }));
        });
    }

    #[test]
    fn weeks_start_on_monday_and_end_with_this_one() {
        // A Saturday.
        let starts = bucket_starts(Period::Weeks, date!(2026 - 09 - 26));
        assert_eq!(starts.len(), BUCKETS);
        assert_eq!(starts.last(), Some(&date!(2026 - 09 - 21)));
        assert_eq!(starts.first(), Some(&date!(2026 - 07 - 06)));
    }

    #[test]
    fn months_cross_into_the_previous_year() {
        let starts = bucket_starts(Period::Months, date!(2026 - 03 - 15));
        assert_eq!(starts.first(), Some(&date!(2025 - 04 - 01)));
        assert_eq!(starts.last(), Some(&date!(2026 - 03 - 01)));
        assert_eq!(bucket_label(Period::Months, date!(2026 - 01 - 01)).as_ref(), "Jan 26");
        assert_eq!(bucket_label(Period::Weeks, date!(2026 - 09 - 21)).as_ref(), "Sep 21");
    }

    #[test]
    fn counts_what_was_done_and_where_the_rest_stands() {
        let issues = vec![
            // Started Monday, done Wednesday of this week: 2 days.
            issue(
                StateType::Completed,
                "Done",
                Some("2026-09-21T12:00:00Z"),
                Some("2026-09-23T12:00:00Z"),
                true,
            ),
            // Done earlier this month, 4 days.
            issue(
                StateType::Completed,
                "Done",
                Some("2026-09-01T12:00:00Z"),
                Some("2026-09-05T12:00:00Z"),
                false,
            ),
            issue(StateType::Started, "In Progress", Some("2026-09-24T12:00:00Z"), None, true),
            issue(StateType::Unstarted, "Todo", None, None, true),
            issue(StateType::Backlog, "Backlog", None, None, false),
        ];
        let summary = summarize(
            &issues,
            &[],
            Period::Weeks,
            date!(2026 - 09 - 26),
            UtcOffset::UTC,
        );

        assert_eq!(summary.completed_this_week, 1);
        assert_eq!(summary.completed_this_month, 2);
        assert_eq!(summary.open, 3);
        assert_eq!(summary.in_progress, 1);
        assert_eq!(summary.median_cycle_days, Some(3.));
        assert_eq!(summary.cycle, Some((1, 3)));
        assert_eq!(summary.buckets.last().map(|bucket| bucket.completed), Some(1));
        assert_eq!(summary.buckets.iter().map(|bucket| bucket.completed).sum::<usize>(), 2);
        let names: Vec<(&str, usize)> = summary
            .statuses
            .iter()
            .map(|slice| (slice.name.as_ref(), slice.count))
            .collect();
        assert_eq!(
            names,
            vec![("Backlog", 1), ("Todo", 1), ("In Progress", 1), ("Done", 2)]
        );
    }

    /// The status filter narrows where things stand, but what was completed is
    /// counted whatever it says — otherwise leaving "Done" out would empty the
    /// chart of completed issues.
    #[test]
    fn the_status_filter_narrows_only_where_things_stand() {
        let issues = vec![
            issue(
                StateType::Completed,
                "Done",
                None,
                Some("2026-09-23T12:00:00Z"),
                false,
            ),
            issue(StateType::Started, "In Progress", None, None, false),
        ];
        let summary = summarize(
            &issues,
            &[StateType::Started],
            Period::Weeks,
            date!(2026 - 09 - 26),
            UtcOffset::UTC,
        );
        assert_eq!(summary.completed_this_week, 1);
        assert_eq!(summary.statuses.len(), 1);
        assert_eq!(summary.open, 1);
    }

    #[test]
    fn the_median_of_an_even_count_is_the_middle_pair() {
        assert_eq!(median(vec![4., 1., 3., 2.]), Some(2.5));
        assert_eq!(median(Vec::new()), None);
    }
}
