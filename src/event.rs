use crate::{
    app::{EnvironmentOp, IntentTarget, LayoutMode, Project},
    compose::Environment,
    integration::AgentUpdate,
    intent::{Agent, Plan},
    sandbox::CleanReport,
    worktree::Worktree,
};
use std::path::PathBuf;

pub enum Event {
    Input(crossterm::event::Event),
    Dirty(u64),
    /// A pane's program ended; `code` says how, for the pane's header.
    Exited {
        session: u64,
        code: String,
        /// Exit code 0, not a signal. False as well when the status could not be read.
        success: bool,
        /// How long the program ran: an agent that ends at once most likely did not start.
        ran: std::time::Duration,
    },
    /// A pane's OSC 52 copy. Only the UI thread may write to the outer terminal.
    Clipboard {
        ty: Vec<u8>,
        data: Vec<u8>,
    },
    /// The system clipboard, read off the UI thread: its text, or why it was unreadable.
    /// `target` is the session that asked, or `None` for the dialog that was open then.
    ClipboardText {
        target: Option<u64>,
        result: Result<String, String>,
    },
    Agent(AgentUpdate),
    ProjectOpened {
        request: u64,
        result: Result<Project, String>,
        agent: Option<Agent>,
        /// Open another tab for the folder instead of selecting the one it already has.
        fresh_tab: bool,
    },
    IntentTargets {
        request: u64,
        result: Result<Vec<IntentTarget>, String>,
    },
    IntentResolved {
        request: u64,
        result: Result<Plan, String>,
    },
    Projects {
        request: u64,
        projects: Vec<PathBuf>,
    },
    Created {
        project: PathBuf,
        source: PathBuf,
        preparation_error: Option<String>,
        agent: Option<Agent>,
        split: Option<LayoutMode>,
        result: Result<Worktree, String>,
    },
    Worktrees {
        request: u64,
        project: PathBuf,
        result: Result<Vec<Worktree>, String>,
    },
    SplitTargets {
        request: u64,
        project: PathBuf,
        layout: LayoutMode,
        result: Result<Vec<crate::worktree::SplitTarget>, String>,
    },
    SplitOpened {
        request: u64,
        project: PathBuf,
        layout: LayoutMode,
        choices: Vec<crate::worktree::SplitTarget>,
        selected: usize,
        source: PathBuf,
        preparation_error: Option<String>,
        result: Result<(Worktree, bool), String>,
    },
    /// Background Git discovery: each repository that changed since its last listing and listed
    /// successfully, keyed by its common directory. Unchanged repositories and failures leave the
    /// last known metadata intact.
    Metadata(Vec<(PathBuf, Vec<Worktree>)>),
    Removed {
        tree: String,
        /// `Ok` carries why Docker could not be checked, when it could not.
        result: Result<Option<String>, String>,
        /// Why the removed checkout's scratch could not be removed from the sandbox, when it
        /// could not: only a warning, as the checkout itself is gone.
        scratch: Option<String>,
    },
    Environment {
        tree: String,
        operation: EnvironmentOp,
        prepared: bool,
        result: Result<Environment, String>,
    },
    /// The agent CPU limit could not be applied: an asynchronous error for a dialog.
    CpuLimit(String),
    /// A pass over the sandbox ended: what it removed, or why the sandbox could not be read.
    SandboxCleaned(Result<CleanReport, String>),
    Error(String),
    Shutdown,
}
