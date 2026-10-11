use crate::{
    compose,
    event::Event,
    integration::{self, AgentState},
    intent::{self, Agent as AgentKind, Command as IntentCommand, Plan},
    keys::{self, Action},
    kitty,
    terminal::Session,
    ui, updates,
    worktree::{self, Worktree},
};
use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Full,
    Vertical,
    Horizontal,
}
#[derive(Clone, Copy)]
pub enum Confirm {
    Quit,
    CloseWorktree,
    CloseSession,
    StopEnvironment,
    RemoveWorktree,
}

pub struct ProjectPicker {
    pub base: PathBuf,
    pub terminal_path: Option<PathBuf>,
    pub input: String,
    pub projects: Vec<PathBuf>,
    pub selected: usize,
    pub searching: bool,
    pub pending: bool,
    search_text: Vec<String>,
    matches: Vec<usize>,
}
impl ProjectPicker {
    fn new(base: PathBuf, jobs: &SyncSender<Job>) -> Result<Self> {
        jobs.try_send(Job::Projects(base.clone()))
            .context("command queue is busy")?;
        Ok(Self {
            base,
            terminal_path: None,
            input: String::new(),
            projects: Vec::new(),
            selected: 0,
            searching: true,
            pending: false,
            search_text: Vec::new(),
            matches: Vec::new(),
        })
    }

    fn set_projects(&mut self, projects: Vec<PathBuf>) {
        self.search_text.clear();
        self.search_text.extend(
            projects
                .iter()
                .map(|path| path.to_string_lossy().to_lowercase()),
        );
        self.projects = projects;
        self.searching = false;
        self.filter();
    }

    fn filter(&mut self) {
        let query = self.input.trim().to_lowercase();
        self.matches.clear();
        self.matches.extend(
            self.search_text
                .iter()
                .enumerate()
                .filter_map(|(index, text)| text.contains(&query).then_some(index)),
        );
        self.selected = 0;
    }

    pub fn visible_projects(&self) -> impl ExactSizeIterator<Item = &PathBuf> {
        self.matches.iter().map(|&index| &self.projects[index])
    }

    pub fn shows_terminal(&self) -> bool {
        self.terminal_path.is_some() && self.input.trim().is_empty()
    }

    fn paste(&mut self, text: &str) {
        if self.pending {
            return;
        }
        let previous_len = self.input.len();
        paste_input(&mut self.input, text, 4096);
        if self.input.len() != previous_len {
            self.filter();
        }
    }

    fn key(&mut self, key: KeyEvent, jobs: &SyncSender<Job>) -> Result<bool> {
        if self.pending {
            return Ok(false);
        }
        match key.code {
            KeyCode::Enter => {
                if self.shows_terminal() && self.selected == 0 {
                    return Ok(true);
                }
                let input = self.input.trim();
                let selected = self
                    .matches
                    .get(
                        self.selected
                            .saturating_sub(usize::from(self.shows_terminal())),
                    )
                    .map(|&index| &self.projects[index]);
                let path = if input.is_empty() {
                    selected.cloned()
                } else {
                    let explicit = Path::new(input).is_absolute()
                        || matches!(input, "~" | "." | "..")
                        || input.starts_with("~/")
                        || input.starts_with("./")
                        || input.starts_with("../");
                    if explicit {
                        Some(project_path(input, &self.base)?)
                    } else if let Some(selected) = selected {
                        Some(selected.clone())
                    } else {
                        let path = project_path(input, &self.base)?;
                        path.is_dir().then_some(path)
                    }
                };
                if let Some(path) = path {
                    jobs.try_send(Job::OpenProject { path, agent: None })
                        .context("command queue is busy")?;
                    self.pending = true;
                }
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Home
            | KeyCode::End => {
                let count = self.matches.len() + usize::from(self.shows_terminal());
                navigate(key, &mut self.selected, count);
            }
            _ => {
                let previous_len = self.input.len();
                edit_input(key, &mut self.input, 4096);
                if self.input.len() != previous_len {
                    self.filter();
                }
            }
        }
        Ok(false)
    }
}

pub struct IntentDialog {
    pub input: String,
    pub error: Option<String>,
    pub loading: bool,
    pub pending: bool,
    stale: bool,
    worker: Option<std::thread::JoinHandle<()>>,
    request: u64,
    submitted: bool,
    targets: Vec<IntentTarget>,
    context: Arc<serde_json::Value>,
    conversation: Arc<Vec<serde_json::Value>>,
    cancel: Arc<AtomicBool>,
}

impl IntentDialog {
    pub fn question(&self) -> Option<&str> {
        self.conversation
            .iter()
            .rev()
            .find(|turn| turn["role"] == "assistant")
            .and_then(|turn| turn["content"].as_str())
    }
}

impl Drop for IntentDialog {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(serde::Serialize)]
pub struct IntentTarget {
    id: usize,
    kind: &'static str,
    name: String,
    path: PathBuf,
    branch: Option<String>,
}

pub const COMMANDS: &[(&str, IntentCommand)] = &[
    ("Open project folder", IntentCommand::OpenProject),
    ("Open existing worktree", IntentCommand::OpenWorktree),
    ("Switch project", IntentCommand::SwitchProject),
    ("New branch / worktree", IntentCommand::NewWorktree),
    ("New agent / shell session", IntentCommand::NewSession),
    ("Split side by side", IntentCommand::SplitVertical),
    ("Split top and bottom", IntentCommand::SplitHorizontal),
    ("Full width", IntentCommand::FullWidth),
    (
        "Start isolated environment",
        IntentCommand::StartEnvironment,
    ),
    (
        "Stop environment (retain data)",
        IntentCommand::StopEnvironment,
    ),
    ("Restart environment", IntentCommand::RestartEnvironment),
    (
        "Refresh environment status",
        IntentCommand::RefreshEnvironment,
    ),
    ("Environment logs", IntentCommand::EnvironmentLogs),
    ("Open preview endpoint", IntentCommand::Preview),
    ("Close worktree tab", IntentCommand::CloseWorktree),
    ("Close agent / shell session", IntentCommand::CloseSession),
    (
        "Remove worktree (clean only)",
        IntentCommand::RemoveWorktree,
    ),
    ("Toggle native Kitty shortcuts", IntentCommand::ToggleNative),
    ("Exit Crit", IntentCommand::Quit),
];

pub enum Modal {
    Intent(IntentDialog),
    Commands {
        selected: usize,
    },
    OpenProject(ProjectPicker),
    SwitchProject {
        selected: usize,
    },
    NewWorktree {
        input: String,
    },
    NewSession {
        selected: usize,
    },
    OpenWorktree {
        trees: Vec<Worktree>,
        selected: usize,
    },
    Split {
        layout: LayoutMode,
        selected: usize,
    },
    Preview {
        urls: Vec<(String, String)>,
        selected: usize,
    },
    Confirm(Confirm),
}

pub struct Workspace {
    pub tree: Worktree,
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub environment: Option<compose::Environment>,
    pub pending: bool,
}
impl Workspace {
    fn new(tree: Worktree) -> Self {
        Self {
            tree,
            sessions: Vec::new(),
            selected: 0,
            environment: None,
            pending: false,
        }
    }
    pub fn label(&self) -> &str {
        if self.tree.branch.is_empty() {
            "terminal"
        } else {
            &self.tree.branch
        }
    }
    pub fn state(&self) -> AgentState {
        if self.sessions.iter().any(|s| s.state == AgentState::Blocked) {
            AgentState::Blocked
        } else if self.sessions.iter().any(|s| s.state == AgentState::Working) {
            AgentState::Working
        } else if self.sessions.iter().any(|s| s.state == AgentState::Unknown) {
            AgentState::Unknown
        } else {
            AgentState::Idle
        }
    }
}

pub struct Pane {
    pub workspace: usize,
    pub body: Rect,
}

pub struct Project {
    pub repository: Option<worktree::Repository>,
    pub name: String,
    pub workspaces: Vec<Workspace>,
    pub active: usize,
    pub secondary: Option<usize>,
    pub focus_secondary: bool,
    pub layout: LayoutMode,
}
impl Project {
    fn load(path: &Path) -> Result<Self> {
        let path = path.canonicalize().context("resolve project folder")?;
        anyhow::ensure!(path.is_dir(), "choose a project folder, not a file");
        let repository = worktree::discover(&path)?;
        let mut trees = worktree::list(&repository)?;
        let active = trees
            .iter()
            .enumerate()
            .filter(|(_, tree)| path.starts_with(&tree.path))
            .max_by_key(|(_, tree)| tree.path.components().count())
            .map(|(index, _)| index)
            .context("folder is not inside a checked-out worktree")?;
        let name = trees[0]
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        let tree = trees.swap_remove(active);
        Ok(Self {
            repository: Some(repository),
            name,
            workspaces: vec![Workspace::new(tree)],
            active: 0,
            secondary: None,
            focus_secondary: false,
            layout: LayoutMode::Full,
        })
    }

