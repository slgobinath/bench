//! Linear: its issues, in a panel and in tabs, and linked to worktrees.
//!
//! One [`Linear`] entity for the whole application holds the API key, what the
//! panel is filtering by and what it found, and the issues worktree branches
//! are named after. It is a global rather than per panel for the same reason
//! the worktree panel's scans are: Bench has a workspace — and so a panel —
//! per worktree, and which issues you are looking at is not a fact about the
//! worktree you happen to be in.
//!
//! Signing in is a personal API key, kept in the system keychain. Linear has
//! no CLI that is already signed in the way `gh` is, and OAuth would need an
//! application registered with Linear before anyone could use it.
//! `LINEAR_API_KEY` wins over the keychain, for anyone who already exports it.

mod dashboard;
mod issue_view;
mod linear_panel;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use credentials_provider::CredentialsProvider;
use futures::AsyncReadExt as _;
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, Global, Hsla, Rgba, SharedString,
    Task, TaskExt as _,
};
use http_client::{AsyncBody, HttpClient, Method, Request, StatusCode};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use ui::IconName;

pub use dashboard::{DashboardView, OpenDashboard, open_dashboard};
pub use issue_view::{IssueView, open_issue};
pub use linear_panel::LinearPanel;

/// Creates a worktree for a Linear issue, named after the branch Linear
/// suggests for it.
///
/// Defined here and handled by the worktree panel, which is the crate that
/// knows how to make a worktree: this one only knows which issue it is for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Action)]
#[action(namespace = linear)]
#[serde(deny_unknown_fields)]
pub struct CreateWorktree {
    /// The issue's identifier, such as `ENG-123`.
    pub identifier: String,
}

const API_URL: &str = "https://api.linear.app/graphql";
/// What the key is stored under in the keychain.
const CREDENTIALS_URL: &str = "https://api.linear.app";
const API_KEY_ENV_VAR: &str = "LINEAR_API_KEY";
/// Where a personal API key is made.
pub const API_KEY_SETTINGS_URL: &str = "https://linear.app/settings/account/security";

/// How many issues the panel shows. Linear orders them by when they last
/// changed, so the ones past this are the ones you are least likely to want.
const LIST_LIMIT: usize = 100;
const SEARCH_LIMIT: usize = 50;
/// How long typing has to pause before the panel searches. Every keystroke
/// would otherwise be a request.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
/// How long what Linear said about a branch's issue is trusted. An issue
/// moves through its states while Bench is open, and the worktree row showing
/// it should follow.
const LOOKUP_REFRESH: Duration = Duration::from_secs(300);
/// How soon a lookup that could not reach Linear is tried again. A request
/// that failed says nothing about the issues — the network was down, or Bench
/// launched before it was up — so it is retried soon, and on its own rather
/// than waiting for the next draw.
const LOOKUP_RETRY: Duration = Duration::from_secs(15);
/// How often what Bench shows of Linear is asked about again. Statuses move in
/// Linear, not here, so they are only as fresh as the last time Bench asked.
const POLL_EVERY: Duration = Duration::from_secs(60);

pub fn init(cx: &mut App) {
    let linear = cx.new(Linear::new);
    cx.set_global(GlobalLinear(linear));
    linear_panel::init(cx);
    dashboard::init(cx);
}

struct GlobalLinear(Entity<Linear>);

impl Global for GlobalLinear {}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub id: SharedString,
    pub identifier: SharedString,
    pub title: SharedString,
    pub url: SharedString,
    /// The branch Linear suggests for the issue — `name/eng-123-fix-login` by
    /// default. Naming a branch this way is also what Linear's own GitHub
    /// integration recognises, so pull requests from it are linked back.
    pub branch_name: SharedString,
    /// 0 is no priority, then 1 (urgent) to 4 (low).
    pub priority: f64,
    pub priority_label: SharedString,
    pub state: WorkflowState,
    pub assignee: Option<User>,
    pub team: Team,
    pub project: Option<Named>,
    pub cycle: Option<Cycle>,
    #[serde(deserialize_with = "nodes")]
    pub labels: Vec<IssueLabel>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct WorkflowState {
    pub id: SharedString,
    pub name: SharedString,
    pub color: SharedString,
    #[serde(rename = "type")]
    pub kind: StateType,
}

/// The fixed set of groups every team's own workflow states belong to. A
/// team's states are named whatever the team likes; these are what filters
/// and "move to In Progress" can rely on.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum StateType {
    Triage,
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
    #[serde(other)]
    Other,
}

impl StateType {
    pub const ALL: [StateType; 6] = [
        StateType::Triage,
        StateType::Backlog,
        StateType::Unstarted,
        StateType::Started,
        StateType::Completed,
        StateType::Canceled,
    ];

    fn as_str(self) -> &'static str {
        match self {
            StateType::Triage => "triage",
            StateType::Backlog => "backlog",
            StateType::Unstarted => "unstarted",
            StateType::Started => "started",
            StateType::Completed => "completed",
            StateType::Canceled => "canceled",
            StateType::Other => "other",
        }
    }

    /// What Linear's own UI calls the group, which is what a filter offers.
    pub fn label(self) -> &'static str {
        match self {
            StateType::Triage => "Triage",
            StateType::Backlog => "Backlog",
            StateType::Unstarted => "Todo",
            StateType::Started => "In Progress",
            StateType::Completed => "Done",
            StateType::Canceled => "Canceled",
            StateType::Other => "Other",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            StateType::Unstarted => IconName::TodoPending,
            StateType::Started => IconName::TodoProgress,
            StateType::Completed => IconName::TodoComplete,
            StateType::Canceled => IconName::XCircle,
            StateType::Triage | StateType::Backlog | StateType::Other => IconName::Circle,
        }
    }

    /// Whether an issue in this group is one work on has not begun, which is
    /// the only kind that making a worktree for moves along. An issue already
    /// in progress, in review, or done stays where it is.
    fn is_not_started(self) -> bool {
        matches!(
            self,
            StateType::Triage | StateType::Backlog | StateType::Unstarted
        )
    }
}

