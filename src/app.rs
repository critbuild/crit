use crate::{
    clipboard, compose,
    event::Event,
    integration::{self, AgentState},
    intent::{self, Agent as AgentKind, Command as IntentCommand, Plan},
    keys::{self, Action},
    kitty, restore,
    selection::Selection,
    terminal::Session,
    ui, updates,
    worktree::{self, Worktree},
};
use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
        MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use std::{
    collections::VecDeque,
    fmt::Write as _,
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
    request: u64,
    search_text: Vec<String>,
    matches: Vec<usize>,
}
impl ProjectPicker {
    fn new(base: PathBuf, jobs: &SyncSender<Job>, request: u64) -> Result<Self> {
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
            request,
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
                        || input.contains('/');
                    let path = project_path(input, &self.base)?;
                    if explicit {
                        Some(path)
                    } else if let Some(selected) = selected {
                        Some(selected.clone())
                    } else if path.is_dir() {
                        Some(path)
                    } else {
                        anyhow::bail!(
                            "Folder not found: {}. Enter an existing path, or ~ for home.",
                            path.display()
                        );
                    }
                };
                if let Some(path) = path {
                    jobs.try_send(Job::OpenProject {
                        request: self.request,
                        path,
                        agent: None,
                    })
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
    NewTab {
        choices: Vec<(String, IntentCommand)>,
        selected: usize,
    },
    OpenProject(ProjectPicker),
    Loading {
        request: u64,
        message: &'static str,
        retry: Option<IntentDialog>,
    },
    SwitchProject {
        selected: usize,
    },
    NewWorktree {
        input: String,
        /// When set, the created worktree opens as the second pane of this layout.
        split: Option<LayoutMode>,
    },
    NewSession {
        selected: usize,
    },
    OpenWorktree {
        trees: Vec<Worktree>,
        selected: usize,
    },
    /// `selected == 0` is "new worktree"; `selected - 1` indexes `workspaces`.
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
    pub source: Option<PathBuf>,
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub environment: Option<compose::Environment>,
    pub pending: bool,
}
impl Workspace {
    fn new(tree: Worktree) -> Self {
        Self {
            tree,
            source: None,
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
    pub scrollbar: Option<Scrollbar>,
}

#[derive(Clone, Copy)]
pub struct Scrollbar {
    pub track: Rect,
    pub thumb: Rect,
    pub max_offset: usize,
}

impl Scrollbar {
    fn offset_at(self, row: u16, grab: u16) -> usize {
        let travel = self.track.height.saturating_sub(self.thumb.height);
        let position = row
            .saturating_sub(self.track.y)
            .saturating_sub(grab)
            .min(travel);
        self.max_offset - self.max_offset * usize::from(position) / usize::from(travel.max(1))
    }
}

/// A clickable cell in the tab bar: a project name (`workspace: None`) or one
/// of its worktrees. `project: None` is the current project; `Some(i)` indexes
/// `App::saved_projects`.
pub struct TabHit {
    pub area: Rect,
    pub project: Option<usize>,
    pub workspace: Option<usize>,
}

pub struct Project {
    pub repository: Option<worktree::Repository>,
    pub name: String,
    pub workspaces: Vec<Workspace>,
    pub active: usize,
    pub secondary: Option<usize>,
    pub focus_secondary: bool,
    pub layout: LayoutMode,
    /// Opening sequence; the tab bar lists projects in this order regardless
    /// of which one is current. `App::saved_projects` stays sorted by it.
    pub order: u64,
}
impl Project {
    /// Any folder: a Git checkout when present, otherwise an ordinary workspace.
    fn open(path: &Path) -> Result<Self> {
        let path = folder(path)?;
        match worktree::discover_optional(&path)? {
            Some(repository) => Self::from_repository(path, repository),
            None => Self::terminal(&path),
        }
    }

    fn from_repository(path: PathBuf, repository: worktree::Repository) -> Result<Self> {
        let mut trees = worktree::list(&repository)?;
        let active = trees
            .iter()
            .enumerate()
            .filter(|(_, tree)| path.starts_with(&tree.path))
            .max_by_key(|(_, tree)| tree.path.components().count())
            .map(|(index, _)| index)
            .context("folder is not inside a checked-out worktree")?;
        let name = folder_name(&trees[0].path);
        let tree = trees.swap_remove(active);
        Ok(Self {
            repository: Some(repository),
            name,
            workspaces: vec![Workspace::new(tree)],
            active: 0,
            secondary: None,
            focus_secondary: false,
            layout: LayoutMode::Full,
            order: 0,
        })
    }

    fn terminal(path: &Path) -> Result<Self> {
        let path = folder(path)?;
        let mut name = folder_name(&path);
        if name.is_empty() {
            name.push_str("Terminal");
        }
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
            name,
            workspaces: vec![Workspace::new(tree)],
            active: 0,
            secondary: None,
            focus_secondary: false,
            layout: LayoutMode::Full,
            order: 0,
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
    scroll_drag: Option<(u64, u16)>,
    pub selection: Option<Selection>,
    pub tab_hits: Vec<TabHit>,
    pub interaction_visible: bool,
    pub should_quit: bool,
    default_agent: String,
    next_session: u64,
    next_request: u64,
    pending_creations: VecDeque<(PathBuf, String, Modal)>,
    state_dirty: bool,
    socket: PathBuf,
    tx: SyncSender<Event>,
    jobs: SyncSender<Job>,
    use_kitty: bool,
}

enum Job {
    OpenProject {
        request: u64,
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
        split: Option<LayoutMode>,
    },
    List {
        request: u64,
        repository: worktree::Repository,
    },
    Environment {
        tree: Worktree,
        operation: EnvironmentOp,
        source: Option<PathBuf>,
    },
    Remove {
        repository: worktree::Repository,
        tree: Worktree,
    },
}
#[derive(Clone, Copy)]
pub enum EnvironmentOp {
    Up,
    Down,
    Restart,
    /// Explicit refresh: the outcome is reported in the notice line.
    Status,
    /// Background check on open: fills `Workspace::environment` silently.
    Probe,
}

/// Open projects in opening order, pairing each with its `saved_projects`
/// index (`None` for the current project). Relies on `saved` being sorted.
pub fn project_slots<'a>(
    current: &'a Project,
    saved: &'a [Project],
) -> impl Iterator<Item = (Option<usize>, &'a Project)> {
    let position = saved.partition_point(|project| project.order < current.order);
    (0..=saved.len()).map(move |slot| match slot.cmp(&position) {
        std::cmp::Ordering::Less => (Some(slot), &saved[slot]),
        std::cmp::Ordering::Equal => (None, current),
        std::cmp::Ordering::Greater => (Some(slot - 1), &saved[slot - 1]),
    })
}

/// One-line Compose status for the notice row, e.g. `main: + web :8080 · ! db`.
fn environment_summary(label: &str, environment: &compose::Environment) -> String {
    let mut summary = format!("{label}: ");
    if environment.services.is_empty() {
        summary.push_str("stack stopped");
    }
    for (index, service) in environment.services.iter().enumerate() {
        let healthy = service.state == "running" && service.health != "unhealthy";
        if index > 0 {
            summary.push_str(" · ");
        }
        summary.push_str(if healthy { "+ " } else { "! " });
        summary.push_str(&service.name);
        for port in &service.ports {
            let _ = write!(summary, " :{}", port.host);
        }
    }
    summary
}

impl App {
    fn workspace_changed(&mut self) {
        // Hit targets belong to the last frame, not the newly selected topology.
        self.panes.clear();
        self.scroll_drag = None;
        self.selection = None;
        self.tab_hits.clear();
        self.state_dirty = true;
    }

    fn open_link(&self, uri: String) -> Result<()> {
        anyhow::ensure!(
            ["https://", "http://", "mailto:", "file://"]
                .iter()
                .any(|scheme| uri.starts_with(scheme))
                && !uri.chars().any(char::is_control),
            "Unsupported or invalid link target"
        );
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("open-link".into())
            .spawn(move || {
                let result = std::process::Command::new("xdg-open")
                    .arg(&uri)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                let error = match result {
                    Ok(status) if status.success() => return,
                    Ok(status) => format!("Opening link failed ({status}): {uri}"),
                    Err(error) => format!("Could not open link with xdg-open: {error}"),
                };
                let _ = tx.send(Event::Error(error));
            })?;
        Ok(())
    }

    fn snapshot(&self) -> restore::Snapshot {
        let mut active_project = 0;
        let projects = project_slots(&self.project, &self.saved_projects)
            .filter(|(_, project)| !project.workspaces.is_empty())
            .enumerate()
            .map(|(index, (slot, project))| {
                if slot.is_none() {
                    active_project = index;
                }
                restore::SavedProject {
                    workspaces: project
                        .workspaces
                        .iter()
                        .map(|workspace| {
                            let mut selected = 0;
                            let sessions = workspace
                                .sessions
                                .iter()
                                .enumerate()
                                .filter(|(_, session)| {
                                    matches!(session.kind.as_str(), "omp" | "opencode" | "shell")
                                })
                                .enumerate()
                                .map(|(saved_index, (index, session))| {
                                    if index == workspace.selected {
                                        selected = saved_index;
                                    }
                                    restore::SavedSession {
                                        kind: session.kind.clone(),
                                        resume: session.resume.clone(),
                                    }
                                })
                                .collect();
                            restore::SavedWorkspace {
                                path: workspace.tree.path.clone(),
                                source: workspace.source.clone(),
                                sessions,
                                selected,
                            }
                        })
                        .collect(),
                    active: project.active,
                    secondary: project.secondary,
                    focus_secondary: project.focus_secondary,
                    layout: match project.layout {
                        LayoutMode::Full => restore::SavedLayout::Full,
                        LayoutMode::Vertical => restore::SavedLayout::Vertical,
                        LayoutMode::Horizontal => restore::SavedLayout::Horizontal,
                    },
                }
            })
            .collect();
        restore::Snapshot {
            version: restore::VERSION,
            projects,
            active_project,
            native_keys: self.native_keys,
        }
    }

    fn checkpoint(&mut self, store: &mut Option<restore::Store>) -> bool {
        if !self.state_dirty {
            return false;
        }
        self.state_dirty = false;
        let Some(owner) = store.as_ref() else {
            return false;
        };
        if let Err(error) = owner.save(&self.snapshot()) {
            self.set_notice(format!(
                "Workspace could not be saved: {error:#}. Saving disabled for this window."
            ));
            *store = None;
            return true;
        }
        false
    }

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

    /// Worktree tabs across all open projects, in tab-bar order.
    fn tabs(&self) -> impl Iterator<Item = (Option<usize>, usize)> + '_ {
        project_slots(&self.project, &self.saved_projects)
            .flat_map(|(slot, project)| (0..project.workspaces.len()).map(move |w| (slot, w)))
    }

    fn swap_project(&mut self, index: usize) {
        self.workspace_changed();
        std::mem::swap(&mut self.project, &mut self.saved_projects[index]);
        self.saved_projects
            .sort_unstable_by_key(|project| project.order);
    }

    fn switch_project(&mut self, index: usize, workspace: Option<usize>) -> Result<()> {
        self.swap_project(index);
        self.modal = None;
        self.notice = None;
        let workspace = workspace.unwrap_or_else(|| self.focused_index());
        self.select_workspace(workspace, None)
    }

    fn focus_tab(&mut self, slot: Option<usize>, workspace: usize) -> Result<()> {
        match slot {
            Some(index) => self.switch_project(index, Some(workspace)),
            None => self.select_workspace(workspace, None),
        }
    }

    fn open_project(&mut self, mut project: Project, agent: Option<AgentKind>) -> Result<()> {
        self.workspace_changed();
        if !self.project.same_project(&project) {
            if let Some(index) = self
                .saved_projects
                .iter()
                .position(|saved| saved.same_project(&project))
            {
                self.swap_project(index);
            } else {
                project.order = self
                    .saved_projects
                    .last()
                    .map_or(0, |saved| saved.order)
                    .max(self.project.order)
                    + 1;
                let previous = std::mem::replace(&mut self.project, project);
                self.saved_projects.push(previous);
                self.saved_projects
                    .sort_unstable_by_key(|project| project.order);
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
            None,
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
        self.state_dirty = true;
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
            self.state_dirty = true;
            Ok(())
        } else {
            self.spawn(index, "shell", None)
        }
    }
    fn ensure_session(&mut self, index: usize) -> Result<()> {
        if self.project.workspaces[index].sessions.is_empty() {
            let kind = self.default_agent.clone();
            self.spawn(index, &kind, None)?;
        }
        Ok(())
    }
    fn select_workspace(&mut self, index: usize, agent: Option<AgentKind>) -> Result<()> {
        if index >= self.project.workspaces.len() {
            return Ok(());
        }
        self.workspace_changed();
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
            self.env(index, EnvironmentOp::Probe)?;
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
            .try_send(Job::Environment {
                tree,
                operation,
                source: self.project.workspaces[index].source.clone(),
            })
            .context("command queue is busy")?;
        self.project.workspaces[index].pending = true;
        Ok(())
    }
    fn full_width(&mut self) {
        self.workspace_changed();
        self.project.active = self.focused_index();
        self.project.layout = LayoutMode::Full;
        self.project.secondary = None;
        self.project.focus_secondary = false;
    }

    fn split(&mut self, layout: LayoutMode) -> Result<()> {
        if self.project.layout == layout {
            self.full_width();
            return Ok(());
        }
        self.git_repository()?;
        self.modal = Some(if self.project.workspaces.len() < 2 {
            Modal::NewWorktree {
                input: String::new(),
                split: Some(layout),
            }
        } else {
            Modal::Split {
                layout,
                selected: 0,
            }
        });
        Ok(())
    }
    /// Shows `index` next to the focused worktree; `focus_new` moves focus into it.
    fn split_with(&mut self, layout: LayoutMode, index: usize, focus_new: bool) -> Result<()> {
        self.workspace_changed();
        self.project.active = self.focused_index();
        self.project.secondary = Some(index);
        self.project.focus_secondary = focus_new;
        self.project.layout = layout;
        self.ensure_session(index)?;
        let workspace = &self.project.workspaces[index];
        if workspace.environment.is_none()
            && !workspace.pending
            && compose::has_config(&workspace.tree)
        {
            self.env(index, EnvironmentOp::Probe)?;
        }
        Ok(())
    }
    fn new_tab_choices(&self) -> Vec<(String, IntentCommand)> {
        let mut choices = Vec::with_capacity(4);
        choices.push((
            "Open folder — no Git required".to_owned(),
            IntentCommand::OpenProject,
        ));
        if self.project.repository.is_some() {
            choices.push((
                format!("New branch / worktree in {}", self.project.name),
                IntentCommand::NewWorktree,
            ));
            choices.push((
                format!("Open existing worktree of {}", self.project.name),
                IntentCommand::OpenWorktree,
            ));
        }
        if !self.saved_projects.is_empty() {
            choices.push((
                format!("Switch to an open project ({})", self.saved_projects.len()),
                IntentCommand::SwitchProject,
            ));
        }
        choices
    }
    fn close_workspace(&mut self) {
        self.workspace_changed();
        let index = self.focused_index();
        if self.project.workspaces.len() == 1 {
            if let Some(project) = self.saved_projects.pop() {
                self.project = project;
            } else {
                self.project.workspaces.clear();
                self.should_quit = true;
                return;
            }
        } else {
            self.project.close_workspace(index);
        }
        self.set_notice("Tab closed; any checkout and Docker environment retained.");
    }
    fn new_request(&mut self) -> u64 {
        let request = self.next_request;
        self.next_request += 1;
        request
    }

    fn navigation_error(&mut self, error: String) {
        if let Some(Modal::Loading {
            retry: Some(mut dialog),
            ..
        }) = self.modal.take()
        {
            Arc::make_mut(&mut dialog.conversation).pop();
            dialog.error = Some(error.clone());
            self.modal = Some(Modal::Intent(dialog));
        }
        self.set_notice(error);
    }

    fn open_intent(&mut self) -> Result<()> {
        let request = self.new_request();
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
                        split: None,
                    })
                    .context("command queue is busy")?;
            }
            Plan::Open { target, agent } => {
                let target = targets
                    .iter()
                    .find(|candidate| candidate.id == target)
                    .context("The interpreter selected an unknown project or worktree")?;
                let request = self.new_request();
                self.jobs
                    .try_send(Job::OpenProject {
                        request,
                        path: target.path.clone(),
                        agent,
                    })
                    .context("command queue is busy")?;
                self.modal = Some(Modal::Loading {
                    request,
                    message: "Opening project...",
                    retry: None,
                });
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
                let request = self.new_request();
                self.modal = Some(Modal::OpenProject(ProjectPicker::new(
                    self.focused().tree.path.clone(),
                    &self.jobs,
                    request,
                )?))
            }
            OpenWorktree => {
                let request = self.new_request();
                self.jobs
                    .try_send(Job::List {
                        request,
                        repository: self.git_repository()?.clone(),
                    })
                    .context("command queue is busy")?;
                self.modal = Some(Modal::Loading {
                    request,
                    message: "Reading worktrees...",
                    retry: None,
                });
            }
            SwitchProject => {
                if self.saved_projects.is_empty() {
                    self.set_notice("No other project open. F1 → ask to open a project.");
                } else {
                    self.modal = Some(Modal::SwitchProject { selected: 0 });
                }
            }
            NewWorktree => {
                self.modal = Some(Modal::NewWorktree {
                    input: String::new(),
                    split: None,
                })
            }
            NewSession => self.action(Action::NewSession)?,
            SplitVertical => self.split(LayoutMode::Vertical)?,
            SplitHorizontal => self.split(LayoutMode::Horizontal)?,
            FullWidth => self.full_width(),
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
                self.state_dirty = true;
                self.modal = None;
                if self.use_kitty {
                    kitty::set_native(&mut io::stdout(), self.native_keys)?;
                }
            }
            Action::Launcher => self.open_intent()?,
            Action::Quit => self.modal = Some(Modal::Confirm(Confirm::Quit)),
            Action::NewTab => {
                self.modal = Some(Modal::NewTab {
                    choices: self.new_tab_choices(),
                    selected: 0,
                })
            }
            Action::CloseWorktree => self.modal = Some(Modal::Confirm(Confirm::CloseWorktree)),
            Action::NewSession => self.modal = Some(Modal::NewSession { selected: 0 }),
            Action::CloseSession => self.modal = Some(Modal::Confirm(Confirm::CloseSession)),
            Action::NextWorktree | Action::PreviousWorktree => {
                let current = (None, self.focused_index());
                let count = self.tabs().count();
                let position = self.tabs().position(|tab| tab == current).unwrap_or(0);
                let target = if action == Action::NextWorktree {
                    (position + 1) % count
                } else {
                    (position + count - 1) % count
                };
                let target = self.tabs().nth(target);
                if let Some((slot, workspace)) = target {
                    self.focus_tab(slot, workspace)?;
                }
            }
            Action::NextSession | Action::PreviousSession => {
                self.workspace_changed();
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
                    self.state_dirty = true;
                }
            }
            Action::SplitVertical => self.split(LayoutMode::Vertical)?,
            Action::SplitHorizontal => self.split(LayoutMode::Horizontal)?,
            Action::CycleLayout => match self.project.layout {
                LayoutMode::Full => self.split(LayoutMode::Vertical)?,
                LayoutMode::Vertical => {
                    self.workspace_changed();
                    self.project.layout = LayoutMode::Horizontal;
                }
                LayoutMode::Horizontal => self.full_width(),
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
        let result = (|| -> Result<()> {
            match &mut modal {
                Modal::Loading { .. } => {}
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
                Modal::NewTab { choices, selected } => {
                    navigate(key, selected, choices.len());
                    if key.code == KeyCode::Enter {
                        if let Some(&(_, command)) = choices.get(*selected) {
                            self.run_command(command)?;
                        }
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
                        self.switch_project(*selected, None)?;
                        retain = false;
                    }
                }
                Modal::NewWorktree { input, split } => match key.code {
                    KeyCode::Enter if !input.trim().is_empty() => {
                        self.jobs
                            .try_send(Job::Create {
                                branch: input.trim().to_owned(),
                                base: None,
                                agent: None,
                                split: *split,
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
                    navigate(key, selected, self.project.workspaces.len() + 1);
                    if key.code == KeyCode::Enter {
                        if *selected == 0 {
                            self.modal = Some(Modal::NewWorktree {
                                input: String::new(),
                                split: Some(*layout),
                            });
                            retain = false;
                        } else if *selected - 1 != self.focused_index() {
                            self.split_with(*layout, *selected - 1, false)?;
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
                                        let _ = tx
                                            .send(Event::Error(format!("Opening {url}: {error}")));
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
                                retain = false;
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
                                self.state_dirty = true;
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
            Ok(())
        })();
        if let Err(error) = result {
            if retain && self.modal.is_none() {
                self.modal = Some(modal);
            }
            return Err(error);
        }
        if !retain && matches!(&modal, Modal::NewWorktree { .. }) {
            self.pending_creations.push_back((
                self.git_repository()?.common_dir.clone(),
                self.focused().tree.id.clone(),
                modal,
            ));
            return Ok(());
        }
        if retain {
            self.modal = Some(modal);
        }
        Ok(())
    }

    fn handle(&mut self, event: Event) -> Result<bool> {
        if self.should_quit {
            return Ok(false);
        }
        if !self.interaction_visible {
            match &event {
                Event::Input(crossterm::event::Event::Key(key))
                    if keys::action(*key) == Some(Action::ToggleNative) => {}
                Event::Input(crossterm::event::Event::Key(key))
                    if key.code == KeyCode::Esc && key.kind != KeyEventKind::Release =>
                {
                    self.modal = None;
                    return Ok(true);
                }
                Event::Input(crossterm::event::Event::Resize(_, _)) => {}
                Event::Input(_) => return Ok(false),
                _ => {}
            }
        }
        match event {
            Event::Input(crossterm::event::Event::Key(key)) => {
                self.scroll_drag = None;
                if key.kind == KeyEventKind::Release {
                    return Ok(false);
                }
                if key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                {
                    match key.code {
                        KeyCode::Char('c' | 'C') => {
                            if let Some(selection) = &self.selection
                                && selection.moved
                            {
                                clipboard::copy(&selection.text())?;
                            }
                            return Ok(false);
                        }
                        KeyCode::Char('v' | 'V') => {
                            let text = clipboard::paste()?;
                            return self.handle(Event::Input(crossterm::event::Event::Paste(text)));
                        }
                        _ => {}
                    }
                }
                self.selection = None;
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
            Event::Input(crossterm::event::Event::Paste(text)) => {
                self.selection = None;
                match &mut self.modal {
                    Some(Modal::Intent(dialog)) if !dialog.pending && !dialog.submitted => {
                        paste_input(&mut dialog.input, &text, 4096);
                    }
                    Some(Modal::NewWorktree { input, .. }) => paste_input(input, &text, 256),
                    Some(Modal::OpenProject(picker)) => picker.paste(&text),
                    None => {
                        if let Some(session) = self.focused_session() {
                            session.paste(&text)?;
                        }
                    }
                    _ => {}
                }
            }
            Event::Input(crossterm::event::Event::Mouse(mouse)) if self.modal.is_none() => {
                if self
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.dragging)
                    && matches!(
                        mouse.kind,
                        MouseEventKind::Drag(MouseButton::Left)
                            | MouseEventKind::Up(MouseButton::Left)
                    )
                {
                    let selection = self.selection.as_mut().unwrap();
                    selection.update(mouse.column, mouse.row);
                    if matches!(mouse.kind, MouseEventKind::Up(_)) {
                        selection.dragging = false;
                        if selection.moved {
                            clipboard::copy(&selection.text())?;
                        } else {
                            let link = selection.link();
                            self.selection = None;
                            if let Some(uri) = link {
                                self.open_link(uri)?;
                            }
                        }
                    }
                    return Ok(true);
                }
                if matches!(
                    mouse.kind,
                    MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.selection = None;
                }
                if let Some((id, grab)) = self.scroll_drag {
                    if matches!(
                        mouse.kind,
                        MouseEventKind::Drag(MouseButton::Left)
                            | MouseEventKind::Up(MouseButton::Left)
                    ) {
                        if let Some((session, bar)) = self.panes.iter().find_map(|pane| {
                            let workspace = self.project.workspaces.get(pane.workspace)?;
                            let session = workspace.sessions.get(workspace.selected)?;
                            (session.id == id).then_some((session, pane.scrollbar?))
                        }) {
                            session.scroll_to(bar.offset_at(mouse.row, grab));
                        }
                        if matches!(mouse.kind, MouseEventKind::Up(_)) {
                            self.scroll_drag = None;
                        }
                        return Ok(true);
                    }
                }
                if let Some(hit) = self
                    .tab_hits
                    .iter()
                    .find(|hit| hit.area.contains((mouse.column, mouse.row).into()))
                {
                    if matches!(mouse.kind, crossterm::event::MouseEventKind::Down(_)) {
                        match (hit.project, hit.workspace) {
                            (slot, Some(workspace)) => self.focus_tab(slot, workspace)?,
                            (Some(index), None) => self.switch_project(index, None)?,
                            (None, None) => {}
                        }
                    }
                } else if let Some(pane) = self.panes.iter().find(|p| {
                    p.body.contains((mouse.column, mouse.row).into())
                        || p.scrollbar
                            .is_some_and(|bar| bar.track.contains((mouse.column, mouse.row).into()))
                }) {
                    let (index, body, scrollbar) = (pane.workspace, pane.body, pane.scrollbar);
                    if matches!(mouse.kind, crossterm::event::MouseEventKind::Down(_)) {
                        self.project.focus_secondary = self.project.secondary == Some(index)
                            && self.project.layout != LayoutMode::Full;
                        self.state_dirty = true;
                    }
                    let workspace = &self.project.workspaces[index];
                    if let Some(session) = workspace.sessions.get(workspace.selected) {
                        if let Some(bar) = scrollbar
                            .filter(|bar| bar.track.contains((mouse.column, mouse.row).into()))
                        {
                            match mouse.kind {
                                MouseEventKind::Down(MouseButton::Left) => {
                                    let grab =
                                        if bar.thumb.contains((mouse.column, mouse.row).into()) {
                                            mouse.row - bar.thumb.y
                                        } else {
                                            bar.thumb.height / 2
                                        };
                                    self.scroll_drag = Some((session.id, grab));
                                    session.scroll_to(bar.offset_at(mouse.row, grab));
                                }
                                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                                    let mut scroll = mouse;
                                    scroll.modifiers |= KeyModifiers::SHIFT;
                                    session.mouse(scroll, 0, mouse.row - body.y)?;
                                }
                                _ => {}
                            }
                        } else if mouse.kind == MouseEventKind::Down(MouseButton::Middle) {
                            let text = clipboard::paste()?;
                            session.paste(&text)?;
                        } else if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                            let parser = session.parser.lock();
                            let selection = Selection::new(
                                session.id,
                                body,
                                parser.screen(),
                                mouse.column,
                                mouse.row,
                            );
                            let select = parser.screen().mouse_protocol_mode()
                                == vt100::MouseProtocolMode::None
                                || mouse.modifiers.contains(KeyModifiers::CONTROL)
                                || selection.link().is_some();
                            drop(parser);
                            if select {
                                self.selection = Some(selection);
                            } else {
                                session.mouse(mouse, mouse.column - body.x, mouse.row - body.y)?;
                            }
                        } else {
                            session.mouse(mouse, mouse.column - body.x, mouse.row - body.y)?;
                        }
                    }
                }
            }
            Event::Input(crossterm::event::Event::Resize(cols, rows)) => {
                self.interaction_visible = cols >= 30 && rows >= 10;
                self.panes.clear();
                self.scroll_drag = None;
                self.selection = None;
                self.tab_hits.clear();
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
                let mut identity_changed = false;
                for workspace in self.all_workspaces_mut() {
                    if let Some(session) = workspace
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == update.session)
                    {
                        if session.alive.load(Ordering::Acquire) {
                            let resume = match update.resume {
                                integration::ResumeUpdate::Unknown => None,
                                integration::ResumeUpdate::Clear => Some(None),
                                integration::ResumeUpdate::Selected(identity) => {
                                    Some(Some(identity))
                                }
                            };
                            if let Some(resume) = resume {
                                identity_changed = session.resume != resume;
                                session.resume = resume;
                            }
                            session.state = update.state;
                            session.message = update.message;
                        }
                        break;
                    }
                }
                self.state_dirty |= identity_changed;
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
                        let creates_worktree = matches!(&plan, Plan::CreateWorktree { .. });
                        if let Err(error) = self.execute_intent(plan, &dialog.targets) {
                            dialog.error = Some(format!("{error:#}"));
                            Arc::make_mut(&mut dialog.conversation).pop();
                            self.modal = Some(Modal::Intent(dialog));
                        } else if creates_worktree {
                            self.pending_creations.push_back((
                                self.git_repository()?.common_dir.clone(),
                                self.focused().tree.id.clone(),
                                Modal::Intent(dialog),
                            ));
                        } else if let Some(Modal::Loading { retry, .. }) = &mut self.modal {
                            *retry = Some(dialog);
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
            Event::ProjectOpened {
                request,
                result,
                agent,
            } => {
                let expected = match &self.modal {
                    Some(Modal::OpenProject(picker)) => picker.pending && picker.request == request,
                    Some(Modal::Loading {
                        request: pending, ..
                    }) => *pending == request,
                    _ => false,
                };
                if !expected {
                    return Ok(false);
                }
                match result {
                    Ok(project) => self.open_project(project, agent)?,
                    Err(error) => {
                        if let Some(Modal::OpenProject(picker)) = &mut self.modal {
                            picker.pending = false;
                            self.set_notice(error);
                        } else {
                            self.navigation_error(error);
                        }
                    }
                }
            }
            Event::Created {
                project,
                source,
                preparation_error,
                result,
                agent,
                split,
            } => {
                let pending = self
                    .pending_creations
                    .iter()
                    .position(|(origin, _, _)| origin == &project)
                    .and_then(|index| self.pending_creations.remove(index));
                match result {
                    Ok(tree) => {
                        self.workspace_changed();
                        let has_compose = compose::has_config(&tree);
                        if self.project.common_dir() != Some(project.as_path()) {
                            if let Some(saved) = self
                                .saved_projects
                                .iter_mut()
                                .find(|saved| saved.common_dir() == Some(project.as_path()))
                            {
                                let mut workspace = Workspace::new(tree);
                                workspace.source =
                                    preparation_error.as_ref().map(|_| source.clone());
                                if has_compose && preparation_error.is_none() {
                                    self.jobs
                                        .try_send(Job::Environment {
                                            tree: workspace.tree.clone(),
                                            operation: EnvironmentOp::Up,
                                            source: workspace.source.clone(),
                                        })
                                        .context("command queue is busy")?;
                                    workspace.pending = true;
                                }
                                saved.workspaces.push(workspace);
                            }
                            if let Some(error) = preparation_error {
                                self.set_notice(format!("Worktree created; local environment preparation failed: {error}"));
                            }
                            return Ok(true);
                        }
                        let index = if let Some(index) = self
                            .project
                            .workspaces
                            .iter()
                            .position(|w| w.tree.id == tree.id)
                        {
                            index
                        } else {
                            self.project.workspaces.push(Workspace::new(tree));
                            self.project.workspaces.last_mut().unwrap().source =
                                preparation_error.as_ref().map(|_| source);
                            let index = self.project.workspaces.len() - 1;
                            // Creation owns the initial start; do not enqueue a redundant status query.
                            if has_compose && preparation_error.is_none() {
                                self.env(index, EnvironmentOp::Up)?;
                            }
                            index
                        };
                        // A newer dialog describes the current focus; do not retarget it.
                        if self.modal.is_none() {
                            match split {
                                Some(layout) if index != self.focused_index() => {
                                    self.split_with(layout, index, true)?;
                                }
                                _ => self.select_workspace(index, agent)?,
                            }
                        } else if let Some(agent) = agent {
                            self.spawn(index, agent.as_str(), None)?;
                        }
                        if let Some(error) = preparation_error {
                            self.set_notice(format!("Worktree created; local environment preparation failed: {error}. Fix the files, then start its environment again."));
                        } else {
                            self.set_notice(if has_compose {
                                "Worktree created; starting its isolated Docker environment."
                            } else {
                                "Worktree created. No Compose config found; terminal is ready."
                            });
                        }
                    }
                    Err(error) => {
                        if self.modal.is_none()
                            && self.project.common_dir() == Some(project.as_path())
                            && let Some((_, workspace, mut modal)) = pending
                            && self.focused().tree.id == workspace
                        {
                            if let Modal::Intent(dialog) = &mut modal {
                                Arc::make_mut(&mut dialog.conversation).pop();
                                dialog.error = Some(error.clone());
                            }
                            self.modal = Some(modal);
                        }
                        self.set_notice(error);
                    }
                }
            }
            Event::Environment {
                tree,
                operation,
                prepared,
                result,
            } => {
                let active = self.project.workspaces.iter().any(|w| w.tree.id == tree);
                let Some(workspace) = self.all_workspaces_mut().find(|w| w.tree.id == tree) else {
                    return Ok(false);
                };
                workspace.pending = false;
                let source_changed = prepared && workspace.source.take().is_some();
                match result {
                    Ok(environment) => {
                        let summary = (!matches!(operation, EnvironmentOp::Probe))
                            .then(|| environment_summary(workspace.label(), &environment));
                        workspace.environment = Some(environment);
                        if !active {
                        } else if let Some(summary) = summary {
                            self.set_notice(summary);
                        } else {
                            self.notice = None;
                        }
                    }
                    Err(error) => self.set_notice(error),
                }
                self.state_dirty |= source_changed;
            }
            Event::Worktrees {
                request,
                project,
                result,
            } => {
                if self.project.common_dir() == Some(project.as_path())
                    && matches!(&self.modal, Some(Modal::Loading { request: pending, .. }) if *pending == request)
                {
                    match result {
                        Ok(trees) => self.modal = Some(Modal::OpenWorktree { trees, selected: 0 }),
                        Err(error) => self.navigation_error(error),
                    }
                }
            }
            Event::Removed { tree, result } => match result {
                Ok(()) => {
                    self.workspace_changed();
                    if let Some(index) = self
                        .project
                        .workspaces
                        .iter()
                        .position(|w| w.tree.id == tree)
                    {
                        let removed_focus = index == self.focused_index();
                        if removed_focus {
                            self.modal = None;
                        }
                        if self.project.workspaces.len() == 1 {
                            self.close_workspace();
                        } else {
                            self.project.close_workspace(index);
                        }
                        if removed_focus && !self.should_quit {
                            self.ensure_session(self.focused_index())?;
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

/// Canonical existing directory, or an error naming the problem.
fn folder(path: &Path) -> Result<PathBuf> {
    let path = path.canonicalize().context("resolve folder")?;
    anyhow::ensure!(path.is_dir(), "choose a folder, not a file");
    Ok(path)
}

/// Last path component with control characters removed; empty for `/`.
fn folder_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .filter(|c| !c.is_control())
        .collect()
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

fn prepare_local_environment(source: &Path, target: &Worktree) -> Result<()> {
    let repository = worktree::discover(source)?;
    let source = crate::selected_tree(&repository, source)?;
    compose::prepare_local_files(&source, target)
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
                    Job::OpenProject {
                        request,
                        path,
                        agent,
                    } => Event::ProjectOpened {
                        request,
                        result: Project::open(&path).map_err(|e| format!("{e:#}")),
                        agent,
                    },
                    Job::Create {
                        repository,
                        branch,
                        base,
                        agent,
                        split,
                    } => {
                        let result = worktree::create(&repository, &branch, base.as_deref())
                            .map_err(|error| format!("{error:#}"));
                        let preparation_error = result.as_ref().ok().and_then(|tree| {
                            prepare_local_environment(&repository.root, tree)
                                .err()
                                .map(|error| format!("{error:#}"))
                        });
                        Event::Created {
                            result,
                            preparation_error,
                            source: repository.root,
                            project: repository.common_dir,
                            agent,
                            split,
                        }
                    }
                    Job::List {
                        request,
                        repository,
                    } => Event::Worktrees {
                        request,
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
                    Job::Environment {
                        tree,
                        operation,
                        source,
                    } => {
                        let mut prepared = false;
                        let result = (|| {
                            if matches!(operation, EnvironmentOp::Up | EnvironmentOp::Restart)
                                && let Some(source) = source
                            {
                                prepare_local_environment(&source, &tree)?;
                                prepared = true;
                            }
                            match operation {
                                EnvironmentOp::Up => compose::up(&tree),
                                EnvironmentOp::Down => compose::down(&tree),
                                EnvironmentOp::Restart => compose::restart(&tree),
                                EnvironmentOp::Status | EnvironmentOp::Probe => {
                                    compose::status(&tree)
                                }
                            }
                        })();
                        Event::Environment {
                            tree: tree.id,
                            operation,
                            prepared,
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
) -> Result<Option<(Project, bool, bool)>> {
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
        "Choose New terminal for a shell here, or open a project or any folder.".into()
    };
    let mut picker = ProjectPicker::new(base, jobs, 0)?;
    picker.terminal_path = Some(launch_path.clone());
    let mut native_keys = false;
    let mut interaction_visible = true;
    loop {
        terminal.draw(|frame| {
            let bounds = frame.area();
            interaction_visible = bounds.width >= 30 && bounds.height >= 10;
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
                    _ if !interaction_visible => {}
                    _ => match picker.key(key, jobs) {
                        Ok(true) => {
                            return Ok(Some((Project::terminal(&launch_path)?, native_keys, true)));
                        }
                        Ok(false) => {}
                        Err(error) => notice = format!("{error:#}"),
                    },
                }
            }
            Event::Input(crossterm::event::Event::Paste(text)) if interaction_visible => {
                picker.paste(&text);
            }
            Event::Projects { base, projects } if picker.base == base => {
                picker.set_projects(projects);
            }
            Event::ProjectOpened {
                request, result, ..
            } if picker.request == request => match result {
                Ok(project) => return Ok(Some((project, native_keys, false))),
                Err(error) => {
                    picker.pending = false;
                    notice = error;
                }
            },
            Event::Error(error) => notice = error,
            Event::Shutdown => return Ok(None),
            _ => {}
        }
    }
}

fn restore_projects(
    snapshot: &restore::Snapshot,
    socket: &Path,
    tx: &SyncSender<Event>,
    size: ratatui::layout::Size,
    next_session: &mut u64,
) -> Result<(Project, Vec<Project>)> {
    let mut projects = Vec::with_capacity(snapshot.projects.len());
    for (order, saved) in snapshot.projects.iter().enumerate() {
        let mut project = Project::open(&saved.workspaces[0].path)
            .with_context(|| format!("restore project {}", saved.workspaces[0].path.display()))?;
        for (index, saved_workspace) in saved.workspaces.iter().enumerate() {
            let tree = if index == 0 {
                project.workspaces.remove(0).tree
            } else {
                let mut candidate = Project::open(&saved_workspace.path).with_context(|| {
                    format!("restore worktree {}", saved_workspace.path.display())
                })?;
                anyhow::ensure!(
                    project.same_project(&candidate),
                    "saved worktree {} no longer belongs to its project",
                    saved_workspace.path.display()
                );
                candidate.workspaces.remove(0).tree
            };
            let mut workspace = Workspace::new(tree);
            workspace.source = saved_workspace.source.clone();
            for saved_session in &saved_workspace.sessions {
                let id = *next_session;
                *next_session += 1;
                let session = Session::spawn(
                    id,
                    &saved_session.kind,
                    saved_session.resume.as_deref(),
                    &workspace.tree.path,
                    socket,
                    tx.clone(),
                    size.height.saturating_sub(4),
                    size.width.saturating_sub(2),
                    None,
                )
                .with_context(|| {
                    format!(
                        "restore {} session in {}",
                        saved_session.kind,
                        workspace.tree.path.display()
                    )
                })?;
                workspace.sessions.push(session);
            }
            workspace.selected = saved_workspace.selected;
            project.workspaces.push(workspace);
        }
        project.active = saved.active;
        project.secondary = saved.secondary;
        project.focus_secondary = saved.focus_secondary;
        project.layout = match saved.layout {
            restore::SavedLayout::Full => LayoutMode::Full,
            restore::SavedLayout::Vertical => LayoutMode::Vertical,
            restore::SavedLayout::Horizontal => LayoutMode::Horizontal,
        };
        project.order = order as u64;
        projects.push(project);
    }
    let current = projects.remove(snapshot.active_project);
    Ok((current, projects))
}

pub fn run(
    path: &Path,
    agent: &str,
    use_kitty: bool,
    direct_start: bool,
    fresh: bool,
) -> Result<()> {
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
        notice: mut update_notice,
        mut pending,
    } = match startup {
        Ok(Some(startup)) => startup,
        result => {
            signal_handle.close();
            return result.map(|_| ());
        }
    };
    let mut store = match restore::Store::open() {
        Ok(store) => Some(store),
        Err(error) => {
            update_notice = Some(format!(
                "Workspace restoration/saving unavailable: {error:#}. This window will not replace the saved workspace."
            ));
            None
        }
    };
    let snapshot = if fresh {
        None
    } else {
        match store.as_ref().map(restore::Store::load).transpose() {
            Ok(snapshot) => snapshot
                .flatten()
                .filter(|snapshot| !snapshot.projects.is_empty()),
            Err(error) => {
                update_notice = Some(format!(
                    "Saved workspace could not be loaded: {error:#}. Original state retained; saving disabled. Use --fresh to replace it."
                ));
                store = None;
                None
            }
        }
    };
    let server = integration::start(tx.clone())?;
    let mut next_session = 1;
    let restored = match snapshot
        .as_ref()
        .map(|snapshot| {
            restore_projects(
                snapshot,
                &server.socket_path,
                &tx,
                terminal.size()?,
                &mut next_session,
            )
        })
        .transpose()
    {
        Ok(restored) => restored,
        Err(error) => {
            update_notice = Some(format!(
                "Saved workspace could not be restored: {error:#}. Original state retained; saving disabled. Fix the cause and restart, or use --fresh."
            ));
            store = None;
            None
        }
    };
    let was_restored = restored.is_some();
    let (project, saved_projects, native_keys, start_shell) =
        if let Some((project, saved)) = restored {
            (
                project,
                saved,
                snapshot.as_ref().unwrap().native_keys,
                false,
            )
        } else {
            let selection = match direct_start.then(|| Project::open(path)) {
                Some(Ok(project)) => Ok(Some((project, false, false))),
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
            match selection {
                Ok(Some((project, native_keys, start_shell))) => {
                    (project, Vec::new(), native_keys, start_shell)
                }
                result => {
                    signal_handle.close();
                    return result.map(|_| ());
                }
            }
        };
    if use_kitty && native_keys {
        kitty::set_native(&mut io::stdout(), true)?;
    }
    let active = project.active;
    let mut app = App {
        project,
        saved_projects,
        native_keys,
        modal: None,
        notice: update_notice,
        panes: Vec::with_capacity(2),
        scroll_drag: None,
        selection: None,
        tab_hits: Vec::new(),
        interaction_visible: true,
        should_quit: false,
        default_agent: agent.into(),
        next_session,
        next_request: 1,
        pending_creations: VecDeque::new(),
        state_dirty: true,
        socket: server.socket_path.clone(),
        tx: tx.clone(),
        jobs,
        use_kitty,
    };
    if !was_restored {
        let launched = if start_shell {
            app.spawn(active, "shell", None)
        } else {
            app.ensure_session(active)
        };
        if let Err(error) = launched {
            app.set_notice(format!(
                "{error:#}. F1 → ask to start another agent or shell."
            ));
        }
    }
    if app.project.repository.is_some() && compose::has_config(&app.project.workspaces[active].tree)
    {
        app.env(active, EnvironmentOp::Probe)?;
    }
    let frame_interval = Duration::from_millis(16);
    let mut last_draw = Instant::now() - frame_interval;
    let mut redraw = true;
    while !app.should_quit {
        redraw |= app.checkpoint(&mut store);
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
    app.checkpoint(&mut store);
    signal_handle.close();
    drop(app);
    drop(server);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_local_preparation_releases_source_even_when_docker_fails() {
        let (mut app, _queued, _events) = interaction_app();
        let source = PathBuf::from("/interaction-fixture/source");
        app.project.workspaces[0].source = Some(source.clone());
        app.handle(Event::Environment {
            tree: "main".into(),
            operation: EnvironmentOp::Up,
            prepared: false,
            result: Err("missing local input".into()),
        })
        .unwrap();
        assert_eq!(
            app.snapshot().projects[0].workspaces[0].source,
            Some(source)
        );
        app.handle(Event::Environment {
            tree: "main".into(),
            operation: EnvironmentOp::Up,
            prepared: true,
            result: Err("Docker daemon unavailable".into()),
        })
        .unwrap();
        assert_eq!(app.snapshot().projects[0].workspaces[0].source, None);
        assert!(app.project.workspaces[0].environment.is_none());
        assert!(!app.project.workspaces[0].pending);
    }

    #[test]
    fn scrollbar_click_and_drag_reach_history_and_live_output() {
        use crossterm::event::MouseEvent;
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = Session::spawn(
            1,
            "shell",
            None,
            directory.path(),
            &directory.path().join("unused.sock"),
            app.tx.clone(),
            12,
            38,
            Some((
                "/bin/sh".into(),
                vec!["-c".into(), "IFS= read -r line".into()],
            )),
        )
        .unwrap();
        {
            let mut parser = session.parser.lock();
            for row in 0..60 {
                parser.process(format!("row-{row:02}\r\n").as_bytes());
            }
        }
        app.project.workspaces[0].sessions.push(session);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(40, 14)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        let bar = app.panes[0].scrollbar.unwrap();
        assert_eq!(app.panes[0].body.bottom(), 14);
        assert_eq!(bar.thumb.bottom(), bar.track.bottom());
        let mouse = |kind, column, row| {
            Event::Input(crossterm::event::Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }))
        };
        app.handle(mouse(
            MouseEventKind::Down(MouseButton::Left),
            bar.track.x,
            bar.track.y,
        ))
        .unwrap();
        assert!(
            app.focused_session()
                .unwrap()
                .parser
                .lock()
                .screen()
                .contents()
                .starts_with("row-00")
        );
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        // A drag remains captured even after the pointer leaves the narrow gutter.
        app.handle(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            0,
            bar.track.bottom() - 1,
        ))
        .unwrap();
        app.handle(mouse(
            MouseEventKind::Up(MouseButton::Left),
            0,
            bar.track.bottom() - 1,
        ))
        .unwrap();
        let parser = app.focused_session().unwrap().parser.lock();
        assert_eq!(parser.screen().scrollback(), 0);
        assert!(parser.screen().contents().contains("row-59"));
        drop(parser);
        assert!(app.scroll_drag.is_none());
        app.notice = Some("Environment ready".into());
        ui::resize_sessions(&mut app, ratatui::layout::Size::new(40, 14)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        assert_eq!(app.panes[0].body.bottom(), 13);
        app.notice = None;
        ui::resize_sessions(&mut app, ratatui::layout::Size::new(40, 14)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        assert_eq!(app.panes[0].body.bottom(), 14);
    }

    #[test]
    fn queued_mouse_and_output_cannot_use_a_closed_split_pane() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut app, _queued, _events) = interaction_app();
        app.project.layout = LayoutMode::Vertical;
        app.project.secondary = Some(1);
        app.project.focus_secondary = true;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        let old_body = app.panes[1].body;
        app.close_workspace();
        app.handle(Event::Input(crossterm::event::Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: old_body.x,
            row: old_body.y,
            modifiers: KeyModifiers::NONE,
        })))
        .unwrap();
        assert!(!app.handle(Event::Dirty(99)).unwrap());
        assert_eq!(app.focused().tree.id, "main");
        assert_eq!(app.project.workspaces.len(), 1);
        assert!(!app.project.focus_secondary);
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.panes[0].workspace, 0);
    }

    #[test]
    fn closing_last_workspace_forgets_it_but_quit_preserves_open_tabs() {
        let (mut app, _queued, _events) = interaction_app();
        app.project.active = 1;
        app.action(Action::Quit).unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .unwrap();
        let saved = app.snapshot();
        assert_eq!(saved.projects[0].workspaces.len(), 2);
        assert_eq!(saved.projects[0].active, 1);
        app.should_quit = false;
        app.close_workspace();
        assert_eq!(
            app.snapshot().projects[0].workspaces[0].path,
            PathBuf::from("/interaction-fixture/main")
        );
        app.close_workspace();
        assert!(app.should_quit);
        assert!(app.snapshot().projects.is_empty());
    }

    fn interaction_app() -> (App, mpsc::Receiver<Job>, mpsc::Receiver<Event>) {
        let (tx, events) = mpsc::sync_channel(8);
        let (jobs, queued) = mpsc::sync_channel(1);
        let root = PathBuf::from("/interaction-fixture");
        let app = App {
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
                order: 0,
            },
            saved_projects: Vec::new(),
            native_keys: false,
            modal: None,
            notice: None,
            panes: Vec::new(),
            scroll_drag: None,
            selection: None,
            tab_hits: Vec::new(),
            interaction_visible: true,
            should_quit: false,
            default_agent: "shell".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
            socket: root.join("agent.sock"),
            tx,
            jobs,
            use_kitty: false,
        };
        (app, queued, events)
    }

    #[test]
    fn canceled_worktree_listing_cannot_replace_a_newer_dialog() {
        let (mut app, queued, _events) = interaction_app();
        app.run_command(IntentCommand::OpenWorktree).unwrap();
        let Job::List {
            request,
            repository,
        } = queued.recv().unwrap()
        else {
            panic!("expected worktree listing");
        };
        app.modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        app.modal = Some(Modal::Confirm(Confirm::Quit));
        let tree = app.project.workspaces[1].tree.clone();
        app.handle(Event::Worktrees {
            request,
            project: repository.common_dir.clone(),
            result: Ok(vec![tree.clone()]),
        })
        .unwrap();
        assert!(matches!(app.modal, Some(Modal::Confirm(Confirm::Quit))));
        assert!(!app.should_quit);

        app.run_command(IntentCommand::OpenWorktree).unwrap();
        let Job::List {
            request: current, ..
        } = queued.recv().unwrap()
        else {
            panic!("expected replacement listing");
        };
        app.handle(Event::Worktrees {
            request,
            project: repository.common_dir.clone(),
            result: Err("stale failure".into()),
        })
        .unwrap();
        assert!(matches!(app.modal, Some(Modal::Loading { request, .. }) if request == current));
        app.handle(Event::Worktrees {
            request: current,
            project: repository.common_dir,
            result: Ok(vec![tree]),
        })
        .unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::OpenWorktree { trees, .. }) if trees[0].id == "other")
        );
    }

    #[test]
    fn canceled_project_open_cannot_complete_a_replacement_picker() {
        let (mut app, queued, _events) = interaction_app();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let mut requests = Vec::new();
        for path in ["/cancelled", "/replacement"] {
            app.run_command(IntentCommand::OpenProject).unwrap();
            queued.recv().unwrap();
            let Some(Modal::OpenProject(picker)) = &mut app.modal else {
                panic!("expected project picker");
            };
            picker.input = path.into();
            app.modal_key(enter).unwrap();
            let Job::OpenProject { request, .. } = queued.recv().unwrap() else {
                panic!("expected project opening");
            };
            requests.push(request);
            if path == "/cancelled" {
                app.modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                    .unwrap();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        assert!(
            !app.handle(Event::ProjectOpened {
                request: requests[0],
                agent: None,
                result: Ok(Project::terminal(directory.path()).unwrap()),
            })
            .unwrap()
        );
        assert_eq!(app.project.name, "fixture");
        assert!(app.saved_projects.is_empty());
        assert!(
            matches!(&app.modal, Some(Modal::OpenProject(picker)) if picker.pending && picker.input == "/replacement")
        );
        app.handle(Event::ProjectOpened {
            request: requests[1],
            agent: None,
            result: Err("replacement unavailable".into()),
        })
        .unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::OpenProject(picker)) if !picker.pending && picker.input == "/replacement")
        );
        assert_eq!(app.notice.as_deref(), Some("replacement unavailable"));
    }

    #[test]
    fn failed_interpreted_navigation_restores_the_editable_request() {
        let (mut app, queued, _events) = interaction_app();
        app.open_intent().unwrap();
        queued.recv().unwrap();
        let Some(Modal::Intent(dialog)) = &mut app.modal else {
            panic!("expected interpreter dialog");
        };
        dialog.input = "open gone".into();
        dialog.loading = false;
        dialog.pending = true;
        dialog.targets.push(IntentTarget {
            id: 0,
            kind: "project",
            name: "gone".into(),
            path: PathBuf::from("/gone"),
            branch: None,
        });
        Arc::make_mut(&mut dialog.conversation).push(serde_json::json!({
            "role": "user", "content": dialog.input,
        }));
        let request = dialog.request;
        app.handle(Event::IntentResolved {
            request,
            result: Ok(Plan::Open {
                target: 0,
                agent: None,
            }),
        })
        .unwrap();
        let Job::OpenProject { request, .. } = queued.recv().unwrap() else {
            panic!("expected project opening");
        };
        app.handle(Event::ProjectOpened {
            request,
            agent: None,
            result: Err("folder disappeared".into()),
        })
        .unwrap();
        let Some(Modal::Intent(dialog)) = &app.modal else {
            panic!("failed navigation discarded the request");
        };
        assert_eq!(dialog.input, "open gone");
        assert_eq!(dialog.error.as_deref(), Some("folder disappeared"));
        assert!(!dialog.pending);
        assert!(dialog.conversation.is_empty());
    }

    #[test]
    fn modal_errors_preserve_selection_input_and_confirmation_for_retry() {
        let (mut app, queued, _events) = interaction_app();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let repository = app.project.repository.take();
        let selected = COMMANDS
            .iter()
            .position(|(_, command)| *command == IntentCommand::NewWorktree)
            .unwrap();
        app.modal = Some(Modal::Commands { selected });
        assert!(app.modal_key(enter).is_err());
        assert!(
            matches!(app.modal, Some(Modal::Commands { selected: actual }) if actual == selected)
        );
        app.project.repository = repository;
        app.jobs.try_send(Job::Projects(PathBuf::new())).unwrap();
        app.modal = Some(Modal::NewWorktree {
            input: "typed-branch".into(),
            split: Some(LayoutMode::Vertical),
        });
        assert!(app.modal_key(enter).is_err());
        assert!(
            matches!(&app.modal, Some(Modal::NewWorktree { input, split: Some(LayoutMode::Vertical) }) if input == "typed-branch")
        );
        assert!(app.pending_creations.is_empty());
        queued.recv().unwrap();
        app.modal_key(enter).unwrap();
        assert!(
            matches!(queued.recv().unwrap(), Job::Create { branch, .. } if branch == "typed-branch")
        );
        assert!(app.modal.is_none());
        app.jobs.try_send(Job::Projects(PathBuf::new())).unwrap();
        app.modal = Some(Modal::Confirm(Confirm::StopEnvironment));
        assert!(
            app.modal_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                .is_err()
        );
        assert!(matches!(
            app.modal,
            Some(Modal::Confirm(Confirm::StopEnvironment))
        ));
        app.modal_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.modal.is_none());
        assert!(!app.project.workspaces[0].pending);
    }

    #[test]
    fn failed_creation_restores_branch_and_split_without_overwriting_new_dialogs() {
        let (mut app, queued, _events) = interaction_app();
        let submit = |app: &mut App| {
            app.modal = Some(Modal::NewWorktree {
                input: "existing-branch".into(),
                split: Some(LayoutMode::Horizontal),
            });
            app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap();
        };
        let failed = || Event::Created {
            project: PathBuf::from("/interaction-fixture/.git"),
            source: PathBuf::from("/interaction-fixture/main"),
            preparation_error: None,
            result: Err("branch already checked out".into()),
            agent: None,
            split: Some(LayoutMode::Horizontal),
        };
        submit(&mut app);
        queued.recv().unwrap();
        app.handle(failed()).unwrap();
        assert!(app.pending_creations.is_empty());
        assert!(
            matches!(&app.modal, Some(Modal::NewWorktree { input, split: Some(LayoutMode::Horizontal) }) if input == "existing-branch")
        );
        app.modal_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::NewWorktree { input, .. }) if input == "existing-branc")
        );
        submit(&mut app);
        queued.recv().unwrap();
        app.modal = Some(Modal::Confirm(Confirm::Quit));
        app.handle(failed()).unwrap();
        assert!(matches!(app.modal, Some(Modal::Confirm(Confirm::Quit))));
        assert!(app.pending_creations.is_empty());
        submit(&mut app);
        queued.recv().unwrap();
        app.project.active = 1;
        app.handle(failed()).unwrap();
        assert!(
            app.modal.is_none(),
            "do not restore a dialog onto a different worktree"
        );
    }

    #[test]
    fn creation_completion_does_not_retarget_an_open_confirmation() {
        let (mut app, _queued, _events) = interaction_app();
        app.modal = Some(Modal::Confirm(Confirm::CloseWorktree));
        let mut tree = app.project.workspaces[1].tree.clone();
        tree.id = "created".into();
        app.handle(Event::Created {
            project: PathBuf::from("/interaction-fixture/.git"),
            source: PathBuf::from("/interaction-fixture/main"),
            preparation_error: None,
            result: Ok(tree),
            agent: None,
            split: Some(LayoutMode::Vertical),
        })
        .unwrap();
        assert_eq!(app.focused().tree.id, "main");
        assert!(app.project.layout == LayoutMode::Full);
        assert_eq!(app.project.workspaces.len(), 3);
        assert!(matches!(
            app.modal,
            Some(Modal::Confirm(Confirm::CloseWorktree))
        ));
        app.modal_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.project.workspaces.len(), 3);
    }

    #[test]
    fn collapsing_split_keeps_the_focused_worktree_for_every_entry_point() {
        let (mut app, _queued, _events) = interaction_app();
        for entry in 0..3 {
            app.project.active = 0;
            app.project.secondary = Some(1);
            app.project.focus_secondary = true;
            app.project.layout = if entry == 2 {
                LayoutMode::Horizontal
            } else {
                LayoutMode::Vertical
            };
            match entry {
                0 => app.split(LayoutMode::Vertical).unwrap(),
                1 => app.run_command(IntentCommand::FullWidth).unwrap(),
                _ => app.action(Action::CycleLayout).unwrap(),
            }
            assert!(app.project.layout == LayoutMode::Full);
            assert_eq!(app.focused().tree.id, "other");
            assert_eq!(app.project.active, 1);
            assert!(app.project.secondary.is_none());
            assert!(!app.project.focus_secondary);
        }
    }

    #[test]
    fn hidden_ui_rejects_confirmation_and_input_but_keeps_escape_and_shutdown() {
        let (mut app, queued, _events) = interaction_app();
        let input = |code| {
            Event::Input(crossterm::event::Event::Key(KeyEvent::new(
                code,
                KeyModifiers::NONE,
            )))
        };
        app.handle(Event::Input(crossterm::event::Event::Resize(29, 9)))
            .unwrap();
        assert!(!app.interaction_visible);
        app.modal = Some(Modal::Confirm(Confirm::Quit));
        assert!(!app.handle(input(KeyCode::Char('y'))).unwrap());
        assert!(!app.should_quit);
        assert!(matches!(app.modal, Some(Modal::Confirm(Confirm::Quit))));
        app.handle(input(KeyCode::Esc)).unwrap();
        assert!(app.modal.is_none());
        app.handle(input(KeyCode::F(10))).unwrap();
        assert!(
            app.modal.is_none(),
            "hidden shortcuts must not open invisible dialogs"
        );
        app.modal = Some(Modal::NewWorktree {
            input: "kept".into(),
            split: None,
        });
        app.handle(input(KeyCode::Char('x'))).unwrap();
        app.handle(Event::Input(crossterm::event::Event::Paste(
            "hidden".into(),
        )))
        .unwrap();
        app.handle(input(KeyCode::Enter)).unwrap();
        assert!(matches!(&app.modal, Some(Modal::NewWorktree { input, .. }) if input == "kept"));
        assert!(queued.try_recv().is_err());
        app.handle(input(KeyCode::F(12))).unwrap();
        assert!(app.native_keys);
        assert!(
            app.handle(Event::Input(crossterm::event::Event::Resize(80, 24)))
                .unwrap()
        );
        assert!(app.interaction_visible);
        app.handle(Event::Shutdown).unwrap();
        assert!(app.should_quit);
    }

    #[test]
    fn failed_interpreted_creation_restores_editable_request_and_error() {
        let (mut app, queued, _events) = interaction_app();
        app.open_intent().unwrap();
        queued.recv().unwrap();
        let Some(Modal::Intent(dialog)) = &mut app.modal else {
            panic!("expected interpreter dialog");
        };
        dialog.input = "Create branch taken".into();
        dialog.loading = false;
        dialog.pending = true;
        Arc::make_mut(&mut dialog.conversation).push(serde_json::json!({
            "role": "user", "content": dialog.input,
        }));
        let request = dialog.request;
        app.handle(Event::IntentResolved {
            request,
            result: Ok(Plan::CreateWorktree {
                branch: "taken".into(),
                base: None,
                agent: None,
            }),
        })
        .unwrap();
        assert!(app.modal.is_none());
        assert!(matches!(queued.recv().unwrap(), Job::Create { branch, .. } if branch == "taken"));
        app.handle(Event::Created {
            project: PathBuf::from("/interaction-fixture/.git"),
            source: PathBuf::from("/interaction-fixture/main"),
            preparation_error: None,
            result: Err("branch is already checked out".into()),
            agent: None,
            split: None,
        })
        .unwrap();
        let Some(Modal::Intent(dialog)) = &app.modal else {
            panic!("failed creation must restore the original request");
        };
        assert_eq!(dialog.input, "Create branch taken");
        assert_eq!(
            dialog.error.as_deref(),
            Some("branch is already checked out")
        );
        assert!(dialog.conversation.is_empty());
        assert!(!dialog.pending);
        app.modal_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::Intent(dialog)) if dialog.input == "Create branch take")
        );
    }

    #[test]
    fn removing_an_unfocused_checkout_preserves_confirmation_target_and_split() {
        let (mut app, _queued, _events) = interaction_app();
        let mut third = app.project.workspaces[1].tree.clone();
        third.id = "third".into();
        app.project.workspaces.push(Workspace::new(third));
        app.project.secondary = Some(2);
        app.project.focus_secondary = true;
        app.project.layout = LayoutMode::Vertical;
        app.modal = Some(Modal::Confirm(Confirm::CloseWorktree));
        app.handle(Event::Removed {
            tree: "other".into(),
            result: Ok(()),
        })
        .unwrap();
        assert_eq!(app.focused().tree.id, "third");
        assert!(app.project.layout == LayoutMode::Vertical);
        assert_eq!(app.project.secondary, Some(1));
        assert!(matches!(
            app.modal,
            Some(Modal::Confirm(Confirm::CloseWorktree))
        ));
    }

    fn picker(base: &Path) -> (ProjectPicker, SyncSender<Job>, mpsc::Receiver<Job>) {
        let (tx, rx) = mpsc::sync_channel(16);
        let picker = ProjectPicker::new(base.to_path_buf(), &tx, 0).unwrap();
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
    fn plain_folders_open_as_terminal_tabs_named_after_the_folder() {
        let parent = tempfile::tempdir().unwrap();
        let home = parent.path().join("jyq");
        std::fs::create_dir(&home).unwrap();
        let project = Project::open(&home).unwrap();
        assert!(project.repository.is_none());
        assert_eq!(project.name, "jyq");
        assert_eq!(project.path(), home.canonicalize().unwrap());
        assert!(!home.join(".git").exists());

        // Opening the same folder again lands on the existing tab, not a duplicate.
        let (tx, _events) = mpsc::sync_channel(8);
        let (jobs, _queued) = mpsc::sync_channel(8);
        let mut app = App {
            project: Project::terminal(parent.path()).unwrap(),
            saved_projects: Vec::new(),
            native_keys: false,
            modal: None,
            notice: None,
            panes: Vec::new(),
            scroll_drag: None,
            selection: None,
            tab_hits: Vec::new(),
            interaction_visible: true,
            should_quit: false,
            default_agent: "shell".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
            socket: parent.path().join("agent.sock"),
            tx,
            jobs,
            use_kitty: false,
        };
        app.open_project(project, None).unwrap();
        assert_eq!(app.project.name, "jyq");
        assert_eq!(app.saved_projects.len(), 1);
        app.open_project(Project::open(&home).unwrap(), None)
            .unwrap();
        assert_eq!(app.project.name, "jyq");
        assert_eq!(app.project.workspaces.len(), 1);
        assert_eq!(app.saved_projects.len(), 1);
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
            scroll_drag: None,
            selection: None,
            tab_hits: Vec::new(),
            interaction_visible: true,
            should_quit: false,
            default_agent: "omp".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
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
        assert!(app.split(LayoutMode::Vertical).is_err());
        app.action(Action::NewTab).unwrap();
        assert!(matches!(
            &app.modal,
            Some(Modal::NewTab { choices, selected: 0 })
                if choices.iter().map(|(_, command)| *command).eq([IntentCommand::OpenProject])
        ));
        app.modal = None;
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
    fn split_prompts_for_a_worktree_and_new_tab_offers_projects() {
        let (tx, _events) = mpsc::sync_channel(8);
        let (jobs, queued) = mpsc::sync_channel(8);
        let root = PathBuf::from("/split-fixture");
        let workspace = |branch: &str| {
            Workspace::new(Worktree {
                id: branch.into(),
                path: root.join(branch),
                branch: branch.into(),
                head: String::new(),
                is_main: branch == "main",
                locked: false,
            })
        };
        let mut app = App {
            project: Project {
                repository: Some(worktree::Repository {
                    root: root.clone(),
                    common_dir: root.join(".git"),
                }),
                name: "fixture".into(),
                workspaces: vec![workspace("main")],
                active: 0,
                secondary: None,
                focus_secondary: false,
                layout: LayoutMode::Full,
                order: 0,
            },
            saved_projects: Vec::new(),
            native_keys: false,
            modal: None,
            notice: None,
            panes: Vec::new(),
            scroll_drag: None,
            selection: None,
            tab_hits: Vec::new(),
            interaction_visible: true,
            should_quit: false,
            default_agent: "shell".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
            socket: root.join("agent.sock"),
            tx,
            jobs,
            use_kitty: false,
        };
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);

        // One worktree: the split shortcut goes straight to naming a new one.
        app.action(Action::SplitVertical).unwrap();
        assert!(matches!(
            app.modal,
            Some(Modal::NewWorktree {
                split: Some(LayoutMode::Vertical),
                ..
            })
        ));
        for c in "login-fix".chars() {
            app.modal_key(press(KeyCode::Char(c))).unwrap();
        }
        app.modal_key(press(KeyCode::Enter)).unwrap();
        assert!(app.modal.is_none());
        assert!(matches!(
            queued.recv().unwrap(),
            Job::Create {
                branch,
                base: None,
                agent: None,
                split: Some(LayoutMode::Vertical),
                ..
            } if branch == "login-fix"
        ));

        // Two worktrees: the chooser leads with "new worktree" and keeps the
        // existing ones selectable; the current one is rejected.
        app.project.workspaces.push(workspace("other"));
        app.action(Action::SplitHorizontal).unwrap();
        assert!(matches!(
            app.modal,
            Some(Modal::Split {
                layout: LayoutMode::Horizontal,
                selected: 0
            })
        ));
        app.modal_key(press(KeyCode::Down)).unwrap();
        app.modal_key(press(KeyCode::Enter)).unwrap();
        assert!(matches!(app.modal, Some(Modal::Split { selected: 1, .. })));
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("different worktree")
        );
        app.modal_key(press(KeyCode::Up)).unwrap();
        app.modal_key(press(KeyCode::Enter)).unwrap();
        assert!(matches!(
            app.modal,
            Some(Modal::NewWorktree {
                split: Some(LayoutMode::Horizontal),
                ..
            })
        ));
        app.modal = None;
        assert!(queued.try_recv().is_err());

        // A split that is already showing toggles back to full width.
        app.project.layout = LayoutMode::Vertical;
        app.project.secondary = Some(1);
        app.project.focus_secondary = true;
        app.action(Action::SplitVertical).unwrap();
        assert!(app.modal.is_none());
        assert!(app.project.layout == LayoutMode::Full && !app.project.focus_secondary);

        // The manual command still opens a plain worktree tab, never a split.
        app.run_command(IntentCommand::NewWorktree).unwrap();
        assert!(matches!(
            app.modal,
            Some(Modal::NewWorktree { split: None, .. })
        ));

        app.saved_projects.push(Project {
            repository: None,
            name: "Terminal".into(),
            workspaces: vec![workspace("terminal")],
            active: 0,
            secondary: None,
            focus_secondary: false,
            layout: LayoutMode::Full,
            order: 1,
        });
        app.action(Action::NewTab).unwrap();
        let Some(Modal::NewTab { choices, .. }) = &app.modal else {
            panic!("expected new tab chooser");
        };
        assert_eq!(choices.last().unwrap().1, IntentCommand::SwitchProject);
        for _ in 0..3 {
            app.modal_key(press(KeyCode::Down)).unwrap();
        }
        app.modal_key(press(KeyCode::Enter)).unwrap();
        assert!(matches!(
            app.modal,
            Some(Modal::SwitchProject { selected: 0 })
        ));
    }

    #[test]
    fn tab_bar_lists_every_open_project_in_opening_order_and_tabs_wrap_across_them() {
        let (tx, _events) = mpsc::sync_channel(8);
        let (jobs, queued) = mpsc::sync_channel(8);
        let project = |name: &str, order: u64, branches: &[&str]| Project {
            repository: None,
            name: name.into(),
            workspaces: branches
                .iter()
                .map(|branch| {
                    Workspace::new(Worktree {
                        id: format!("{name}:{branch}"),
                        path: PathBuf::from(format!("/{name}/{branch}")),
                        branch: (*branch).into(),
                        head: String::new(),
                        is_main: false,
                        locked: false,
                    })
                })
                .collect(),
            active: 0,
            secondary: None,
            focus_secondary: false,
            layout: LayoutMode::Full,
            order,
        };
        let mut app = App {
            project: project("alpha", 0, &["main", "dev"]),
            saved_projects: vec![project("beta", 1, &["main"])],
            native_keys: false,
            modal: None,
            notice: None,
            panes: Vec::new(),
            scroll_drag: None,
            selection: None,
            tab_hits: Vec::new(),
            interaction_visible: true,
            should_quit: false,
            default_agent: "shell".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
            socket: PathBuf::from("/tabs-fixture/agent.sock"),
            tx,
            jobs,
            use_kitty: false,
        };
        let names = |app: &App| {
            project_slots(&app.project, &app.saved_projects)
                .map(|(_, project)| project.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&app), ["alpha", "beta"]);
        assert_eq!(
            app.tabs().collect::<Vec<_>>(),
            [(None, 0), (None, 1), (Some(0), 0)]
        );

        // Ctrl+Tab from alpha's last tab lands on beta without reordering the bar.
        app.project.active = 1;
        app.action(Action::NextWorktree).unwrap();
        assert_eq!(app.project.name, "beta");
        assert_eq!(app.focused_index(), 0);
        assert_eq!(names(&app), ["alpha", "beta"]);
        assert_eq!(
            app.tabs().collect::<Vec<_>>(),
            [(Some(0), 0), (Some(0), 1), (None, 0)]
        );
        app.action(Action::NextWorktree).unwrap();
        assert_eq!(
            (app.project.name.as_str(), app.focused_index()),
            ("alpha", 0)
        );
        app.action(Action::PreviousWorktree).unwrap();
        assert_eq!(
            (app.project.name.as_str(), app.focused_index()),
            ("beta", 0)
        );

        // The bar renders both projects; clicking another project's tab switches to it.
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        let row: String = terminal.backend().buffer().content[..100]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        for expected in ["alpha", "1 main", "2 dev", "beta"] {
            assert!(row.contains(expected), "{expected:?} missing from {row:?}");
        }
        let dev = app
            .tab_hits
            .iter()
            .find(|hit| hit.project == Some(0) && hit.workspace == Some(1))
            .map(|hit| hit.area)
            .expect("alpha's dev tab is clickable");
        let click = |column, row| {
            Event::Input(crossterm::event::Event::Mouse(
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ),
                    column,
                    row,
                    modifiers: KeyModifiers::NONE,
                },
            ))
        };
        app.handle(click(dev.x + 1, dev.y)).unwrap();
        assert_eq!(
            (app.project.name.as_str(), app.focused_index()),
            ("alpha", 1)
        );
        assert_eq!(names(&app), ["alpha", "beta"]);

        // Clicking a project name switches to that project's focused worktree.
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        let beta = app
            .tab_hits
            .iter()
            .find(|hit| hit.project == Some(0) && hit.workspace.is_none())
            .map(|hit| hit.area)
            .expect("beta's name is clickable");
        app.handle(click(beta.x + 1, beta.y)).unwrap();
        assert_eq!(app.project.name, "beta");
        assert!(queued.try_recv().is_err());
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
                order: 0,
            },
            saved_projects: Vec::new(),
            native_keys: false,
            modal: None,
            notice: None,
            panes: Vec::new(),
            scroll_drag: None,
            selection: None,
            tab_hits: Vec::new(),
            interaction_visible: true,
            should_quit: false,
            default_agent: "shell".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
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
        assert!(
            picker
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx)
                .is_err()
        );
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
