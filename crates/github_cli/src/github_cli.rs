//! Pull requests, by way of the GitHub CLI.
//!
//! Zed's [`crate::hosting_provider`] knows how to *build* a pull request URL
//! and how to read one out of a merge commit, but nothing here or anywhere
//! else can ask a host what pull requests exist — that needs an authenticated
//! API call. `gh` is already installed and already signed in for anyone who
//! opens pull requests from a terminal, so asking it is a question Bench can
//! answer without holding a token of its own, and without an account screen to
//! sign in on.
//!
//! The cost is that this is GitHub-only. Every failure is a reason rather than
//! an empty list — see [`Unavailable`] — because in each case the person
//! reading it is the one who can fix it.

use std::path::Path;

use gpui::SharedString;
use serde::Deserialize;

/// One pull request, as `gh` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: SharedString,
    /// Where the pull request lives. `gh` hands back the full URL, so nothing
    /// on this side has to know how a host spells one.
    pub url: SharedString,
    pub state: PullRequestState,
    pub author: SharedString,
    /// The branch the pull request is *from*, which is what ties it to a
    /// worktree: one worktree is one branch checked out.
    pub head_ref: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    /// Open, but marked as not ready. GitHub reports this as an open pull
    /// request with a flag, and it reads differently enough to keep apart.
    Draft,
    Merged,
    Closed,
}

impl PullRequestState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Draft => "Draft",
            Self::Merged => "Merged",
            Self::Closed => "Closed",
        }
    }
}

/// Why there is no list, in words meant for the person who can do something
/// about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// `gh` is not on the `PATH` this process was started with.
    CliMissing,
    /// `gh` ran and said no: not signed in, no remote, a remote that is not
    /// GitHub. Its own words are carried through rather than summarised,
    /// because they name which of those it was.
    Refused(SharedString),
}

impl Unavailable {
    pub fn message(&self) -> SharedString {
        match self {
            Self::CliMissing => {
                "Pull requests need the GitHub CLI. Install `gh` and sign in with `gh auth login`."
                    .into()
            }
            Self::Refused(message) => message.clone(),
        }
    }
}

/// How many pull requests to ask for. Enough that a branch's whole history of
/// them is there, small enough that the answer is one page and arrives at once.
pub const DEFAULT_LIMIT: usize = 50;

/// The pull requests of the repository at `work_directory`, newest first.
///
/// `head_branch` narrows the list to the pull requests opened *from* that
/// branch; without it the answer covers every branch, which is what a caller
/// showing many worktrees at once wants — one process rather than one per
/// worktree.
///
/// Closed and merged pull requests are included. A branch whose pull request
/// has just merged is the case where knowing is worth most, and an empty list
/// would be indistinguishable from never having opened one.
pub async fn pull_requests(
    work_directory: &Path,
    head_branch: Option<&str>,
    limit: usize,
) -> Result<Vec<PullRequest>, Unavailable> {
    let gh = which::which("gh").map_err(|_| Unavailable::CliMissing)?;

    let mut command = util::command::new_command(gh);
    command
        .current_dir(work_directory)
        // A `gh` left running is a `gh` nobody is waiting for: the caller drops
        // this future when the panel goes away or the branch changes under it.
        .kill_on_drop(true)
        .args(["pr", "list", "--state", "all"])
        .args(["--limit", &limit.to_string()])
        .args(["--json", "number,title,url,state,isDraft,author,headRefName"]);
    if let Some(head_branch) = head_branch {
        command.args(["--head", head_branch]);
    }

    let output = command
        .output()
        .await
        .map_err(|error| Unavailable::Refused(format!("Could not run gh: {error}").into()))?;

    if !output.status.success() {
        let refusal = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Unavailable::Refused(if refusal.is_empty() {
            "gh could not list pull requests".into()
        } else {
            refusal.into()
        }));
    }

    let listed: Vec<ListedPullRequest> = serde_json::from_slice(&output.stdout)
        .map_err(|error| Unavailable::Refused(format!("Could not read gh's answer: {error}").into()))?;
    Ok(listed.into_iter().map(PullRequest::from).collect())
}

/// `gh pr list --json`'s shape, which is why these names are not ours.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedPullRequest {
    number: u64,
    title: String,
    url: String,
    state: String,
    is_draft: bool,
    /// Absent for a pull request whose author no longer has an account.
    author: Option<ListedAuthor>,
    head_ref_name: String,
}

#[derive(Deserialize)]
struct ListedAuthor {
    login: String,
}

impl From<ListedPullRequest> for PullRequest {
    fn from(listed: ListedPullRequest) -> Self {
        let state = match listed.state.as_str() {
            "MERGED" => PullRequestState::Merged,
            "CLOSED" => PullRequestState::Closed,
            _ if listed.is_draft => PullRequestState::Draft,
            _ => PullRequestState::Open,
        };
        Self {
            number: listed.number,
            title: listed.title.into(),
            url: listed.url.into(),
            state,
            author: listed
                .author
                .map(|author| author.login)
                .unwrap_or_else(|| "ghost".to_owned())
                .into(),
            head_ref: listed.head_ref_name.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one piece of this with a decision in it: three fields of `gh`'s
    /// answer collapse into one state, and a draft is an *open* pull request
    /// with a flag rather than a state of its own.
    #[test]
    fn a_draft_is_open_with_a_flag_and_merged_beats_both() {
        let listed: Vec<ListedPullRequest> = serde_json::from_str(
            r#"[
                {"number":1,"title":"Add the thing","url":"https://github.com/o/r/pull/1",
                 "state":"OPEN","isDraft":false,"author":{"login":"octocat"},
                 "headRefName":"feature/thing"},
                {"number":2,"title":"Start the thing","url":"https://github.com/o/r/pull/2",
                 "state":"OPEN","isDraft":true,"author":{"login":"octocat"},
                 "headRefName":"feature/draft"},
                {"number":3,"title":"Ship the thing","url":"https://github.com/o/r/pull/3",
                 "state":"MERGED","isDraft":false,"author":null,
                 "headRefName":"feature/done"}
            ]"#,
        )
        .expect("gh's own shape");

        let parsed: Vec<PullRequest> = listed.into_iter().map(PullRequest::from).collect();

        assert_eq!(parsed[0].state, PullRequestState::Open);
        assert_eq!(parsed[0].author, "octocat");
        assert_eq!(parsed[1].state, PullRequestState::Draft);
        assert_eq!(parsed[2].state, PullRequestState::Merged);
        assert_eq!(parsed[2].head_ref, "feature/done");
        assert_eq!(
            parsed[2].author, "ghost",
            "a pull request outlives the account that opened it"
        );
    }
}