    fn terminal(path: &Path) -> Result<Self> {
        let path = path.canonicalize().context("resolve terminal folder")?;
        anyhow::ensure!(path.is_dir(), "choose a terminal folder, not a file");
        let tree = Worktree {
            id: format!("terminal:{}", path.display()),
            path,
            branch: String::new(),
            head: String::new(),
            is_main: false,
            locked: false,
        };
        Ok(Self {
            repository: None,
            name: "Terminal".into(),
            workspaces: vec![Workspace::new(tree)],
            active: 0,
            secondary: None,
            focus_secondary: false,
            layout: LayoutMode::Full,
        })
    }

    pub fn path(&self) -> &Path {
        self.repository
            .as_ref()
            .map_or(&self.workspaces[0].tree.path, |repository| &repository.root)
    }

    fn common_dir(&self) -> Option<&Path> {
        self.repository
            .as_ref()
            .map(|repository| repository.common_dir.as_path())
    }

    fn same_project(&self, other: &Self) -> bool {
        match (self.common_dir(), other.common_dir()) {
            (Some(left), Some(right)) => left == right,
            (None, None) => self.path() == other.path(),
            _ => false,
        }
    }

    fn close_workspace(&mut self, index: usize) {
        self.workspaces.remove(index);
        self.active = if self.active > index {
            self.active - 1
        } else {
            self.active.min(self.workspaces.len().saturating_sub(1))
        };
        self.secondary = self.secondary.and_then(|s| {
            if s == index {
                None
            } else {
                Some(if s > index { s - 1 } else { s })
            }
        });
        if self.secondary == Some(self.active) || self.secondary.is_none() {
            self.layout = LayoutMode::Full;
            self.secondary = None;
            self.focus_secondary = false;
        }
    }
}

pub struct App {
    pub project: Project,
    pub saved_projects: Vec<Project>,
    pub native_keys: bool,
    pub modal: Option<Modal>,
    pub notice: Option<String>,
    pub panes: Vec<Pane>,
    pub tab_hits: Vec<(Rect, usize)>,
    pub should_quit: bool,
    default_agent: String,
    next_session: u64,
    next_intent: u64,
    socket: PathBuf,
    tx: SyncSender<Event>,
    jobs: SyncSender<Job>,
    use_kitty: bool,
}

enum Job {
    OpenProject {
        path: PathBuf,
        agent: Option<AgentKind>,
    },
    IntentTargets {
        request: u64,
        base: PathBuf,
        repository: Option<worktree::Repository>,
        known_projects: Vec<PathBuf>,
    },
    Projects(PathBuf),
    Create {
        repository: worktree::Repository,
        branch: String,
        base: Option<String>,
        agent: Option<AgentKind>,
    },
    List(worktree::Repository),
    Environment {
        tree: Worktree,
        operation: EnvironmentOp,
    },
    Remove {
        repository: worktree::Repository,
        tree: Worktree,
    },
}
#[derive(Clone, Copy)]
enum EnvironmentOp {
    Up,
    Down,
    Restart,
    Status,
}

impl App {
    pub fn focused_index(&self) -> usize {
        if self.project.layout != LayoutMode::Full && self.project.focus_secondary {
            self.project.secondary.unwrap_or(self.project.active)
        } else {
            self.project.active
        }
    }
    pub fn focused(&self) -> &Workspace {
        &self.project.workspaces[self.focused_index()]
    }
    pub fn focused_session(&self) -> Option<&Session> {
        let workspace = self.focused();
        workspace.sessions.get(workspace.selected)
    }
    fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some(message.into());
    }

    fn git_repository(&self) -> Result<&worktree::Repository> {
        self.project.repository.as_ref().context(
            "Open a Git project first. Terminal-only workspaces do not have worktrees or Docker environments.",
        )
    }

    fn focused_repository(&self) -> Result<worktree::Repository> {
        Ok(worktree::Repository {
            root: self.focused().tree.path.clone(),
            common_dir: self.git_repository()?.common_dir.clone(),
        })
    }

    fn all_workspaces_mut(&mut self) -> impl Iterator<Item = &mut Workspace> {
        std::iter::once(&mut self.project)
            .chain(self.saved_projects.iter_mut())
            .flat_map(|project| project.workspaces.iter_mut())
    }

    fn switch_project(&mut self, index: usize) -> Result<()> {
        std::mem::swap(&mut self.project, &mut self.saved_projects[index]);
        self.modal = None;
        self.notice = None;
        self.select_workspace(self.focused_index(), None)
    }

    fn open_project(&mut self, mut project: Project, agent: Option<AgentKind>) -> Result<()> {
        if !self.project.same_project(&project) {
            if let Some(index) = self
                .saved_projects
                .iter()
                .position(|saved| saved.same_project(&project))
            {
                std::mem::swap(&mut self.project, &mut self.saved_projects[index]);
            } else {
                let previous = std::mem::replace(&mut self.project, project);
                self.saved_projects.push(previous);
                self.modal = None;
                self.notice = None;
                return self.select_workspace(self.focused_index(), agent);
            }
        }
        // Reopening a folder selects its worktree without duplicating its sessions.
        let tree = project.workspaces.swap_remove(project.active).tree;
        let index = if let Some(index) = self
            .project
            .workspaces
            .iter()
            .position(|w| w.tree.id == tree.id)
        {
            index
        } else {
            self.project.workspaces.push(Workspace::new(tree));
            self.project.workspaces.len() - 1
        };
        self.project.layout = LayoutMode::Full;
        self.project.secondary = None;
        self.project.focus_secondary = false;
        self.modal = None;
        self.notice = None;
        self.select_workspace(index, agent)
    }

