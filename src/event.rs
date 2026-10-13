use crate::{
    app::{EnvironmentOp, IntentTarget, LayoutMode, Project},
    compose::Environment,
    integration::AgentUpdate,
    intent::{Agent, Plan},
    worktree::Worktree,
};
use std::path::PathBuf;

pub enum Event {
    Input(crossterm::event::Event),
    Dirty(u64),
    Exited {
        session: u64,
        code: String,
    },
    Agent(AgentUpdate),
    ProjectOpened {
        request: u64,
        result: Result<Project, String>,
        agent: Option<Agent>,
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
        base: PathBuf,
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
    /// Background Git discovery; failures leave the last known metadata intact.
    Metadata(Vec<Worktree>),
    Removed {
        tree: String,
        result: Result<(), String>,
    },
    Environment {
        tree: String,
        operation: EnvironmentOp,
        prepared: bool,
        result: Result<Environment, String>,
    },
    Error(String),
    Shutdown,
}