impl WorkflowState {
    /// The team's own colour for the state, which is what Linear draws it in.
    pub fn color(&self) -> Option<Hsla> {
        hex_color(&self.color)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: SharedString,
    pub name: SharedString,
    pub display_name: SharedString,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Team {
    pub id: SharedString,
    pub key: SharedString,
    pub name: SharedString,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Named {
    pub id: SharedString,
    pub name: SharedString,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Cycle {
    pub id: SharedString,
    pub number: f64,
    pub name: Option<SharedString>,
    /// RFC 3339.
    #[serde(default)]
    pub starts_at: Option<SharedString>,
    #[serde(default)]
    pub is_active: bool,
}

impl Cycle {
    pub fn label(&self) -> SharedString {
        match &self.name {
            Some(name) if !name.is_empty() => name.clone(),
            _ => format!("Cycle {}", self.number).into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct IssueLabel {
    pub id: SharedString,
    pub name: SharedString,
    pub color: SharedString,
}

/// An issue with what only its own tab shows: the description and the
/// conversation under it.
#[derive(Clone, Debug, Deserialize)]
pub struct IssueDetail {
    #[serde(flatten)]
    pub issue: Issue,
    pub description: Option<String>,
    #[serde(deserialize_with = "nodes")]
    pub comments: Vec<Comment>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: SharedString,
    pub body: String,
    pub created_at: String,
    user: Option<User>,
    bot_actor: Option<NameOnly>,
    external_user: Option<NameOnly>,
}

#[derive(Clone, Debug, Deserialize)]
struct NameOnly {
    name: Option<SharedString>,
}

impl Comment {
    /// Who wrote it. A comment can come from a person, an integration, or
    /// someone outside the workspace replying by email or Slack.
    pub fn author(&self) -> SharedString {
        self.user
            .as_ref()
            .map(|user| user.display_name.clone())
            .or_else(|| self.bot_actor.as_ref().and_then(|bot| bot.name.clone()))
            .or_else(|| {
                self.external_user
                    .as_ref()
                    .and_then(|user| user.name.clone())
            })
            .unwrap_or_else(|| "Someone".into())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AssigneeFilter {
    #[default]
    Me,
    Unassigned,
    Anyone,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CycleFilter {
    #[default]
    Any,
    Current,
    None,
}

/// What the panel is showing issues for.
#[derive(Clone, Debug, PartialEq)]
pub struct IssueFilters {
    pub assignee: AssigneeFilter,
    /// Empty is every state.
    pub states: Vec<StateType>,
    pub project: Option<Named>,
    pub cycle: CycleFilter,
    /// Issues with any of these. Empty is no label filter at all.
    pub labels: Vec<Named>,
}

impl Default for IssueFilters {
    /// Your own issues that are not finished: what there is to pick up.
    fn default() -> Self {
        Self {
            assignee: AssigneeFilter::Me,
            states: vec![
                StateType::Triage,
                StateType::Backlog,
                StateType::Unstarted,
                StateType::Started,
            ],
            project: None,
            cycle: CycleFilter::Any,
            labels: Vec::new(),
        }
    }
}

impl IssueFilters {
    /// The filters as Linear's `IssueFilter` input.
    fn to_graphql(&self) -> Value {
        let mut filter = serde_json::Map::new();
        match self.assignee {
            AssigneeFilter::Me => {
                filter.insert("assignee".into(), json!({ "isMe": { "eq": true } }));
            }
            AssigneeFilter::Unassigned => {
                filter.insert("assignee".into(), json!({ "null": true }));
            }
            AssigneeFilter::Anyone => {}
        }
        if !self.states.is_empty() {
            let types: Vec<&str> = self.states.iter().map(|state| state.as_str()).collect();
            filter.insert("state".into(), json!({ "type": { "in": types } }));
        }
        if let Some(project) = &self.project {
            filter.insert(
                "project".into(),
                json!({ "id": { "eq": project.id.as_ref() } }),
            );
        }
        match self.cycle {
            CycleFilter::Any => {}
            CycleFilter::Current => {
                filter.insert("cycle".into(), json!({ "isActive": { "eq": true } }));
            }
            CycleFilter::None => {
                filter.insert("cycle".into(), json!({ "null": true }));
            }
        }
        if !self.labels.is_empty() {
            let ids: Vec<&str> = self.labels.iter().map(|label| label.id.as_ref()).collect();
            filter.insert("labels".into(), json!({ "some": { "id": { "in": ids } } }));
        }
        Value::Object(filter)
    }
}

/// An issue as the dashboard counts it: where it stands, and when it moved.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DashboardIssue {
    pub id: SharedString,
    pub identifier: SharedString,
    pub state: WorkflowState,
    /// RFC 3339, as Linear sends them.
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub canceled_at: Option<String>,
    pub cycle: Option<CycleActivity>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CycleActivity {
    pub is_active: bool,
}

/// What the dashboard fetched, and whether that was everything.
pub struct DashboardIssues {
    pub issues: Vec<DashboardIssue>,
    /// More issues matched than [`DASHBOARD_LIMIT`]; the charts count the
    /// ones fetched.
    pub truncated: bool,
}

/// The most issues a dashboard fetches. Enough for a person's or a team's
/// year; a whole workspace's history is what Linear's own insights are for.
pub const DASHBOARD_LIMIT: usize = 2000;
const DASHBOARD_PAGE: usize = 100;

/// Where the API key came from, which decides whether Bench can forget it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    Keychain,
    /// `LINEAR_API_KEY`. Bench did not store it, so it cannot remove it.
    Environment,
}

pub enum Connection {
    /// Reading the keychain.
    Loading,
    Disconnected {
        /// Why the key could not be read, when that is why there is none.
        error: Option<SharedString>,
    },
    Connected {
        key: Arc<str>,
        source: KeySource,
        /// Who the key belongs to. `None` until Linear has answered.
        viewer: Option<User>,
    },
}

/// The projects and labels the panel's filters offer.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub projects: Vec<Named>,
    pub labels: Vec<Named>,
}

struct Lookup {
    issue: Option<Arc<Issue>>,
    /// Until when this is trusted; asked again after it on the next draw.
    fresh_until: Instant,
}

/// How the panel groups the issues it lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IssueGrouping {
    #[default]
    None,
    Status,
    Cycle,
}

pub enum LinearEvent {
    /// Something the panel, a tab, or a worktree row draws has changed.
    Changed,
}

pub struct Linear {
    http_client: Arc<dyn HttpClient>,
    credentials: Arc<dyn CredentialsProvider>,
    connection: Connection,
    catalog: Catalog,
    filters: IssueFilters,
    grouping: IssueGrouping,
    /// The groups closed in the panel, by [`IssueGrouping`] and group key.
    /// Here rather than on a panel, like the filters, so that every
    /// worktree's panel shows the same list.
    collapsed_groups: HashSet<(IssueGrouping, SharedString)>,
    query: String,
    issues: Vec<Arc<Issue>>,
    /// Whether a list request is in flight. The previous list stays on screen
    /// until it lands, rather than the panel emptying on every keystroke.
    loading: bool,
    list_error: Option<SharedString>,
    /// Dropped — and so cancelled — by the next request, which is what makes
    /// the search debounce work and what keeps a slow, stale answer from
    /// landing on top of a newer one.
    _list: Option<Task<()>>,
    _catalog: Option<Task<()>>,
    _load_key: Option<Task<()>>,
    /// What each branch-named identifier turned out to be; see
    /// [`Self::look_up_branches`].
    lookups: HashMap<SharedString, Lookup>,
    queued_lookups: HashSet<SharedString>,
    /// Every key a panel has asked about, which [`POLL_EVERY`] asks about
    /// again: an issue's status changes in Linear without anything here
    /// knowing to look.
    watched: HashSet<SharedString>,
    _poll: Task<()>,
    looking_up: bool,
}

impl EventEmitter<LinearEvent> for Linear {}

impl Linear {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalLinear>()
            .map(|linear| linear.0.clone())
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            http_client: cx.http_client(),
            credentials: zed_credentials_provider::global(cx),
            connection: Connection::Loading,
            catalog: Catalog::default(),
            filters: IssueFilters::default(),
            grouping: IssueGrouping::default(),
            collapsed_groups: HashSet::new(),
            query: String::new(),
            issues: Vec::new(),
            loading: false,
            list_error: None,
            _list: None,
            _catalog: None,
            _load_key: None,
            lookups: HashMap::new(),
            queued_lookups: HashSet::new(),
            watched: HashSet::new(),
            _poll: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(POLL_EVERY).await;
                    if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                        return;
                    }
                }
            }),
            looking_up: false,
        };
        this.load_key(cx);
        this
    }

    fn load_key(&mut self, cx: &mut Context<Self>) {
        if let Ok(key) = std::env::var(API_KEY_ENV_VAR)
            && !key.trim().is_empty()
        {
            self.set_connected(key.trim().into(), KeySource::Environment, None, cx);
            return;
        }

        let credentials = self.credentials.clone();
        self._load_key = Some(cx.spawn(async move |this, cx| {
            let stored = credentials.read_credentials(CREDENTIALS_URL, cx).await;
            this.update(cx, |this, cx| match stored {
                Ok(Some((_, key))) => match String::from_utf8(key) {
                    Ok(key) => this.set_connected(key.into(), KeySource::Keychain, None, cx),
                    Err(_) => this.set_disconnected(
                        Some("The Linear API key in the keychain is not valid text.".into()),
                        cx,
                    ),
                },
                Ok(None) => this.set_disconnected(None, cx),
                Err(error) => {
                    log::error!("reading the Linear API key from the keychain: {error:#}");
                    this.set_disconnected(
                        Some(format!("Could not read the keychain: {error}").into()),
                        cx,
                    );
                }
            })
            .ok();
        }));
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.connection, Connection::Connected { .. })
    }