    fn spawn(
        &mut self,
        index: usize,
        kind: &str,
        command: Option<(String, Vec<String>)>,
    ) -> Result<()> {
        let (cols, rows) = crossterm::terminal::size()?;
        let session = Session::spawn(
            self.next_session,
            kind,
            &self.project.workspaces[index].tree.path,
            &self.socket,
            self.tx.clone(),
            rows.saturating_sub(8),
            cols.saturating_sub(2),
            command,
        )?;
        self.next_session += 1;
        let workspace = &mut self.project.workspaces[index];
        workspace.sessions.push(session);
        workspace.selected = workspace.sessions.len() - 1;
        Ok(())
    }
    fn select_shell(&mut self) -> Result<()> {
        let index = self.focused_index();
        let workspace = &mut self.project.workspaces[index];
        if let Some(selected) = workspace
            .sessions
            .iter()
            .position(|session| session.kind == "shell" && session.alive.load(Ordering::Acquire))
        {
            workspace.selected = selected;
            Ok(())
        } else {
            self.spawn(index, "shell", None)
        }
    }
    fn ensure_session(&mut self, index: usize) -> Result<()> {
        if self.project.workspaces[index].sessions.is_empty() {
            if self.project.repository.is_none() {
                return self.spawn(index, "shell", None);
            }
            let kind = self.default_agent.clone();
            self.spawn(index, &kind, None)?;
        }
        Ok(())
    }
    fn select_workspace(&mut self, index: usize, agent: Option<AgentKind>) -> Result<()> {
        if index >= self.project.workspaces.len() {
            return Ok(());
        }
        if self.project.layout != LayoutMode::Full && self.project.secondary == Some(index) {
            self.project.focus_secondary = true;
        } else if self.project.active == index {
            self.project.focus_secondary = false;
        } else if self.project.layout != LayoutMode::Full && self.project.focus_secondary {
            self.project.secondary = Some(index);
        } else {
            self.project.active = index;
        }
        if let Some(agent) = agent {
            let kind = agent.as_str();
            if let Some(selected) = self.project.workspaces[index]
                .sessions
                .iter()
                .position(|session| session.kind == kind && session.alive.load(Ordering::Acquire))
            {
                self.project.workspaces[index].selected = selected;
            } else {
                self.spawn(index, kind, None)?;
            }
        } else {
            self.ensure_session(index)?;
        }
        if self.project.repository.is_some()
            && self.project.workspaces[index].environment.is_none()
            && !self.project.workspaces[index].pending
            && compose::has_config(&self.project.workspaces[index].tree)
        {
            self.env(index, EnvironmentOp::Status)?;
        }
        Ok(())
    }
    fn env(&mut self, index: usize, operation: EnvironmentOp) -> Result<()> {
        self.git_repository()?;
        if self.project.workspaces[index].pending {
            self.set_notice("An environment operation is already running for this worktree.");
            return Ok(());
        }
        let tree = self.project.workspaces[index].tree.clone();
        self.jobs
            .try_send(Job::Environment { tree, operation })
            .context("command queue is busy")?;
        self.project.workspaces[index].pending = true;
        Ok(())
    }
    fn split(&mut self, layout: LayoutMode) {
        if self.project.workspaces.len() < 2 {
            self.set_notice("Create or open a second worktree first. F1 → describe what you need.");
            return;
        }
        if self.project.layout == layout {
            self.project.layout = LayoutMode::Full;
            self.project.focus_secondary = false;
        } else {
            self.modal = Some(Modal::Split {
                layout,
                selected: (self.focused_index() + 1) % self.project.workspaces.len(),
            });
        }
    }
    fn close_workspace(&mut self) {
        let index = self.focused_index();
        if self.project.workspaces.len() == 1 {
            if let Some(project) = self.saved_projects.pop() {
                self.project = project;
            } else {
                self.should_quit = true;
                return;
            }
        } else {
            self.project.close_workspace(index);
        }
        self.set_notice("Tab closed; any checkout and Docker environment retained.");
    }
    fn open_intent(&mut self) -> Result<()> {
        let request = self.next_intent;
        self.next_intent += 1;
        let repository = self
            .project
            .repository
            .as_ref()
            .map(|repository| worktree::Repository {
                root: self.focused().tree.path.clone(),
                common_dir: repository.common_dir.clone(),
            });
        let known_projects = std::iter::once(&self.project)
            .chain(&self.saved_projects)
            .filter_map(|project| {
                project
                    .repository
                    .as_ref()
                    .map(|repository| repository.root.clone())
            })
            .collect();
        self.jobs
            .try_send(Job::IntentTargets {
                request,
                base: self.focused().tree.path.clone(),
                repository,
                known_projects,
            })
            .context("command queue is busy")?;
        let context = serde_json::json!({
            "active_project": {
                "name": self.project.name,
                "path": self.project.path(),
                "common_dir": self.project.common_dir(),
                "is_git": self.project.repository.is_some(),
            },
            "focused_worktree": {
                "id": self.focused().tree.id,
                "path": self.focused().tree.path,
                "branch": self.project.repository.as_ref().map(|_| &self.focused().tree.branch),
                "is_git": self.project.repository.is_some(),
            },
            "default_agent": self.default_agent,
            "targets": [],
        });
        self.modal = Some(Modal::Intent(IntentDialog {
            input: String::new(),
            error: None,
            loading: true,
            pending: false,
            stale: false,
            worker: None,
            request,
            submitted: false,
            targets: Vec::new(),
            context: Arc::new(context),
            conversation: Arc::new(Vec::new()),
            cancel: Arc::new(AtomicBool::new(false)),
        }));
        Ok(())
    }

    fn start_intent(dialog: &mut IntentDialog, tx: &SyncSender<Event>) {
        if dialog.loading || dialog.pending || dialog.stale || dialog.input.trim().is_empty() {
            return;
        }
        dialog.submitted = false;
        dialog.error = None;
        dialog.pending = true;
        Arc::make_mut(&mut dialog.conversation).push(serde_json::json!({
            "role": "user", "content": dialog.input.trim(),
        }));
        if let Some(worker) = dialog.worker.take() {
            let _ = worker.join();
        }
        let request = dialog.request;
        let context = Arc::clone(&dialog.context);
        let conversation = Arc::clone(&dialog.conversation);
        let cancel = Arc::clone(&dialog.cancel);
        let tx = tx.clone();
        match std::thread::Builder::new()
            .name("intent".into())
            .spawn(move || {
                let result =
                    intent::resolve(intent::DEFAULT_MODEL, &context, &conversation, &cancel)
                        .map_err(|error| format!("{error:#}"));
                let mut event = Event::IntentResolved { request, result };
                while !cancel.load(Ordering::Acquire) {
                    match tx.try_send(event) {
                        Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => break,
                        Err(mpsc::TrySendError::Full(pending)) => event = pending,
                    }
                    // Closing the dialog can join us even while the UI queue is full.
                    std::thread::sleep(Duration::from_millis(25));
                }
            }) {
            Ok(worker) => dialog.worker = Some(worker),
            Err(error) => {
                dialog.pending = false;
                Arc::make_mut(&mut dialog.conversation).pop();
                dialog.error = Some(format!("Could not start interpreter: {error}"));
            }
        }
    }

    fn execute_intent(&mut self, plan: Plan, targets: &[IntentTarget]) -> Result<()> {
        match plan {
            Plan::Clarify { .. } => anyhow::bail!("A clarification cannot execute an action"),
            Plan::CreateWorktree {
                branch,
                base,
                agent,
            } => {
                let repository = self.focused_repository()?;
                self.set_notice(format!("Creating {branch} in a separate worktree..."));
                self.jobs
                    .try_send(Job::Create {
                        repository,
                        branch,
                        base,
                        agent,
                    })
                    .context("command queue is busy")?;
            }
            Plan::Open { target, agent } => {
                let target = targets
                    .iter()
                    .find(|candidate| candidate.id == target)
                    .context("The interpreter selected an unknown project or worktree")?;
                self.jobs
                    .try_send(Job::OpenProject {
                        path: target.path.clone(),
                        agent,
                    })
                    .context("command queue is busy")?;
                self.set_notice(format!("Opening {}...", target.name));
            }
            Plan::Session { agent } => self.spawn(self.focused_index(), agent.as_str(), None)?,
            Plan::Command { command } => self.run_command(command)?,
        }
        Ok(())
    }

