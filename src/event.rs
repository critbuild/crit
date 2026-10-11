use crate::{
    app::{IntentTarget, Project},
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
        agent: Option<Agent>,
        result: Result<Worktree, String>,
    },
    Worktrees {
        project: PathBuf,
        result: Result<Vec<Worktree>, String>,
    },
    Removed {
        tree: String,
        result: Result<(), String>,
    },
    Environment {
        tree: String,
        result: Result<Environment, String>,
    },
    Error(String),
    Shutdown,
}