    fn key(&self) -> Option<Arc<str>> {
        match &self.connection {
            Connection::Connected { key, .. } => Some(key.clone()),
            Connection::Loading | Connection::Disconnected { .. } => None,
        }
    }

    fn viewer(&self) -> Option<&User> {
        match &self.connection {
            Connection::Connected { viewer, .. } => viewer.as_ref(),
            Connection::Loading | Connection::Disconnected { .. } => None,
        }
    }

    /// Checks the key with Linear before keeping it, so that a mistyped key is
    /// refused where it was typed rather than showing up later as a panel that
    /// cannot load.
    pub fn connect(&mut self, key: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let key: Arc<str> = key.trim().into();
        if key.is_empty() {
            return Task::ready(Err(anyhow!("Paste a Linear API key first.")));
        }
        let http_client = self.http_client.clone();
        let credentials = self.credentials.clone();
        cx.spawn(async move |this, cx| {
            let viewer = fetch_viewer(http_client.as_ref(), &key).await?;
            credentials
                .write_credentials(CREDENTIALS_URL, "Bearer", key.as_bytes(), cx)
                .await
                .context("storing the API key in the keychain")?;
            this.update(cx, |this, cx| {
                this.set_connected(key, KeySource::Keychain, Some(viewer), cx);
            })
        })
    }

    pub fn disconnect(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        if matches!(
            self.connection,
            Connection::Connected {
                source: KeySource::Environment,
                ..
            }
        ) {
            return Task::ready(Err(anyhow!(
                "The key comes from {API_KEY_ENV_VAR}. Unset it to disconnect."
            )));
        }
        let credentials = self.credentials.clone();
        self.set_disconnected(None, cx);
        cx.spawn(async move |_, cx| {
            credentials
                .delete_credentials(CREDENTIALS_URL, cx)
                .await
                .context("removing the API key from the keychain")
        })
    }

    fn set_connected(
        &mut self,
        key: Arc<str>,
        source: KeySource,
        viewer: Option<User>,
        cx: &mut Context<Self>,
    ) {
        let needs_viewer = viewer.is_none();
        self.connection = Connection::Connected {
            key: key.clone(),
            source,
            viewer,
        };
        self.forget_issues();
        self.refresh(cx);
        self.load_catalog(cx);
        if needs_viewer {
            let http_client = self.http_client.clone();
            cx.spawn(async move |this, cx| {
                let viewer = fetch_viewer(http_client.as_ref(), &key).await;
                this.update(cx, |this, cx| match viewer {
                    Ok(viewer) => {
                        if let Connection::Connected {
                            key: current,
                            viewer: slot,
                            ..
                        } = &mut this.connection
                            && *current == key
                        {
                            *slot = Some(viewer);
                            cx.emit(LinearEvent::Changed);
                            cx.notify();
                        }
                    }
                    Err(error) => log::warn!("asking Linear who the API key is for: {error:#}"),
                })
            })
            .detach_and_log_err(cx);
        }
        cx.emit(LinearEvent::Changed);
        cx.notify();
    }

    fn set_disconnected(&mut self, error: Option<SharedString>, cx: &mut Context<Self>) {
        self.connection = Connection::Disconnected { error };
        self.forget_issues();
        self.catalog = Catalog::default();
        cx.emit(LinearEvent::Changed);
        cx.notify();
    }

    /// Drops everything fetched with the previous key: it may have been a
    /// different workspace's.
    fn forget_issues(&mut self) {
        self.issues.clear();
        self.list_error = None;
        self.loading = false;
        self._list = None;
        self.lookups.clear();
        self.queued_lookups.clear();
        self.watched.clear();
    }

    pub fn filters(&self) -> &IssueFilters {
        &self.filters
    }

    pub fn grouping(&self) -> IssueGrouping {
        self.grouping
    }

    pub fn set_grouping(&mut self, grouping: IssueGrouping, cx: &mut Context<Self>) {
        if grouping != self.grouping {
            self.grouping = grouping;
            cx.emit(LinearEvent::Changed);
            cx.notify();
        }
    }

    pub fn is_group_collapsed(&self, key: &SharedString) -> bool {
        self.collapsed_groups
            .contains(&(self.grouping, key.clone()))
    }