    fn run_command(&mut self, command: IntentCommand) -> Result<()> {
        use IntentCommand::*;
        let index = self.focused_index();
        if matches!(
            command,
            OpenWorktree
                | NewWorktree
                | StartEnvironment
                | StopEnvironment
                | RestartEnvironment
                | RefreshEnvironment
                | EnvironmentLogs
                | Preview
                | RemoveWorktree
        ) {
            self.git_repository()?;
        }
        match command {
            OpenProject => {
                self.modal = Some(Modal::OpenProject(ProjectPicker::new(
                    self.focused().tree.path.clone(),
                    &self.jobs,
                )?))
            }
            OpenWorktree => {
                self.jobs
                    .try_send(Job::List(self.git_repository()?.clone()))
                    .context("command queue is busy")?;
                self.set_notice("Reading worktrees...");
            }
            SwitchProject => {
                if self.saved_projects.is_empty() {
                    self.set_notice("No other project open. F1 → ask to open a project.");
                } else {
                    self.modal = Some(Modal::SwitchProject { selected: 0 });
                }
            }
            NewWorktree => self.action(Action::NewWorktree)?,
            NewSession => self.action(Action::NewSession)?,
            SplitVertical => self.split(LayoutMode::Vertical),
            SplitHorizontal => self.split(LayoutMode::Horizontal),
            FullWidth => {
                self.project.layout = LayoutMode::Full;
                self.project.focus_secondary = false;
            }
            StartEnvironment => self.env(index, EnvironmentOp::Up)?,
            StopEnvironment => self.modal = Some(Modal::Confirm(Confirm::StopEnvironment)),
            RestartEnvironment => self.env(index, EnvironmentOp::Restart)?,
            RefreshEnvironment => self.env(index, EnvironmentOp::Status)?,
            EnvironmentLogs => {
                let args = compose::logs_args(&self.project.workspaces[index].tree)?;
                self.spawn(index, "logs", Some(("docker".into(), args)))?;
            }
            Preview => {
                let urls = self
                    .focused()
                    .environment
                    .as_ref()
                    .map(|environment| {
                        environment
                            .services
                            .iter()
                            .flat_map(|service| {
                                service
                                    .ports
                                    .iter()
                                    .filter(|port| port.protocol == "tcp")
                                    .map(|port| {
                                        (
                                            format!("{}:{}", service.name, port.container),
                                            format!("http://127.0.0.1:{}", port.host),
                                        )
                                    })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if urls.is_empty() {
                    self.set_notice(
                        "No published endpoints. Start or refresh this environment first.",
                    );
                } else {
                    self.modal = Some(Modal::Preview { urls, selected: 0 });
                }
            }
            CloseWorktree => self.action(Action::CloseWorktree)?,
            CloseSession => self.action(Action::CloseSession)?,
            RemoveWorktree => self.modal = Some(Modal::Confirm(Confirm::RemoveWorktree)),
            Quit => self.action(Action::Quit)?,
            ToggleNative => self.action(Action::ToggleNative)?,
        }
        Ok(())
    }

    fn action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::ToggleNative => {
                self.native_keys = !self.native_keys;
                self.modal = None;
                if self.use_kitty {
                    kitty::set_native(&mut io::stdout(), self.native_keys)?;
                }
            }
            Action::Launcher => self.open_intent()?,
            Action::Quit => self.modal = Some(Modal::Confirm(Confirm::Quit)),
            Action::NewWorktree => {
                self.git_repository()?;
                self.modal = Some(Modal::NewWorktree {
                    input: String::new(),
                })
            }
            Action::CloseWorktree => self.modal = Some(Modal::Confirm(Confirm::CloseWorktree)),
            Action::NewSession => self.modal = Some(Modal::NewSession { selected: 0 }),
            Action::CloseSession => self.modal = Some(Modal::Confirm(Confirm::CloseSession)),
            Action::NextWorktree | Action::PreviousWorktree => {
                let current = self.focused_index();
                let next = if action == Action::NextWorktree {
                    (current + 1) % self.project.workspaces.len()
                } else {
                    (current + self.project.workspaces.len() - 1) % self.project.workspaces.len()
                };
                self.select_workspace(next, None)?;
            }
            Action::NextSession | Action::PreviousSession => {
                if self.project.layout != LayoutMode::Full {
                    self.project.focus_secondary = !self.project.focus_secondary;
                } else {
                    let workspace = &mut self.project.workspaces[self.project.active];
                    let count = workspace.sessions.len();
                    if count > 0 {
                        workspace.selected = if action == Action::NextSession {
                            (workspace.selected + 1) % count
                        } else {
                            (workspace.selected + count - 1) % count
                        };
                    }
                }
            }
            Action::FocusSession(index) => {
                let focused = self.focused_index();
                if index < self.project.workspaces[focused].sessions.len() {
                    self.project.workspaces[focused].selected = index;
                }
            }
            Action::SplitVertical => self.split(LayoutMode::Vertical),
            Action::SplitHorizontal => self.split(LayoutMode::Horizontal),
            Action::CycleLayout => match self.project.layout {
                LayoutMode::Full => self.split(LayoutMode::Vertical),
                LayoutMode::Vertical => self.project.layout = LayoutMode::Horizontal,
                LayoutMode::Horizontal => {
                    self.project.layout = LayoutMode::Full;
                    self.project.focus_secondary = false;
                }
            },
        }
        Ok(())
    }

    fn modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(mut modal) = self.modal.take() else {
            return Ok(());
        };
        if key.code == KeyCode::Esc {
            return Ok(());
        }
        let mut retain = true;
        match &mut modal {
            Modal::Intent(dialog) => match key.code {
                KeyCode::F(1) => retain = false,
                KeyCode::F(2) => {
                    self.modal = Some(Modal::Commands { selected: 0 });
                    retain = false;
                }
                KeyCode::Enter
                    if !dialog.pending && !dialog.stale && !dialog.input.trim().is_empty() =>
                {
                    dialog.submitted = true;
                    Self::start_intent(dialog, &self.tx);
                }
                _ if !dialog.pending && !dialog.submitted => {
                    edit_input(key, &mut dialog.input, 4096);
                }
                _ => {}
            },
            Modal::Commands { selected } => {
                navigate(key, selected, COMMANDS.len());
                if key.code == KeyCode::Enter {
                    self.run_command(COMMANDS[*selected].1)?;
                    retain = false;
                }
                if key.code == KeyCode::F(1) {
                    self.open_intent()?;
                    retain = false;
                }
            }
            Modal::OpenProject(picker) => {
                if let Err(error) = picker.key(key, &self.jobs) {
                    self.set_notice(format!("{error:#}"));
                }
            }
            Modal::SwitchProject { selected } => {
                navigate(key, selected, self.saved_projects.len());
                if key.code == KeyCode::Enter && *selected < self.saved_projects.len() {
                    self.switch_project(*selected)?;
                    retain = false;
                }
            }
            Modal::NewWorktree { input } => match key.code {
                KeyCode::Enter if !input.trim().is_empty() => {
                    self.jobs
                        .try_send(Job::Create {
                            branch: input.trim().to_owned(),
                            base: None,
                            agent: None,
                            repository: self.focused_repository()?,
                        })
                        .context("command queue is busy")?;
                    self.set_notice(format!("Creating {}...", input.trim()));
                    retain = false;
                }
                _ => edit_input(key, input, 256),
            },
            Modal::NewSession { selected } => {
                navigate(key, selected, 3);
                if key.code == KeyCode::Enter {
                    self.spawn(
                        self.focused_index(),
                        ["omp", "opencode", "shell"][*selected],
                        None,
                    )?;
                    retain = false;
                }
            }
            Modal::OpenWorktree { trees, selected } => {
                navigate(key, selected, trees.len());
                if key.code == KeyCode::Enter {
                    if let Some(tree) = trees.get(*selected) {
                        let index = if let Some(index) = self
                            .project
                            .workspaces
                            .iter()
                            .position(|w| w.tree.id == tree.id)
                        {
                            index
                        } else {
                            self.project.workspaces.push(Workspace::new(tree.clone()));
                            self.project.workspaces.len() - 1
                        };
                        self.select_workspace(index, None)?;
                    }
                    retain = false;
                }
            }
            Modal::Split { layout, selected } => {
                navigate(key, selected, self.project.workspaces.len());
                if key.code == KeyCode::Enter {
                    let primary = self.focused_index();
                    if *selected != primary {
                        self.project.active = primary;
                        self.project.secondary = Some(*selected);
                        self.project.focus_secondary = false;
                        self.project.layout = *layout;
                        self.ensure_session(*selected)?;
                        if self.project.workspaces[*selected].environment.is_none()
                            && compose::has_config(&self.project.workspaces[*selected].tree)
                        {
                            self.env(*selected, EnvironmentOp::Status)?;
                        }
                        retain = false;
                    } else {
                        self.set_notice("Choose a different worktree for the second pane.");
                    }
                }
            }
            Modal::Preview { urls, selected } => {
                navigate(key, selected, urls.len());
                if key.code == KeyCode::Enter {
                    if let Some((_, url)) = urls.get(*selected) {
                        let url = url.clone();
                        let tx = self.tx.clone();
                        std::thread::spawn(move || {
                            match std::process::Command::new("xdg-open")
                                .arg(&url)
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .status()
                            {
                                Ok(status) if status.success() => {}
                                Ok(status) => {
                                    let _ = tx.send(Event::Error(format!(
                                        "Opening {url} failed: {status}"
                                    )));
                                }
                                Err(error) => {
                                    let _ =
                                        tx.send(Event::Error(format!("Opening {url}: {error}")));
                                }
                            }
                        });
                    }
                    retain = false;
                }
            }
            Modal::Confirm(confirm) => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    let index = self.focused_index();
                    match confirm {
                        Confirm::Quit => self.should_quit = true,
                        Confirm::CloseWorktree => {
                            self.close_workspace();
                            if !self.should_quit {
                                self.ensure_session(self.focused_index())?;
                            }
                        }
                        Confirm::CloseSession => {
                            let workspace = &mut self.project.workspaces[index];
                            if !workspace.sessions.is_empty() {
                                workspace.sessions.remove(workspace.selected);
                                workspace.selected = workspace
                                    .selected
                                    .min(workspace.sessions.len().saturating_sub(1));
                            }
                        }
                        Confirm::StopEnvironment => self.env(index, EnvironmentOp::Down)?,
                        Confirm::RemoveWorktree => {
                            let workspace = self.focused();
                            if workspace.pending
                                || workspace
                                    .environment
                                    .as_ref()
                                    .is_some_and(|e| !e.services.is_empty())
                            {
                                self.set_notice(
                                    "Stop this environment before removing its checkout.",
                                );
                            } else {
                                self.jobs
                                    .try_send(Job::Remove {
                                        repository: self.git_repository()?.clone(),
                                        tree: workspace.tree.clone(),
                                    })
                                    .context("command queue is busy")?;
                                self.set_notice("Checking and removing worktree...");
                            }
                        }
                    }
                    retain = false;
                } else if matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N')) {
                    retain = false;
                }
            }
        }
        if retain {
            self.modal = Some(modal);
        }
        Ok(())
    }

    fn handle(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Input(crossterm::event::Event::Key(key)) => {
                if key.kind == KeyEventKind::Release {
                    return Ok(false);
                }
                let action = keys::action(key);
                if action == Some(Action::ToggleNative) {
                    if key.kind == KeyEventKind::Press {
                        self.action(Action::ToggleNative)?;
                    }
                } else if self.native_keys {
                    if let Some(session) = self.focused_session() {
                        session.key(key)?;
                    }
                } else if self.modal.is_some() {
                    self.modal_key(key)?;
                } else if let Some(action) = action {
                    self.notice = None;
                    self.action(action)?;
                } else if matches!(key.code, KeyCode::Char('d' | 'D'))
                    && key.modifiers == KeyModifiers::CONTROL
                    && self
                        .focused_session()
                        .is_some_and(|session| matches!(session.kind.as_str(), "omp" | "opencode"))
                {
                    self.notice = None;
                    self.select_shell()?;
                } else if let Some(session) = self.focused_session() {
                    session.key(key)?;
                }
            }
            Event::Input(crossterm::event::Event::Paste(text)) => match &mut self.modal {
                Some(Modal::Intent(dialog)) if !dialog.pending && !dialog.submitted => {
                    paste_input(&mut dialog.input, &text, 4096);
                }
                Some(Modal::NewWorktree { input }) => paste_input(input, &text, 256),
                Some(Modal::OpenProject(picker)) => picker.paste(&text),
                None => {
                    if let Some(session) = self.focused_session() {
                        session.paste(&text)?;
                    }
                }
                _ => {}
            },
            Event::Input(crossterm::event::Event::Mouse(mouse)) if self.modal.is_none() => {
                if let Some((_, index)) = self
                    .tab_hits
                    .iter()
                    .find(|(rect, _)| rect.contains((mouse.column, mouse.row).into()))
                {
                    if matches!(mouse.kind, crossterm::event::MouseEventKind::Down(_)) {
                        self.select_workspace(*index, None)?;
                    }
                } else if let Some(pane) = self
                    .panes
                    .iter()
                    .find(|p| p.body.contains((mouse.column, mouse.row).into()))
                {
                    let (index, body) = (pane.workspace, pane.body);
                    if matches!(mouse.kind, crossterm::event::MouseEventKind::Down(_)) {
                        self.project.focus_secondary = self.project.secondary == Some(index)
                            && self.project.layout != LayoutMode::Full;
                    }
                    let workspace = &self.project.workspaces[index];
                    if let Some(session) = workspace.sessions.get(workspace.selected) {
                        session.mouse(mouse, mouse.column - body.x, mouse.row - body.y)?;
                    }
                }
            }
            Event::Input(_) => {}
            Event::Dirty(id) => {
                return Ok(self.panes.iter().any(|p| {
                    self.project.workspaces[p.workspace]
                        .sessions
                        .get(self.project.workspaces[p.workspace].selected)
                        .is_some_and(|s| s.id == id)
                }));
            }
            Event::Exited { session, code } => {
                for workspace in self.all_workspaces_mut() {
                    if let Some(found) = workspace.sessions.iter_mut().find(|s| s.id == session) {
                        found.state = AgentState::Exited;
                        found.message = Some(code);
                        break;
                    }
                }
            }
            Event::Agent(update) => {
                for workspace in self.all_workspaces_mut() {
                    if let Some(session) = workspace
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == update.session)
                    {
                        if session.alive.load(Ordering::Acquire) {
                            session.state = update.state;
                            session.message = update.message;
                        }
                        break;
                    }
                }
            }
            Event::IntentTargets { request, result } => {
                if let Some(Modal::Intent(dialog)) = &mut self.modal {
                    if dialog.request != request {
                        return Ok(false);
                    }
                    dialog.loading = false;
                    match result.and_then(|targets| {
                        serde_json::to_value(&targets)
                            .map(|value| (targets, value))
                            .map_err(|error| error.to_string())
                    }) {
                        Ok((targets, value)) => {
                            dialog.targets = targets;
                            Arc::make_mut(&mut dialog.context)["targets"] = value;
                            if dialog.submitted {
                                Self::start_intent(dialog, &self.tx);
                            }
                        }
                        Err(error) => {
                            dialog.submitted = false;
                            dialog.stale = true;
                            dialog.error = Some(format!(
                                "Could not load destinations: {error}. F2 opens manual controls."
                            ));
                        }
                    }
                }
            }
            Event::IntentResolved { request, result } => {
                if !matches!(&self.modal, Some(Modal::Intent(dialog)) if dialog.request == request && dialog.pending)
                {
                    return Ok(false);
                }
                let Some(Modal::Intent(mut dialog)) = self.modal.take() else {
                    return Ok(false);
                };
                dialog.pending = false;
                if let Some(worker) = dialog.worker.take() {
                    let _ = worker.join();
                }
                if dialog.context["focused_worktree"]["id"].as_str()
                    != Some(self.focused().tree.id.as_str())
                {
                    dialog.stale = true;
                    dialog.error = Some(
                        "The focused workspace changed. Close and reopen Ask Crit before retrying."
                            .into(),
                    );
                    self.modal = Some(Modal::Intent(dialog));
                    return Ok(true);
                }
                match result {
                    Ok(Plan::Clarify { question }) => {
                        Arc::make_mut(&mut dialog.conversation).push(serde_json::json!({
                            "role": "assistant", "content": question,
                        }));
                        dialog.input.clear();
                        self.modal = Some(Modal::Intent(dialog));
                    }
                    Ok(plan) => {
                        if let Err(error) = self.execute_intent(plan, &dialog.targets) {
                            dialog.error = Some(format!("{error:#}"));
                            Arc::make_mut(&mut dialog.conversation).pop();
                            self.modal = Some(Modal::Intent(dialog));
                        }
                    }
                    Err(error) => {
                        Arc::make_mut(&mut dialog.conversation).pop();
                        dialog.error = Some(error);
                        self.modal = Some(Modal::Intent(dialog));
                    }
                }
            }
            Event::Projects { base, projects } => {
                if let Some(Modal::OpenProject(picker)) = &mut self.modal {
                    if picker.base == base {
                        picker.set_projects(projects);
                    }
                }
            }
            Event::ProjectOpened { result, agent } => match result {
                Ok(project) => self.open_project(project, agent)?,
                Err(error) => {
                    if let Some(Modal::OpenProject(picker)) = &mut self.modal {
                        picker.pending = false;
                    }
                    self.set_notice(error);
                }
            },
            Event::Created {
                project,
                result,
                agent,
            } => match result {
                Ok(tree) => {
                    let has_compose = compose::has_config(&tree);
                    if self.project.common_dir() != Some(project.as_path()) {
                        if let Some(saved) = self
                            .saved_projects
                            .iter_mut()
                            .find(|saved| saved.common_dir() == Some(project.as_path()))
                        {
                            let mut workspace = Workspace::new(tree);
                            if has_compose {
                                self.jobs
                                    .try_send(Job::Environment {
                                        tree: workspace.tree.clone(),
                                        operation: EnvironmentOp::Up,
                                    })
                                    .context("command queue is busy")?;
                                workspace.pending = true;
                            }
                            saved.workspaces.push(workspace);
                        }
                        return Ok(false);
                    }
                    if let Some(index) = self
                        .project
                        .workspaces
                        .iter()
                        .position(|w| w.tree.id == tree.id)
                    {
                        self.select_workspace(index, agent)?;
                    } else {
                        self.project.workspaces.push(Workspace::new(tree));
                        let index = self.project.workspaces.len() - 1;
                        // Creation owns the initial start; do not enqueue a redundant status query.
                        if has_compose {
                            self.env(index, EnvironmentOp::Up)?;
                        }
                        self.select_workspace(index, agent)?;
                    }
                    self.set_notice(if has_compose {
                        "Worktree created; starting its isolated Docker environment."
                    } else {
                        "Worktree created. No Compose config found; terminal is ready."
                    });
                }
                Err(error) => self.set_notice(error),
            },
            Event::Environment { tree, result } => {
                let active = self.project.workspaces.iter().any(|w| w.tree.id == tree);
                let error = if let Some(workspace) =
                    self.all_workspaces_mut().find(|w| w.tree.id == tree)
                {
                    workspace.pending = false;
                    match result {
                        Ok(environment) => {
                            workspace.environment = Some(environment);
                            None
                        }
                        Err(error) => Some(error),
                    }
                } else {
                    None
                };
                if let Some(error) = error {
                    self.set_notice(error);
                } else if active {
                    self.notice = None;
                }
            }
            Event::Worktrees { project, result } => {
                if self.project.common_dir() == Some(project.as_path()) {
                    match result {
                        Ok(trees) => self.modal = Some(Modal::OpenWorktree { trees, selected: 0 }),
                        Err(error) => self.set_notice(error),
                    }
                }
            }
            Event::Removed { tree, result } => match result {
                Ok(()) => {
                    if let Some(index) = self
                        .project
                        .workspaces
                        .iter()
                        .position(|w| w.tree.id == tree)
                    {
                        self.project.layout = LayoutMode::Full;
                        self.project.focus_secondary = false;
                        self.project.active = index;
                        self.close_workspace();
                        if !self.should_quit {
                            self.ensure_session(self.project.active)?;
                        }
                    } else if let Some((project, index)) = self
                        .saved_projects
                        .iter()
                        .enumerate()
                        .find_map(|(project, saved)| {
                            saved
                                .workspaces
                                .iter()
                                .position(|w| w.tree.id == tree)
                                .map(|index| (project, index))
                        })
                    {
                        if self.saved_projects[project].workspaces.len() == 1 {
                            self.saved_projects.remove(project);
                        } else {
                            self.saved_projects[project].close_workspace(index);
                        }
                    }
                    self.set_notice("Checkout removed; Git branch and Docker volumes retained.");
                }
                Err(error) => self.set_notice(error),
            },
            Event::Error(error) => self.set_notice(error),
            Event::Shutdown => self.should_quit = true,
        }
        Ok(true)
    }
}

fn project_path(input: &str, base: &Path) -> Result<PathBuf> {
    if input == "~" || input.starts_with("~/") {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(input.strip_prefix("~/").unwrap_or("")))
    } else {
        Ok(base.join(input))
    }
}

fn edit_input(key: KeyEvent, input: &mut String, limit: usize) {
    match key.code {
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => input.clear(),
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && !c.is_control()
                && input.len() + c.len_utf8() <= limit =>
        {
            input.push(c)
        }
        KeyCode::Backspace => {
            input.pop();
        }
        _ => {}
    }
}

fn paste_input(input: &mut String, text: &str, limit: usize) {
    for c in text.chars().filter(|c| !c.is_control()) {
        if input.len() + c.len_utf8() > limit {
            break;
        }
        input.push(c);
    }
}

fn navigate(key: KeyEvent, selected: &mut usize, count: usize) {
    if count == 0 {
        return;
    }
    match key.code {
        KeyCode::Down | KeyCode::Tab => *selected = (*selected + 1) % count,
        KeyCode::Up | KeyCode::BackTab => *selected = (*selected + count - 1) % count,
        KeyCode::Home => *selected = 0,
        KeyCode::End => *selected = count - 1,
        _ => {}
    }
}

fn intent_targets(
    base: &Path,
    repository: Option<&worktree::Repository>,
    known_projects: &[PathBuf],
) -> Result<Vec<IntentTarget>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut projects = worktree::projects(base, home.as_deref());
    projects.extend_from_slice(known_projects);
    projects.sort();
    projects.dedup();
    let trees = repository
        .map(worktree::list)
        .transpose()?
        .unwrap_or_default();
    let mut targets = Vec::with_capacity(projects.len() + trees.len());
    for path in projects {
        targets.push(IntentTarget {
            id: targets.len(),
            kind: "project",
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            path,
            branch: None,
        });
    }
    for tree in trees {
        targets.push(IntentTarget {
            id: targets.len(),
            kind: "worktree",
            name: tree.branch.clone(),
            path: tree.path,
            branch: Some(tree.branch),
        });
    }
    Ok(targets)
}