    pub fn toggle_group(&mut self, key: SharedString, cx: &mut Context<Self>) {
        let group = (self.grouping, key);
        if !self.collapsed_groups.remove(&group) {
            self.collapsed_groups.insert(group);
        }
        cx.emit(LinearEvent::Changed);
        cx.notify();
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn issues(&self) -> &[Arc<Issue>] {
        &self.issues
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn list_error(&self) -> Option<&SharedString> {
        self.list_error.as_ref()
    }

    pub fn update_filters(
        &mut self,
        update: impl FnOnce(&mut IssueFilters),
        cx: &mut Context<Self>,
    ) {
        let before = self.filters.clone();
        update(&mut self.filters);
        if self.filters != before {
            self.refresh(cx);
        }
    }

    pub fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        let query = query.trim();
        if query == self.query {
            return;
        }
        self.query = query.to_owned();
        self.refresh(cx);
    }

    /// Asks Linear again for what the panel shows.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.key() else {
            return;
        };
        let http_client = self.http_client.clone();
        let filter = self.filters.to_graphql();
        let query = self.query.clone();
        self.loading = true;
        self._list = Some(cx.spawn(async move |this, cx| {
            if !query.is_empty() {
                cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            }
            let found = if query.is_empty() {
                list_issues(http_client.as_ref(), &key, filter).await
            } else {
                search_issues(http_client.as_ref(), &key, &query, filter, SEARCH_LIMIT).await
            };
            this.update(cx, |this, cx| {
                this.loading = false;
                match found {
                    Ok(issues) => {
                        this.list_error = None;
                        this.issues = issues.into_iter().map(Arc::new).collect();
                        for issue in this.issues.clone() {
                            this.remember(issue);
                        }
                    }
                    Err(error) => {
                        log::warn!("listing Linear issues: {error:#}");
                        this.list_error = Some(format!("{error:#}").into());
                    }
                }
                cx.emit(LinearEvent::Changed);
                cx.notify();
            })
            .ok();
        }));
        cx.emit(LinearEvent::Changed);
        cx.notify();
    }

    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.key() else {
            return;
        };
        let http_client = self.http_client.clone();
        self._catalog = Some(cx.spawn(async move |this, cx| {
            let catalog = fetch_catalog(http_client.as_ref(), &key).await;
            this.update(cx, |this, cx| match catalog {
                Ok(catalog) => {
                    this.catalog = catalog;
                    cx.emit(LinearEvent::Changed);
                    cx.notify();
                }
                Err(error) => log::warn!("listing Linear projects and labels: {error:#}"),
            })
            .ok();
        }));
    }

    /// Searches for issues to link a new worktree to, apart from what the
    /// panel is showing. An empty search is your own unfinished issues, which
    /// is what a worktree is most likely to be made for.
    pub fn search(&self, query: &str, cx: &App) -> Task<Result<Vec<Arc<Issue>>>> {
        let Some(key) = self.key() else {
            return Task::ready(Err(anyhow!("Linear is not connected.")));
        };
        let http_client = self.http_client.clone();
        let query = query.trim().to_owned();
        let filter = if query.is_empty() {
            IssueFilters::default().to_graphql()
        } else {
            json!({})
        };
        cx.background_spawn(async move {
            let issues = if query.is_empty() {
                list_issues(http_client.as_ref(), &key, filter).await?
            } else {
                search_issues(http_client.as_ref(), &key, &query, filter, 20).await?
            };
            Ok(issues.into_iter().map(Arc::new).collect())
        })
    }

    /// The issues a dashboard counts: those matching `filters` that are still
    /// open, or that were completed or canceled since `since` (RFC 3339).
    ///
    /// The status filter is left out of the request. The dashboard applies it
    /// to what it shows as the current state of things, but counting what was
    /// completed needs the completed issues whatever the filter says.
    pub fn dashboard_issues(
        &self,
        filters: &IssueFilters,
        since: String,
        cx: &App,
    ) -> Task<Result<DashboardIssues>> {
        let Some(key) = self.key() else {
            return Task::ready(Err(anyhow!("Linear is not connected.")));
        };
        let http_client = self.http_client.clone();
        let filter = dashboard_filter(filters, &since);
        cx.background_spawn(async move {
            #[derive(Deserialize)]
            struct Response {
                issues: Page,
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Page {
                nodes: Vec<DashboardIssue>,
                page_info: PageInfo,
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct PageInfo {
                has_next_page: bool,
                end_cursor: Option<String>,
            }

            let mut issues = Vec::new();
            let mut after: Option<String> = None;
            loop {
                let page: Response = graphql(
                    http_client.as_ref(),
                    &key,
                    "query($filter: IssueFilter, $first: Int, $after: String) { \
                     issues(filter: $filter, first: $first, after: $after) { \
                     nodes { id identifier createdAt startedAt completedAt canceledAt \
                     state { id name color type } cycle { isActive } } \
                     pageInfo { hasNextPage endCursor } } }",
                    json!({ "filter": filter, "first": DASHBOARD_PAGE, "after": after }),
                )
                .await?;
                issues.extend(page.issues.nodes);
                let more = page.issues.page_info.has_next_page;
                if !more || issues.len() >= DASHBOARD_LIMIT {
                    issues.truncate(DASHBOARD_LIMIT);
                    return Ok(DashboardIssues {
                        issues,
                        truncated: more,
                    });
                }
                after = page.issues.page_info.end_cursor;
                if after.is_none() {
                    return Ok(DashboardIssues {
                        issues,
                        truncated: false,
                    });
                }
            }
        })
    }

    /// One issue, by its identifier, from the cache or from Linear.
    pub fn issue(&self, identifier: &str, cx: &App) -> Task<Result<Arc<Issue>>> {
        if let Some(issue) = self
            .lookups
            .get(identifier)
            .and_then(|lookup| lookup.issue.clone())
        {
            return Task::ready(Ok(issue));
        }
        let Some(key) = self.key() else {
            return Task::ready(Err(anyhow!("Linear is not connected.")));
        };
        let http_client = self.http_client.clone();
        let identifier = identifier.to_owned();
        cx.background_spawn(async move {
            let found: IssueResponse<Issue> = graphql(
                http_client.as_ref(),
                &key,
                &format!("query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} }} }}"),
                json!({ "id": identifier }),
            )
            .await?;
            Ok(Arc::new(found.issue))
        })
    }

    /// Everything the issue tab shows, always asked of Linear: the tab is
    /// where you go to see the issue as it is now.
    pub fn issue_detail(&self, identifier: &str, cx: &App) -> Task<Result<IssueDetail>> {
        let Some(key) = self.key() else {
            return Task::ready(Err(anyhow!("Linear is not connected.")));
        };
        let http_client = self.http_client.clone();
        let identifier = identifier.to_owned();
        cx.background_spawn(async move {
            let found: IssueResponse<IssueDetail> = graphql(
                http_client.as_ref(),
                &key,
                &format!(
                    "query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} description \
                     comments(first: 100) {{ nodes {{ id body createdAt \
                     user {{ id name displayName }} botActor {{ name }} externalUser {{ name }} }} }} }} }}"
                ),
                json!({ "id": identifier }),
            )
            .await?;
            let mut detail = found.issue;
            // RFC 3339 in UTC, so the strings sort as the times do.
            detail
                .comments
                .sort_by(|a, b| a.created_at.cmp(&b.created_at));
            Ok(detail)
        })
    }

    /// Asks again about every issue a panel is showing, and refreshes the
    /// panel's list, so a status changed in Linear shows up here.
    fn poll(&mut self, cx: &mut Context<Self>) {
        if !self.is_connected() {
            return;
        }
        self.queued_lookups.extend(self.watched.iter().cloned());
        self.start_lookups(cx);
        if !self.loading {
            self.refresh(cx);
        }
    }

    /// The issue with Linear's own `id`, once it has been looked up; see
    /// [`Self::look_up_ids`].
    pub fn issue_for_id(&self, id: &str) -> Option<Arc<Issue>> {
        self.lookups.get(id)?.issue.clone()
    }

    /// Asks Linear about the issues with these ids — the ones worktrees were
    /// made for — including archived ones, alongside the branch lookups. An
    /// id finds its issue whatever has become of its identifier.
    pub fn look_up_ids<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>, cx: &mut Context<Self>) {
        if !self.is_connected() {
            return;
        }
        for id in ids {
            self.watched.insert(SharedString::from(id.to_owned()));
            let fresh = self
                .lookups
                .get(id)
                .is_some_and(|lookup| Instant::now() < lookup.fresh_until);
            if !fresh {
                self.queued_lookups.insert(SharedString::from(id.to_owned()));
            }
        }
        self.start_lookups(cx);
    }

    /// The issue a branch is named after, when Linear has already said which
    /// that is. See [`Self::look_up_branches`] for how it comes to know.
    pub fn issue_for_branch(&self, branch: &str) -> Option<Arc<Issue>> {
        let identifier = identifier_in_branch(branch)?;
        self.lookups.get(&identifier)?.issue.clone()
    }

    /// Asks Linear about the issues these branches are named after, in one
    /// request, for any it has not been asked about recently.
    ///
    /// The link between a worktree and its issue is the branch name itself —
    /// `eng-123` in `name/eng-123-fix-login` — which is also how Linear links a
    /// branch to an issue. Nothing is stored, so a worktree made outside Bench
    /// from Linear's "copy branch name" is linked just the same.
    ///
    /// Cheap to call on every draw: identifiers already known or already asked
    /// about are skipped.
    pub fn look_up_branches<'a>(
        &mut self,
        branches: impl IntoIterator<Item = &'a str>,
        cx: &mut Context<Self>,
    ) {
        if !self.is_connected() {
            return;
        }
        for branch in branches {
            let Some(identifier) = identifier_in_branch(branch) else {
                continue;
            };
            self.watched.insert(identifier.clone());
            let fresh = self
                .lookups
                .get(&identifier)
                .is_some_and(|lookup| Instant::now() < lookup.fresh_until);
            if !fresh {
                self.queued_lookups.insert(identifier);
            }
        }
        self.start_lookups(cx);
    }

    /// Asks Linear about the queued identifiers, unless that is already under
    /// way — the running request picks up whatever is queued behind it.
    fn start_lookups(&mut self, cx: &mut Context<Self>) {
        if self.queued_lookups.is_empty() || self.looking_up {
            return;
        }
        let Some(key) = self.key() else {
            return;
        };

        self.looking_up = true;
        let http_client = self.http_client.clone();
        // Detached rather than held: it ends by clearing `looking_up`, and a
        // task held in a field cannot drop itself.
        cx.spawn(async move |this, cx| {
            loop {
                let batch: Vec<SharedString> = this.update(cx, |this, _| {
                    this.queued_lookups.drain().collect()
                })?;
                if batch.is_empty() {
                    break;
                }
                let found = issues_by_key(http_client.as_ref(), &key, &batch).await;
                this.update(cx, |this, cx| match found {
                    Ok(found) => {
                        log::info!(
                            "looked up Linear issues {batch:?}, found {:?}",
                            found
                                .iter()
                                .map(|issue| issue.identifier.as_ref())
                                .collect::<Vec<_>>()
                        );
                        // Linear answered, so an identifier it did not return
                        // is not an issue — a branch that only looks like one.
                        let fresh_until = Instant::now() + LOOKUP_REFRESH;
                        for identifier in &batch {
                            this.lookups.insert(
                                identifier.clone(),
                                Lookup {
                                    issue: None,
                                    fresh_until,
                                },
                            );
                        }
                        for issue in found {
                            this.remember(Arc::new(issue));
                        }
                        cx.emit(LinearEvent::Changed);
                        cx.notify();
                    }
                    Err(error) => {
                        log::warn!("looking up Linear issues {batch:?}: {error:#}");
                        this.retry_lookups(batch, cx);
                    }
                })?;
            }
            this.update(cx, |this, _| this.looking_up = false)
        })
        .detach_and_log_err(cx);
    }

    /// After a request that did not reach Linear: what was known about these
    /// issues stays on screen — it is out of date at worst, where dropping it
    /// would say the worktree has no issue — and they are asked about again
    /// shortly.
    fn retry_lookups(&mut self, identifiers: Vec<SharedString>, cx: &mut Context<Self>) {
        let fresh_until = Instant::now() + LOOKUP_RETRY;
        for identifier in &identifiers {
            self.lookups
                .entry(identifier.clone())
                .and_modify(|lookup| lookup.fresh_until = fresh_until)
                .or_insert(Lookup {
                    issue: None,
                    fresh_until,
                });
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOOKUP_RETRY).await;
            this.update(cx, |this, cx| {
                this.queued_lookups.extend(identifiers);
                this.start_lookups(cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn remember(&mut self, issue: Arc<Issue>) {
        let fresh_until = Instant::now() + LOOKUP_REFRESH;
        // By id as well as identifier: a worktree Bench made remembers the
        // issue's id, which outlives its identifier.
        self.lookups.insert(
            issue.id.clone(),
            Lookup {
                issue: Some(issue.clone()),
                fresh_until,
            },
        );
        self.lookups.insert(
            issue.identifier.clone(),
            Lookup {
                issue: Some(issue),
                fresh_until,
            },
        );
    }

    /// What making a worktree for an issue says about it: work has begun, and
    /// you are the one doing it.
    ///
    /// The issue moves to its team's first "started" state only if it has not
    /// started yet — an issue already in review is not sent back — and it is
    /// assigned to you only if nobody has it.
    pub fn start_issue(&mut self, issue: Arc<Issue>, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(key) = self.key() else {
            return Task::ready(Err(anyhow!("Linear is not connected.")));
        };
        let http_client = self.http_client.clone();
        let viewer_id = self.viewer().map(|viewer| viewer.id.clone());
        cx.spawn(async move |this, cx| {
            let mut input = serde_json::Map::new();
            if issue.assignee.is_none() {
                let viewer_id = match viewer_id {
                    Some(id) => id,
                    None => fetch_viewer(http_client.as_ref(), &key).await?.id,
                };
                input.insert("assigneeId".into(), json!(viewer_id.as_ref()));
            }
            if issue.state.kind.is_not_started()
                && let Some(state) =
                    first_started_state(http_client.as_ref(), &key, &issue.id).await?
            {
                input.insert("stateId".into(), json!(state.as_ref()));
            }
            if input.is_empty() {
                return Ok(());
            }

            let updated: IssueUpdateResponse = graphql(
                http_client.as_ref(),
                &key,
                &format!(
                    "mutation($id: String!, $input: IssueUpdateInput!) {{ \
                     issueUpdate(id: $id, input: $input) {{ success issue {{ {ISSUE_FIELDS} }} }} }}"
                ),
                json!({ "id": issue.id.as_ref(), "input": input }),
            )
            .await?;
            if !updated.issue_update.success {
                return Err(anyhow!("Linear did not update {}.", issue.identifier));
            }
            this.update(cx, |this, cx| {
                if let Some(updated) = updated.issue_update.issue {
                    let updated = Arc::new(updated);
                    for listed in &mut this.issues {
                        if listed.id == updated.id {
                            *listed = updated.clone();
                        }
                    }
                    this.remember(updated);
                }
                cx.emit(LinearEvent::Changed);
                cx.notify();
            })
        })
    }
}

/// The issue identifier a branch is named after, such as `ENG-123` for
/// `name/eng-123-fix-login` or `ENG-123-fix-login`.
///
/// The identifier is the start of the branch's last path component: Linear
/// puts it there whether or not the branch has a user prefix. A branch that
/// merely looks like one — `release-2024` — is looked up and not found, which
/// costs a line in a batched request and nothing on screen.
pub fn identifier_in_branch(branch: &str) -> Option<SharedString> {
    let name = branch.rsplit('/').next()?;
    let mut parts = name.splitn(3, '-');
    let team = parts.next()?;
    let number = parts.next()?;
    let team_is_key = (1..=7).contains(&team.len())
        && team.starts_with(|character: char| character.is_ascii_alphabetic())
        && team.chars().all(|character| character.is_ascii_alphanumeric());
    let number_is_number =
        !number.is_empty() && number.chars().all(|character| character.is_ascii_digit());
    if !team_is_key || !number_is_number {
        return None;
    }
    Some(format!("{}-{number}", team.to_ascii_uppercase()).into())
}

/// The dashboard's request filter: `filters` without their status, limited to
/// issues still open or finished since `since`. See
/// [`Linear::dashboard_issues`].
fn dashboard_filter(filters: &IssueFilters, since: &str) -> Value {
    let mut base = filters.clone();
    base.states.clear();
    json!({
        "and": [
            base.to_graphql(),
            {
                "or": [
                    { "completedAt": { "null": true }, "canceledAt": { "null": true } },
                    { "completedAt": { "gte": since } },
                    { "canceledAt": { "gte": since } },
                ]
            }
        ]
    })
}

/// A colour as Linear writes it, `#rrggbb`.
pub fn hex_color(hex: &str) -> Option<Hsla> {
    Rgba::try_from(hex).ok().map(Hsla::from)
}

const ISSUE_FIELDS: &str = "id identifier title url branchName priority priorityLabel \
    state { id name color type } assignee { id name displayName } team { id key name } \
    project { id name } cycle { id number name startsAt isActive } labels { nodes { id name color } }";

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

fn nodes<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Nodes::<T>::deserialize(deserializer)?.nodes)
}

#[derive(Deserialize)]
struct IssueResponse<T> {
    issue: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueUpdateResponse {
    issue_update: IssueUpdatePayload,
}

#[derive(Deserialize)]
struct IssueUpdatePayload {
    success: bool,
    issue: Option<Issue>,
}

#[derive(Deserialize)]
struct GraphqlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
struct GraphqlError {
    message: String,
    extensions: Option<GraphqlErrorExtensions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlErrorExtensions {
    user_presentable_message: Option<String>,
}

/// Sends a GraphQL request and returns Linear's answer as it came, for
/// callers that read partial answers; see [`issues_by_key`].
async fn post_graphql(
    http_client: &dyn HttpClient,
    key: &str,
    query: &str,
    variables: Value,
) -> Result<(StatusCode, String)> {
    let body = serde_json::to_string(&json!({ "query": query, "variables": variables }))?;
    // A personal API key goes in the header bare. `Bearer` is for OAuth
    // tokens, and Linear refuses a personal key that carries it.
    let request = Request::builder()
        .method(Method::POST)
        .uri(API_URL)
        .header("Content-Type", "application/json")
        .header("Authorization", key)
        .body(AsyncBody::from(body))?;
    let mut response = http_client.send(request).await?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;

    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(anyhow!(
            "Linear refused the API key. It may have been revoked."
        ));
    }
    Ok((response.status(), body))
}

async fn graphql<T: DeserializeOwned>(
    http_client: &dyn HttpClient,
    key: &str,
    query: &str,
    variables: Value,
) -> Result<T> {
    let (status, body) = post_graphql(http_client, key, query, variables).await?;
    let parsed: GraphqlResponse<T> = serde_json::from_str(&body).with_context(|| {
        format!("Linear answered {status} with something that is not GraphQL")
    })?;
    if let Some(error) = parsed.errors.into_iter().next() {
        let message = error
            .extensions
            .and_then(|extensions| extensions.user_presentable_message)
            .unwrap_or(error.message);
        return Err(anyhow!(message));
    }
    parsed
        .data
        .ok_or_else(|| anyhow!("Linear answered {status} with no data"))
}

async fn fetch_viewer(http_client: &dyn HttpClient, key: &str) -> Result<User> {
    #[derive(Deserialize)]
    struct Viewer {
        viewer: User,
    }
    let response: Viewer = graphql(
        http_client,
        key,
        "query { viewer { id name displayName } }",
        json!({}),
    )
    .await?;
    Ok(response.viewer)
}

async fn list_issues(http_client: &dyn HttpClient, key: &str, filter: Value) -> Result<Vec<Issue>> {
    #[derive(Deserialize)]
    struct Issues {
        #[serde(deserialize_with = "nodes")]
        issues: Vec<Issue>,
    }
    let response: Issues = graphql(
        http_client,
        key,
        &format!(
            "query($filter: IssueFilter, $first: Int) {{ \
             issues(filter: $filter, first: $first, orderBy: updatedAt) {{ nodes {{ {ISSUE_FIELDS} }} }} }}"
        ),
        json!({ "filter": filter, "first": LIST_LIMIT }),
    )
    .await?;
    Ok(response.issues)
}

async fn search_issues(
    http_client: &dyn HttpClient,
    key: &str,
    term: &str,
    filter: Value,
    limit: usize,
) -> Result<Vec<Issue>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Search {
        #[serde(deserialize_with = "nodes")]
        search_issues: Vec<Issue>,
    }
    let response: Search = graphql(
        http_client,
        key,
        &format!(
            "query($term: String!, $filter: IssueFilter, $first: Int) {{ \
             searchIssues(term: $term, filter: $filter, first: $first) {{ nodes {{ {ISSUE_FIELDS} }} }} }}"
        ),
        json!({ "term": term, "filter": filter, "first": limit }),
    )
    .await?;
    Ok(response.search_issues)
}

/// How many issues one lookup request asks for.
const LOOKUP_BATCH: usize = 10;

/// Issues by exact key — an identifier such as `ENG-123` or Linear's own id —
/// each asked for under an alias of its own, several to a request.
///
/// `issue(id:)` takes either kind of key, finds archived issues, and finds an
/// issue by its id after its identifier has changed. A filter on team key and
/// number looks like it would do the same for identifiers in one query, but
/// Linear does not apply the number inside `or`, and answers with the team's
/// newest issues instead. A key with no issue is an error for its alias alone;
/// the others in the request are still answered.
async fn issues_by_key(
    http_client: &dyn HttpClient,
    key: &str,
    keys: &[SharedString],
) -> Result<Vec<Issue>> {
    let mut found = Vec::new();
    for batch in keys.chunks(LOOKUP_BATCH) {
        let parameters = (0..batch.len())
            .map(|index| format!("$k{index}: String!"))
            .collect::<Vec<_>>()
            .join(", ");
        let fields = (0..batch.len())
            .map(|index| format!("i{index}: issue(id: $k{index}) {{ {ISSUE_FIELDS} }}"))
            .collect::<Vec<_>>()
            .join(" ");
        let variables: serde_json::Map<String, Value> = batch
            .iter()
            .enumerate()
            .map(|(index, key)| (format!("k{index}"), json!(key.as_ref())))
            .collect();
        let (status, body) = post_graphql(
            http_client,
            key,
            &format!("query({parameters}) {{ {fields} }}"),
            Value::Object(variables),
        )
        .await?;
        let response: Value = serde_json::from_str(&body).with_context(|| {
            format!("Linear answered {status} with something that is not GraphQL")
        })?;
        let Some(data) = response.get("data").and_then(Value::as_object) else {
            // No data at all is the request failing, not an issue missing.
            let message = response
                .pointer("/errors/0/message")
                .and_then(Value::as_str)
                .unwrap_or("no data");
            return Err(anyhow!("Linear answered {status}: {message}"));
        };
        for index in 0..batch.len() {
            let Some(issue) = data.get(&format!("i{index}")).filter(|issue| !issue.is_null())
            else {
                continue;
            };
            match serde_json::from_value::<Issue>(issue.clone()) {
                Ok(issue) => found.push(issue),
                Err(error) => log::warn!("reading a Linear issue: {error:#}"),
            }
        }
    }
    Ok(found)
}

async fn fetch_catalog(http_client: &dyn HttpClient, key: &str) -> Result<Catalog> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Response {
        #[serde(deserialize_with = "nodes")]
        projects: Vec<Named>,
        #[serde(deserialize_with = "nodes")]
        issue_labels: Vec<Named>,
    }
    // Group labels are left out: an issue carries the labels inside a group,
    // never the group itself, so filtering by one finds nothing.
    let mut response: Response = graphql(
        http_client,
        key,
        "query { projects(first: 100, orderBy: updatedAt) { nodes { id name } } \
         issueLabels(first: 250, filter: { isGroup: { eq: false } }) { nodes { id name } } }",
        json!({}),
    )
    .await?;
    response
        .projects
        .sort_by_key(|project| project.name.to_lowercase());
    response
        .issue_labels
        .sort_by_key(|label| label.name.to_lowercase());
    // Labels are per team, and teams often share the same names. A filter
    // offering "Bug" five times is no use; the first of each name stands in.
    response
        .issue_labels
        .dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
    Ok(Catalog {
        projects: response.projects,
        labels: response.issue_labels,
    })
}

/// The state an issue of this team moves to when work on it begins: the first
/// of the team's "started" states, in the order the team arranged them.
async fn first_started_state(
    http_client: &dyn HttpClient,
    key: &str,
    issue_id: &str,
) -> Result<Option<SharedString>> {
    #[derive(Deserialize)]
    struct Response {
        issue: IssueTeam,
    }
    #[derive(Deserialize)]
    struct IssueTeam {
        team: TeamStates,
    }
    #[derive(Deserialize)]
    struct TeamStates {
        #[serde(deserialize_with = "nodes")]
        states: Vec<PositionedState>,
    }
    #[derive(Deserialize)]
    struct PositionedState {
        id: SharedString,
        position: f64,
    }
    let response: Response = graphql(
        http_client,
        key,
        "query($id: String!) { issue(id: $id) { team { \
         states(filter: { type: { eq: \"started\" } }) { nodes { id position } } } } }",
        json!({ "id": issue_id }),
    )
    .await?;
    Ok(response
        .issue
        .team
        .states
        .into_iter()
        .min_by(|a, b| a.position.total_cmp(&b.position))
        .map(|state| state.id))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui::{AsyncApp, TestAppContext};
    use http_client::{FakeHttpClient, Response};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NoCredentials;

    impl CredentialsProvider for NoCredentials {
        fn read_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
            Box::pin(async { Ok(None) })
        }

        fn write_credentials<'a>(
            &'a self,
            _url: &'a str,
            _username: &'a str,
            _password: &'a [u8],
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn delete_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    pub(crate) fn issue_json(identifier: &str) -> Value {
        json!({
            "id": format!("id-{identifier}"),
            "identifier": identifier,
            "title": "Fix login",
            "url": format!("https://linear.app/acme/issue/{identifier}"),
            "branchName": format!("me/{}-fix-login", identifier.to_lowercase()),
            "priority": 0,
            "priorityLabel": "No priority",
            "state": { "id": "s", "name": "Todo", "color": "#e2e2e2", "type": "unstarted" },
            "assignee": null,
            "team": { "id": "t", "key": "RB", "name": "Rentbee" },
            "project": null,
            "cycle": null,
            "labels": { "nodes": [] },
        })
    }

    /// A connected `Linear` whose requests fail for the first `failures`
    /// calls and then find `identifier`.
    fn linear_failing_then_finding(
        failures: usize,
        identifier: &'static str,
        cx: &mut TestAppContext,
    ) -> Entity<Linear> {
        let calls = Arc::new(AtomicUsize::new(0));
        let http_client = FakeHttpClient::create(move |_| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if call < failures {
                    return Err(anyhow!("dns error: failed to lookup address information"));
                }
                let body = json!({ "data": { "i0": issue_json(identifier) } });
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(body.to_string()))?)
            }
        });
        connected_linear(http_client, cx)
    }

    /// A `Linear` connected with a key, asking `http_client` for everything.
    pub(crate) fn connected_linear(
        http_client: Arc<dyn HttpClient>,
        cx: &mut TestAppContext,
    ) -> Entity<Linear> {
        cx.new(|_| Linear {
            http_client,
            credentials: Arc::new(NoCredentials),
            connection: Connection::Connected {
                key: "key".into(),
                source: KeySource::Environment,
                viewer: None,
            },
            catalog: Catalog::default(),
            filters: IssueFilters::default(),
            grouping: IssueGrouping::default(),
            collapsed_groups: HashSet::new(),
            query: String::new(),
            issues: Vec::new(),
            loading: false,
            list_error: None,
            _list: None,
            _catalog: None,
            _load_key: None,
            lookups: HashMap::new(),
            queued_lookups: HashSet::new(),
            watched: HashSet::new(),
            _poll: Task::ready(()),
            looking_up: false,
        })
    }

    /// Bench launching before the network is up is the usual way this
    /// happens: the first lookup fails, and the worktree's issue has to show
    /// up once Linear can be reached, without waiting for a redraw.
    #[gpui::test]
    async fn a_lookup_that_could_not_reach_linear_is_retried(cx: &mut TestAppContext) {
        let linear = linear_failing_then_finding(1, "RB-146", cx);
        linear.update(cx, |linear, cx| {
            linear.look_up_branches(["RB-146-connect-extras"], cx)
        });
        cx.run_until_parked();
        linear.read_with(cx, |linear, _| {
            assert!(linear.issue_for_branch("RB-146-connect-extras").is_none());
        });

        cx.executor().advance_clock(LOOKUP_RETRY);
        cx.run_until_parked();
        linear.read_with(cx, |linear, _| {
            assert_eq!(
                linear
                    .issue_for_branch("RB-146-connect-extras")
                    .map(|issue| issue.identifier.clone())
                    .as_deref(),
                Some("RB-146")
            );
        });
    }

    /// A worktree Bench made remembers its issue's id, which finds the issue
    /// even once its identifier has changed, and asks for archived ones too.
    #[gpui::test]
    async fn an_issue_is_found_by_its_id(cx: &mut TestAppContext) {
        const ID: &str = "3f2b1c4d-0000-4000-8000-00000000abcd";
        let asked_for_archived = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let http_client = FakeHttpClient::create({
            let asked_for_archived = asked_for_archived.clone();
            move |request| {
                let asked_for_archived = asked_for_archived.clone();
                async move {
                    let mut body = String::new();
                    let mut request_body = request.into_body();
                    futures::AsyncReadExt::read_to_string(&mut request_body, &mut body).await?;
                    // `issue(id:)` is what finds archived issues, and an
                    // issue whose identifier has changed.
                    if body.contains("issue(id: $k0)") && body.contains(ID) {
                        asked_for_archived.store(true, Ordering::SeqCst);
                    }
                    let mut issue = issue_json("RB2-7");
                    issue["id"] = json!(ID);
                    let response = json!({ "data": { "i0": issue } });
                    Ok(Response::builder()
                        .status(200)
                        .body(AsyncBody::from(response.to_string()))?)
                }
            }
        });
        let linear = connected_linear(http_client, cx);
        linear.update(cx, |linear, cx| linear.look_up_ids([ID], cx));
        cx.run_until_parked();

        linear.read_with(cx, |linear, _| {
            assert_eq!(
                linear
                    .issue_for_id(ID)
                    .map(|issue| issue.identifier.clone())
                    .as_deref(),
                Some("RB2-7"),
                "found by id, under the identifier it has now"
            );
        });
        assert!(asked_for_archived.load(Ordering::SeqCst));
    }

    /// The bug this replaced: asking for RB-116, RB-154 and RB-134 by team
    /// and number came back with the team's three newest issues. Each key is
    /// asked for exactly now, and one with no issue leaves the others found.
    #[gpui::test]
    async fn each_issue_is_found_by_its_own_key(cx: &mut TestAppContext) {
        let http_client = FakeHttpClient::create(|_| async {
            let body = json!({
                "data": { "i0": null, "i1": issue_json("RB-134") },
                "errors": [{ "message": "Entity not found", "path": ["i0"] }],
            });
            Ok(Response::builder()
                .status(200)
                .body(AsyncBody::from(body.to_string()))?)
        });
        let linear = connected_linear(http_client, cx);
        linear.update(cx, |linear, cx| {
            linear.look_up_branches(["dev/rb-999-gone", "rb-134-new-website"], cx)
        });
        cx.run_until_parked();
        linear.read_with(cx, |linear, _| {
            assert!(linear.issue_for_branch("rb-134-new-website").is_some());
            assert!(linear.issue_for_branch("dev/rb-999-gone").is_none());
        });
    }

    /// Statuses change in Linear: what a panel shows is asked about again on
    /// the poll, without anything redrawing.
    #[gpui::test]
    async fn watched_issues_are_asked_about_again(cx: &mut TestAppContext) {
        let states = Arc::new(std::sync::Mutex::new(vec!["unstarted", "started"]));
        let http_client = FakeHttpClient::create({
            let states = states.clone();
            move |_| {
                let states = states.clone();
                async move {
                    let state = {
                        let mut states = states.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        if states.len() > 1 { states.remove(0) } else { states[0] }
                    };
                    let mut issue = issue_json("RB-134");
                    issue["state"]["type"] = json!(state);
                    let body = json!({ "data": { "i0": issue } });
                    Ok(Response::builder()
                        .status(200)
                        .body(AsyncBody::from(body.to_string()))?)
                }
            }
        });
        let linear = connected_linear(http_client, cx);
        linear.update(cx, |linear, cx| linear.look_up_branches(["rb-134-x"], cx));
        cx.run_until_parked();
        let state = |cx: &mut TestAppContext| {
            linear.read_with(cx, |linear, _| {
                linear.issue_for_branch("rb-134-x").map(|issue| issue.state.kind)
            })
        };
        assert_eq!(state(cx), Some(StateType::Unstarted));

        linear.update(cx, |linear, cx| linear.poll(cx));
        cx.run_until_parked();
        assert_eq!(state(cx), Some(StateType::Started));
    }

    /// A refresh that fails leaves the issue on screen rather than making the
    /// worktree look unlinked.
    #[gpui::test]
    async fn a_failed_refresh_keeps_what_was_known(cx: &mut TestAppContext) {
        let linear = linear_failing_then_finding(usize::MAX, "RB-146", cx);
        linear.update(cx, |linear, cx| {
            let issue: Issue = serde_json::from_value(issue_json("RB-146")).unwrap();
            linear.remember(Arc::new(issue));
            // Due for a refresh.
            for lookup in linear.lookups.values_mut() {
                lookup.fresh_until = Instant::now();
            }
            linear.look_up_branches(["RB-146-connect-extras"], cx);
        });
        cx.run_until_parked();
        linear.read_with(cx, |linear, _| {
            assert!(linear.issue_for_branch("RB-146-connect-extras").is_some());
        });
    }

    #[test]
    fn finds_the_identifier_a_branch_is_named_after() {
        assert_eq!(
            identifier_in_branch("gobinath/eng-123-fix-login").as_deref(),
            Some("ENG-123")
        );
        assert_eq!(
            identifier_in_branch("eng-123-fix-login").as_deref(),
            Some("ENG-123")
        );
        assert_eq!(identifier_in_branch("ENG-7").as_deref(), Some("ENG-7"));
        assert_eq!(identifier_in_branch("a1-42-x").as_deref(), Some("A1-42"));
    }

    #[test]
    fn leaves_branches_that_are_not_named_after_an_issue() {
        assert_eq!(identifier_in_branch("main"), None);
        assert_eq!(identifier_in_branch("fix-login"), None);
        assert_eq!(identifier_in_branch("feature/123-thing"), None);
        assert_eq!(identifier_in_branch("eng-"), None);
        assert_eq!(identifier_in_branch("toolongkey-12"), None);
        assert_eq!(identifier_in_branch(""), None);
    }

    #[test]
    fn default_filters_are_my_unfinished_issues() {
        assert_eq!(
            IssueFilters::default().to_graphql(),
            json!({
                "assignee": { "isMe": { "eq": true } },
                "state": { "type": { "in": ["triage", "backlog", "unstarted", "started"] } },
            })
        );
    }

    #[test]
    fn every_filter_becomes_part_of_the_issue_filter() {
        let filters = IssueFilters {
            assignee: AssigneeFilter::Unassigned,
            states: Vec::new(),
            project: Some(Named {
                id: "project".into(),
                name: "Project".into(),
            }),
            cycle: CycleFilter::Current,
            labels: vec![Named {
                id: "bug".into(),
                name: "Bug".into(),
            }],
        };
        assert_eq!(
            filters.to_graphql(),
            json!({
                "assignee": { "null": true },
                "project": { "id": { "eq": "project" } },
                "cycle": { "isActive": { "eq": true } },
                "labels": { "some": { "id": { "in": ["bug"] } } },
            })
        );
    }

    #[test]
    fn the_dashboard_asks_for_open_and_recently_finished_issues_of_any_status() {
        let filter = dashboard_filter(&IssueFilters::default(), "2026-07-06T00:00:00Z");
        assert_eq!(
            filter,
            json!({
                "and": [
                    { "assignee": { "isMe": { "eq": true } } },
                    { "or": [
                        { "completedAt": { "null": true }, "canceledAt": { "null": true } },
                        { "completedAt": { "gte": "2026-07-06T00:00:00Z" } },
                        { "canceledAt": { "gte": "2026-07-06T00:00:00Z" } },
                    ] },
                ]
            })
        );
    }

    #[test]
    fn reads_an_issue_as_linear_sends_it() {
        let issue: Issue = serde_json::from_value(json!({
            "id": "uuid",
            "identifier": "ENG-1",
            "title": "Fix login",
            "url": "https://linear.app/acme/issue/ENG-1/fix-login",
            "branchName": "me/eng-1-fix-login",
            "priority": 2,
            "priorityLabel": "High",
            "state": { "id": "s", "name": "In Review", "color": "#0f783c", "type": "started" },
            "assignee": null,
            "team": { "id": "t", "key": "ENG", "name": "Engineering" },
            "project": null,
            "cycle": { "id": "c", "number": 4, "name": null },
            "labels": { "nodes": [{ "id": "l", "name": "Bug", "color": "#eb5757" }] },
        }))
        .unwrap();
        assert_eq!(issue.state.kind, StateType::Started);
        assert_eq!(issue.labels.len(), 1);
        assert_eq!(issue.cycle.map(|cycle| cycle.label()).as_deref(), Some("Cycle 4"));
        assert!(issue.state.color().is_some());
    }
}