fn start_worker(tx: SyncSender<Event>) -> Result<SyncSender<Job>> {
    let (jobs, rx) = mpsc::sync_channel(16);
    std::thread::Builder::new()
        .name("commands".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                let event = match job {
                    Job::Projects(base) => {
                        let home = std::env::var_os("HOME").map(PathBuf::from);
                        let projects = worktree::projects(&base, home.as_deref());
                        Event::Projects { base, projects }
                    }
                    Job::IntentTargets {
                        request,
                        base,
                        repository,
                        known_projects,
                    } => Event::IntentTargets {
                        request,
                        result: intent_targets(&base, repository.as_ref(), &known_projects)
                            .map_err(|error| format!("{error:#}")),
                    },
                    Job::OpenProject { path, agent } => Event::ProjectOpened {
                        result: Project::load(&path).map_err(|e| format!("{e:#}")),
                        agent,
                    },
                    Job::Create {
                        repository,
                        branch,
                        base,
                        agent,
                    } => Event::Created {
                        result: worktree::create(&repository, &branch, base.as_deref())
                            .map_err(|e| format!("{e:#}")),
                        project: repository.common_dir,
                        agent,
                    },
                    Job::List(repository) => Event::Worktrees {
                        result: worktree::list(&repository).map_err(|e| format!("{e:#}")),
                        project: repository.common_dir,
                    },
                    Job::Remove { repository, tree } => {
                        let result = (|| -> Result<()> {
                            if compose::has_config(&tree)
                                && !compose::status(&tree)?.services.is_empty()
                            {
                                anyhow::bail!(
                                    "Stop this worktree's Docker environment before removing it"
                                );
                            }
                            worktree::remove(&repository, &tree)
                        })();
                        Event::Removed {
                            tree: tree.id,
                            result: result.map_err(|e| format!("{e:#}")),
                        }
                    }
                    Job::Environment { tree, operation } => {
                        let result = match operation {
                            EnvironmentOp::Up => compose::up(&tree),
                            EnvironmentOp::Down => compose::down(&tree),
                            EnvironmentOp::Restart => compose::restart(&tree),
                            EnvironmentOp::Status => compose::status(&tree),
                        };
                        Event::Environment {
                            tree: tree.id,
                            result: result.map_err(|e| format!("{e:#}")),
                        }
                    }
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
        })?;
    Ok(jobs)
}

struct TerminalGuard {
    use_kitty: bool,
}
impl TerminalGuard {
    fn enter(use_kitty: bool) -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let guard = Self { use_kitty };
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        if use_kitty {
            kitty::enter(&mut io::stdout())?;
        }
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.use_kitty {
            let _ = kitty::leave(&mut io::stdout());
        }
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = io::stdout().flush();
    }
}

fn choose_project(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    rx: &mpsc::Receiver<Event>,
    pending: &mut VecDeque<Event>,
    jobs: &SyncSender<Job>,
    path: &Path,
    error: Option<anyhow::Error>,
    update_notice: Option<&str>,
    use_kitty: bool,
) -> Result<Option<(Project, bool)>> {
    let launch_path = std::env::current_dir()?;
    let base = if path.is_dir() {
        path.to_path_buf()
    } else {
        launch_path.clone()
    };
    let mut notice = if let Some(error) = error {
        format!("{error:#}")
    } else if let Some(notice) = update_notice {
        notice.to_owned()
    } else {
        "Choose New terminal for a shell here, or open a Git project.".into()
    };
    let mut picker = ProjectPicker::new(base, jobs)?;
    picker.terminal_path = Some(launch_path.clone());
    let mut native_keys = false;
    loop {
        terminal.draw(|frame| {
            ui::draw_project_chooser(frame, &picker, &notice, native_keys);
        })?;
        match pending.pop_front().map_or_else(|| rx.recv(), Ok)? {
            Event::Input(crossterm::event::Event::Key(key))
                if key.kind != KeyEventKind::Release =>
            {
                match key.code {
                    KeyCode::Esc | KeyCode::F(10) => return Ok(None),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None);
                    }
                    KeyCode::F(12) if key.modifiers.is_empty() => {
                        if key.kind == KeyEventKind::Press {
                            native_keys = !native_keys;
                            if use_kitty {
                                kitty::set_native(&mut io::stdout(), native_keys)?;
                            }
                        }
                    }
                    _ => match picker.key(key, jobs) {
                        Ok(true) => {
                            return Ok(Some((Project::terminal(&launch_path)?, native_keys)));
                        }
                        Ok(false) => {}
                        Err(error) => notice = format!("{error:#}"),
                    },
                }
            }
            Event::Input(crossterm::event::Event::Paste(text)) => picker.paste(&text),
            Event::Projects { base, projects } if picker.base == base => {
                picker.set_projects(projects);
            }
            Event::ProjectOpened {
                result: Ok(project),
                ..
            } => return Ok(Some((project, native_keys))),
            Event::ProjectOpened {
                result: Err(error), ..
            } => {
                picker.pending = false;
                notice = error;
            }
            Event::Error(error) => notice = error,
            Event::Shutdown => return Ok(None),
            _ => {}
        }
    }
}

pub fn run(path: &Path, agent: &str, use_kitty: bool, direct_start: bool) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!("Crit needs an interactive terminal; use --help for noninteractive commands");
    }
    let (tx, rx) = mpsc::sync_channel(128);
    let jobs = start_worker(tx.clone())?;
    let input_tx = tx.clone();
    let signal_tx = tx.clone();
    let mut signals =
        signal_hook::iterator::Signals::new([libc::SIGTERM, libc::SIGHUP, libc::SIGINT])?;
    let signal_handle = signals.handle();
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                let _ = signal_tx.send(Event::Shutdown);
            }
        })?;
    let _guard = TerminalGuard::enter(use_kitty)?;
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            loop {
                match crossterm::event::read() {
                    Ok(event) => {
                        if input_tx.send(Event::Input(event)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = input_tx.send(Event::Error(format!("terminal input: {error}")));
                        let _ = input_tx.send(Event::Shutdown);
                        break;
                    }
                }
            }
        })?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let startup = updates::run(&mut terminal, &rx);
    let updates::Startup {
        notice: update_notice,
        mut pending,
    } = match startup {
        Ok(Some(startup)) => startup,
        result => {
            signal_handle.close();
            return result.map(|_| ());
        }
    };
    let selection = match direct_start.then(|| Project::load(path)) {
        Some(Ok(project)) => Ok(Some((project, false))),
        result => choose_project(
            &mut terminal,
            &rx,
            &mut pending,
            &jobs,
            path,
            result.and_then(Result::err),
            update_notice.as_deref(),
            use_kitty,
        ),
    };
    let (project, native_keys) = match selection {
        Ok(Some(selection)) => selection,
        result => {
            signal_handle.close();
            return result.map(|_| ());
        }
    };
    let active = project.active;
    let server = integration::start(tx.clone())?;
    let mut app = App {
        project,
        saved_projects: Vec::new(),
        native_keys,
        modal: None,
        notice: update_notice,
        panes: Vec::with_capacity(2),
        tab_hits: Vec::new(),
        should_quit: false,
        default_agent: agent.into(),
        next_session: 1,
        next_intent: 1,
        socket: server.socket_path.clone(),
        tx: tx.clone(),
        jobs,
        use_kitty,
    };
    if let Err(error) = app.ensure_session(active) {
        app.set_notice(format!(
            "{error:#}. F1 → ask to start another agent or shell."
        ));
    }
    if app.project.repository.is_some() && compose::has_config(&app.project.workspaces[active].tree)
    {
        app.env(active, EnvironmentOp::Status)?;
    }
    let frame_interval = Duration::from_millis(16);
    let mut last_draw = Instant::now() - frame_interval;
    let mut redraw = true;
    while !app.should_quit {
        if redraw && last_draw.elapsed() >= frame_interval {
            ui::resize_sessions(&mut app, terminal.size()?)?;
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            redraw = false;
            last_draw = Instant::now();
        }
        let event = if let Some(event) = pending.pop_front() {
            Some(event)
        } else if redraw {
            match rx.recv_timeout(frame_interval.saturating_sub(last_draw.elapsed())) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            }
        } else {
            match rx.recv() {
                Ok(event) => Some(event),
                Err(_) => break,
            }
        };
        if let Some(event) = event {
            match app.handle(event) {
                Ok(changed) => redraw |= changed,
                Err(error) => {
                    app.set_notice(format!("{error:#}"));
                    redraw = true;
                }
            }
            for _ in 0..127 {
                let Some(event) = pending.pop_front().or_else(|| rx.try_recv().ok()) else {
                    break;
                };
                match app.handle(event) {
                    Ok(changed) => redraw |= changed,
                    Err(error) => {
                        app.set_notice(format!("{error:#}"));
                        redraw = true;
                    }
                }
            }
        }
    }
    signal_handle.close();
    drop(app);
    drop(server);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(base: &Path) -> (ProjectPicker, SyncSender<Job>, mpsc::Receiver<Job>) {
        let (tx, rx) = mpsc::sync_channel(16);
        let picker = ProjectPicker::new(base.to_path_buf(), &tx).unwrap();
        rx.recv().unwrap();
        (picker, tx, rx)
    }

    #[test]
    fn startup_terminal_is_visible_selected_and_does_not_queue_a_project() {
        let directory = tempfile::tempdir().unwrap();
        let (mut picker, tx, rx) = picker(directory.path());
        picker.terminal_path = Some(directory.path().to_path_buf());
        let project = directory.path().join("project");
        picker.set_projects(vec![project.clone()]);
        assert!(picker.shows_terminal());
        assert!(
            picker
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx)
                .unwrap()
        );
        assert!(!picker.pending);
        assert!(rx.try_recv().is_err());

        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| ui::draw_project_chooser(frame, &picker, "", false))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("New terminal"));
        assert!(text.contains("project"));

        picker
            .key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx)
            .unwrap();
        assert!(
            !picker
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx)
                .unwrap()
        );
        assert!(matches!(rx.recv().unwrap(), Job::OpenProject { path, .. } if path == project));
    }

    #[test]
    fn terminal_projects_have_no_git_metadata_and_distinct_folder_identity() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let project = Project::terminal(first.path()).unwrap();
        assert!(project.repository.is_none());
        assert_eq!(project.path(), first.path().canonicalize().unwrap());
        assert!(project.workspaces[0].tree.branch.is_empty());
        assert!(project.workspaces[0].tree.head.is_empty());
        assert!(!first.path().join(".git").exists());
        assert!(project.same_project(&Project::terminal(first.path()).unwrap()));
        assert!(!project.same_project(&Project::terminal(second.path()).unwrap()));
    }

    #[test]
    fn terminal_workspace_guards_git_actions_and_keeps_launcher_available() {
        let directory = tempfile::tempdir().unwrap();
        let (tx, _events) = mpsc::sync_channel(8);
        let (jobs, queued) = mpsc::sync_channel(8);
        let mut app = App {
            project: Project::terminal(directory.path()).unwrap(),
            saved_projects: Vec::new(),
            native_keys: false,
            modal: None,
            notice: None,
            panes: Vec::new(),
            tab_hits: Vec::new(),
            should_quit: false,
            default_agent: "omp".into(),
            next_session: 1,
            next_intent: 1,
            socket: directory.path().join("agent.sock"),
            tx,
            jobs,
            use_kitty: false,
        };
        for command in [
            IntentCommand::OpenWorktree,
            IntentCommand::NewWorktree,
            IntentCommand::StartEnvironment,
            IntentCommand::StopEnvironment,
            IntentCommand::RestartEnvironment,
            IntentCommand::RefreshEnvironment,
            IntentCommand::EnvironmentLogs,
            IntentCommand::Preview,
            IntentCommand::RemoveWorktree,
        ] {
            assert!(
                app.run_command(command)
                    .unwrap_err()
                    .to_string()
                    .contains("Open a Git project")
            );
        }
        assert!(app.action(Action::NewWorktree).is_err());
        assert!(app.env(0, EnvironmentOp::Status).is_err());
        assert!(
            app.execute_intent(
                Plan::CreateWorktree {
                    branch: "feature".into(),
                    base: None,
                    agent: None,
                },
                &[]
            )
            .is_err()
        );
        assert!(queued.try_recv().is_err());
        assert!(app.notice.is_none());
        assert!(!directory.path().join(".git").exists());

        app.open_intent().unwrap();
        assert!(matches!(
            queued.recv().unwrap(),
            Job::IntentTargets {
                repository: None,
                ..
            }
        ));
        let Some(Modal::Intent(dialog)) = app.modal.as_ref() else {
            panic!("expected launcher");
        };
        assert_eq!(dialog.context["active_project"]["is_git"], false);
        assert!(dialog.context["active_project"]["common_dir"].is_null());
        assert!(dialog.context["focused_worktree"]["branch"].is_null());
        app.run_command(IntentCommand::OpenProject).unwrap();
        assert!(matches!(app.modal, Some(Modal::OpenProject(_))));
        assert!(matches!(queued.recv().unwrap(), Job::Projects(_)));
    }

    #[test]
    fn intent_results_require_the_live_request_and_workspace_and_keep_confirmations() {
        let (tx, _events) = mpsc::sync_channel(8);
        let (jobs, queued) = mpsc::sync_channel(8);
        let root = PathBuf::from("/intent-fixture");
        let mut app = App {
            project: Project {
                repository: Some(worktree::Repository {
                    root: root.clone(),
                    common_dir: root.join(".git"),
                }),
                name: "fixture".into(),
                workspaces: ["main", "other"]
                    .into_iter()
                    .map(|branch| {
                        Workspace::new(Worktree {
                            id: branch.into(),
                            path: root.join(branch),
                            branch: branch.into(),
                            head: String::new(),
                            is_main: branch == "main",
                            locked: false,
                        })
                    })
                    .collect(),
                active: 0,
                secondary: None,
                focus_secondary: false,
                layout: LayoutMode::Full,
            },
            saved_projects: Vec::new(),
            native_keys: false,
            modal: None,
            notice: None,
            panes: Vec::new(),
            tab_hits: Vec::new(),
            should_quit: false,
            default_agent: "shell".into(),
            next_session: 1,
            next_intent: 1,
            socket: root.join("agent.sock"),
            tx,
            jobs,
            use_kitty: false,
        };
        let dialog = |request, id: &str| {
            Modal::Intent(IntentDialog {
                input: "Create a branch named login-fix".into(),
                error: None,
                loading: false,
                pending: true,
                stale: false,
                worker: None,
                request,
                submitted: false,
                targets: Vec::new(),
                context: Arc::new(
                    serde_json::json!({"focused_worktree": {"id": id}, "targets": []}),
                ),
                conversation: Arc::new(Vec::new()),
                cancel: Arc::new(AtomicBool::new(false)),
            })
        };
        let create = |request| Event::IntentResolved {
            request,
            result: Ok(Plan::CreateWorktree {
                branch: "login-fix".into(),
                base: None,
                agent: None,
            }),
        };
        app.modal = Some(dialog(2, "main"));
        assert!(!app.handle(create(1)).unwrap());
        assert!(
            matches!(&app.modal, Some(Modal::Intent(dialog)) if dialog.request == 2 && dialog.pending)
        );
        assert!(queued.try_recv().is_err());

        // A background operation may change focus while the model is answering.
        app.project.active = 1;
        assert!(app.handle(create(2)).unwrap());
        assert!(
            matches!(&app.modal, Some(Modal::Intent(dialog)) if dialog.stale && !dialog.pending)
        );
        assert!(queued.try_recv().is_err());

        app.modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(!app.handle(create(2)).unwrap());
        assert!(app.modal.is_none());
        assert!(queued.try_recv().is_err());

        app.modal = Some(dialog(3, "other"));
        app.handle(Event::IntentResolved {
            request: 3,
            result: Ok(Plan::Command {
                command: IntentCommand::Quit,
            }),
        })
        .unwrap();
        assert!(matches!(app.modal, Some(Modal::Confirm(Confirm::Quit))));
        assert!(!app.should_quit);
    }

    #[test]
    fn filtering_navigates_and_opens_the_highlighted_match() {
        let directory = tempfile::tempdir().unwrap();
        // A query can also name a local folder; the highlighted match must still win.
        std::fs::create_dir(directory.path().join("LiMA")).unwrap();
        let (mut picker, tx, rx) = picker(directory.path());
        let paths: Vec<_> = ["jury-validation", "lima-client", "lima-guard", "prompts"]
            .into_iter()
            .map(|name| directory.path().join("repos").join(name))
            .collect();
        picker.set_projects(paths.clone());
        picker
            .key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &tx)
            .unwrap();
        for character in "LiMA".chars() {
            picker
                .key(
                    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                    &tx,
                )
                .unwrap();
        }
        assert_eq!(
            picker.visible_projects().collect::<Vec<_>>(),
            vec![&paths[1], &paths[2]]
        );
        assert_eq!(picker.selected, 0);
        picker
            .key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx)
            .unwrap();
        assert_eq!(picker.input, "LiMA");
        picker
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx)
            .unwrap();
        let Job::OpenProject { path, .. } = rx.recv().unwrap() else {
            panic!("expected project opening");
        };
        assert_eq!(path, paths[2]);
    }

    #[test]
    fn paste_edits_and_late_discovery_keep_the_filtered_selection_valid() {
        let directory = tempfile::tempdir().unwrap();
        let (mut picker, tx, rx) = picker(directory.path());
        let paths: Vec<_> = ["café-api", "café-ui", "unrelated"]
            .into_iter()
            .map(|name| directory.path().join(name))
            .collect();
        picker.paste("CAFÉ");
        picker.set_projects(paths.clone());
        assert_eq!(
            picker.visible_projects().collect::<Vec<_>>(),
            vec![&paths[0], &paths[1]]
        );
        picker
            .key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx)
            .unwrap();
        picker.paste("-ui");
        assert_eq!(
            picker.visible_projects().collect::<Vec<_>>(),
            vec![&paths[1]]
        );
        assert_eq!(picker.selected, 0);
        for _ in 0..3 {
            picker
                .key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &tx)
                .unwrap();
        }
        assert_eq!(
            picker.visible_projects().collect::<Vec<_>>(),
            vec![&paths[0], &paths[1]]
        );
        picker
            .key(
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
                &tx,
            )
            .unwrap();
        assert_eq!(
            picker.visible_projects().collect::<Vec<_>>(),
            paths.iter().collect::<Vec<_>>()
        );
        picker.paste("no-such-project");
        assert_eq!(
            picker.visible_projects().collect::<Vec<_>>(),
            Vec::<&PathBuf>::new()
        );
        picker
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx)
            .unwrap();
        assert!(!picker.pending);
        assert!(
            rx.try_recv().is_err(),
            "an empty result set must not open a stale selection"
        );
    }

    #[test]
    fn explicit_folder_paths_take_precedence_over_partial_matches() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("project");
        std::fs::create_dir(&target).unwrap();
        let (mut picker, tx, rx) = picker(directory.path());
        picker.set_projects(vec![directory.path().join("project-other")]);
        picker.paste(target.to_str().unwrap());
        picker
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx)
            .unwrap();
        let Job::OpenProject { path, .. } = rx.recv().unwrap() else {
            panic!("expected project opening");
        };
        assert_eq!(path, target);
    }
}
