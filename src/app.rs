use crate::{
    clipboard, compose,
    event::Event,
    integration::{self, AgentState},
    intent::{self, Agent as AgentKind, Command as IntentCommand, Plan},
    keys::{self, Action},
    kitty, limits, restore,
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
    execute, queue,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    },
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
#[derive(Clone)]
pub enum Confirm {
    Quit,
    CloseWorktree {
        selected: usize,
    },
    CloseSession,
    StopEnvironment,
    /// Delete a checkout: `None` is the focused worktree, `Some` the one a worktree pane row names.
    RemoveWorktree {
        tree: Option<Worktree>,
    },
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
        jobs.try_send(Job::Projects {
            request,
            base: base.clone(),
        })
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
                        fresh_tab: false,
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
    /// Navigation uses this exact path; the interpreter sees a lossy copy.
    #[serde(serialize_with = "lossy_path")]
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
    ("Toggle worktree pane", IntentCommand::Worktrees),
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

#[derive(Clone, Copy)]
pub enum NewTabAction {
    Home,
    Command(IntentCommand),
}

pub enum Modal {
    Intent(IntentDialog),
    Commands {
        selected: usize,
    },
    NewTab {
        choices: Vec<(String, NewTabAction)>,
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
    Split {
        layout: LayoutMode,
        selected: usize,
    },
    ExistingSplit {
        layout: LayoutMode,
        choices: Vec<worktree::SplitTarget>,
        selected: usize,
        error: Option<String>,
    },
    /// An accepted checkout operation runs on while this shows; Esc only hides it.
    OpeningSplit {
        request: u64,
    },
    Preview {
        urls: Vec<(String, String)>,
        selected: usize,
    },
    Confirm(Confirm),
    Removal {
        tree: String,
        confirm: Confirm,
        error: Option<String>,
    },
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
        self.tree.label()
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
    pub session: usize,
    pub secondary: bool,
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

#[derive(Clone, Copy)]
pub struct SplitDivider {
    pub track: Rect,
    pub handle: Rect,
    pub available_width: u16,
}

impl SplitDivider {
    const MIN_PANE_WIDTH: u16 = 28;

    pub fn left_width(available_width: u16, ratio: u16) -> u16 {
        let scale = u32::from(restore::SPLIT_RATIO_SCALE);
        let width = (u32::from(available_width) * u32::from(ratio.min(restore::SPLIT_RATIO_SCALE))
            + scale / 2)
            / scale;
        let minimum = Self::MIN_PANE_WIDTH.min(available_width / 2);
        (width as u16).clamp(minimum, available_width - minimum)
    }

    fn ratio_at(self, column: u16, grab: u16) -> u16 {
        let minimum = Self::MIN_PANE_WIDTH.min(self.available_width / 2);
        // Grab offset is relative to the handle's left edge, before the pane boundary.
        let width = (u32::from(column) + u32::from(self.track.x - self.handle.x))
            .saturating_sub(u32::from(grab))
            .clamp(
                u32::from(minimum),
                u32::from(self.available_width - minimum),
            );
        ((width * u32::from(restore::SPLIT_RATIO_SCALE) + u32::from(self.available_width) / 2)
            / u32::from(self.available_width.max(1))) as u16
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

/// The F2 sidebar listing the project's worktrees. It is driven by the mouse; keyboard focus
/// stays in the terminal, and nothing about it is saved with the workspace.
#[derive(Default)]
pub struct WorktreePane {
    /// Index into `Project::trees` of the first listed row; `ui::draw` clamps it to the rows that fit.
    pub scroll: usize,
}

/// What a click on a worktree pane cell does; indices are into `Project::trees`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeHitAction {
    Open(usize),
    Remove(usize),
    Close,
}

/// A clickable cell of the worktree pane, staged by `ui::draw`.
pub struct WorktreeHit {
    pub area: Rect,
    pub action: WorktreeHitAction,
}

pub struct Project {
    pub repository: Option<worktree::Repository>,
    pub name: String,
    pub workspaces: Vec<Workspace>,
    /// Every checkout Git lists for the repository, opened or not, in Git's order; empty outside
    /// Git. The metadata poll keeps it current, and the worktree pane shows it.
    pub trees: Vec<Worktree>,
    pub active: usize,
    pub secondary: Option<usize>,
    /// Independent session in the second pane when both panes share a workspace.
    pub secondary_session: Option<usize>,
    pub focus_secondary: bool,
    pub layout: LayoutMode,
    pub split_ratio: u16,
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
        let trees = worktree::list(&repository)?;
        let active = trees
            .iter()
            .enumerate()
            .filter(|(_, tree)| path.starts_with(&tree.path))
            .max_by_key(|(_, tree)| tree.path.components().count())
            .map(|(index, _)| index)
            .context("folder is not inside a checked-out worktree")?;
        let name = project_name(&trees[0].path, &repository.common_dir);
        let tree = trees[active].clone();
        Ok(Self {
            repository: Some(repository),
            name,
            workspaces: vec![Workspace::new(tree)],
            trees,
            active: 0,
            secondary: None,
            secondary_session: None,
            focus_secondary: false,
            layout: LayoutMode::Full,
            split_ratio: restore::default_split_ratio(),
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
            bare: false,
            prunable: false,
        };
        Ok(Self {
            repository: None,
            name,
            workspaces: vec![Workspace::new(tree)],
            trees: Vec::new(),
            active: 0,
            secondary: None,
            secondary_session: None,
            focus_secondary: false,
            layout: LayoutMode::Full,
            split_ratio: restore::default_split_ratio(),
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

    pub fn pane_session(&self, secondary: bool) -> usize {
        if secondary {
            if let Some(session) = self.secondary_session {
                return session;
            }
            return self.workspaces[self.secondary.unwrap_or(self.active)].selected;
        }
        self.workspaces[self.active].selected
    }

    fn close_workspace(&mut self, index: usize) {
        // Closing the focused primary leaves its visible peer, not the next tab in order.
        let promoted = self
            .secondary
            .filter(|&s| index == self.active && s != index);
        self.workspaces.remove(index);
        if let Some(peer) = promoted {
            self.active = peer - usize::from(peer > index);
            self.secondary = None;
        } else {
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
        }
        if self.secondary.is_none()
            || (self.secondary == Some(self.active) && self.secondary_session.is_none())
        {
            self.layout = LayoutMode::Full;
            self.secondary = None;
            self.secondary_session = None;
            self.focus_secondary = false;
        }
    }

    /// Removes session `session` of workspace `workspace`, as Kitty closes a window. A split
    /// that showed it beside another session of the same workspace leaves that session the full
    /// width, and the focus. A workspace that had it selected selects the nearest remaining
    /// session in its place, so a split of two workspaces stays split. The workspace can be
    /// left without sessions: closing its tab is up to the caller.
    fn remove_session(&mut self, workspace: usize, session: usize) {
        // The other pane, when both show sessions of this workspace and one of them is this one.
        let peer = self
            .secondary_session
            .filter(|_| self.layout != LayoutMode::Full && workspace == self.active)
            .and_then(|shared| {
                let primary = self.workspaces[workspace].selected;
                if session == primary {
                    Some(shared)
                } else if session == shared {
                    Some(primary)
                } else {
                    None
                }
            });
        let shift = |index: usize| index - usize::from(index > session);
        let open = &mut self.workspaces[workspace];
        open.sessions.remove(session);
        if let Some(peer) = peer {
            open.selected = shift(peer);
            self.layout = LayoutMode::Full;
            self.secondary = None;
            self.secondary_session = None;
            self.focus_secondary = false;
            return;
        }
        open.selected = if open.selected == session {
            session.min(open.sessions.len().saturating_sub(1))
        } else {
            shift(open.selected)
        };
        if workspace == self.active {
            self.secondary_session = self.secondary_session.map(shift);
        }
    }
}

pub struct App {
    pub project: Project,
    pub saved_projects: Vec<Project>,
    pub native_keys: bool,
    /// The agent CPU limit (F3), saved with the workspace like `native_keys`.
    pub limiter: limits::Limiter,
    pub modal: Option<Modal>,
    pub notice: Option<String>,
    /// The notice is a warning that terminal input must not dismiss.
    notice_sticky: bool,
    pub panes: Vec<Pane>,
    /// The `(workspace, session)` of each pane the next frame draws, as `ui::resize_sessions`
    /// last sized them. `panes` is only what the last frame drew, and empty after a layout change.
    pub next_panes: Vec<(usize, usize)>,
    scroll_drag: Option<(u64, u16)>,
    /// The session and pane body that received a forwarded press, until its release.
    mouse_capture: Option<(u64, Rect)>,
    pub split_divider: Option<SplitDivider>,
    pub split_drag: Option<u16>,
    pub selection: Option<Selection>,
    pub tab_hits: Vec<TabHit>,
    /// The worktree sidebar while it is open, even where the window is too narrow to draw it.
    pub worktree_pane: Option<WorktreePane>,
    /// Click cells of the worktree pane in the last frame (capacity reused across frames).
    pub worktree_hits: Vec<WorktreeHit>,
    /// Where the last frame drew the worktree pane; every mouse event inside it is the pane's.
    pub worktree_area: Option<Rect>,
    pub interaction_visible: bool,
    pub should_quit: bool,
    default_agent: String,
    next_session: u64,
    next_request: u64,
    pending_creations: VecDeque<(PathBuf, String, Modal)>,
    state_dirty: bool,
    /// The snapshot last written, so unchanged state is not saved again.
    last_saved: Option<restore::Snapshot>,
    metadata_pending: bool,
    metadata_checked: Instant,
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
        fresh_tab: bool,
    },
    IntentTargets {
        request: u64,
        base: PathBuf,
        repository: Option<worktree::Repository>,
        known_projects: Vec<PathBuf>,
    },
    Projects {
        request: u64,
        base: PathBuf,
    },
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
    SplitTargets {
        request: u64,
        repository: worktree::Repository,
        focused: String,
        layout: LayoutMode,
    },
    OpenSplit {
        request: u64,
        repository: worktree::Repository,
        layout: LayoutMode,
        choices: Vec<worktree::SplitTarget>,
        selected: usize,
    },
    RefreshMetadata(Vec<worktree::Repository>),
    Environment {
        tree: Worktree,
        operation: EnvironmentOp,
        source: Option<PathBuf>,
    },
    Remove {
        repository: worktree::Repository,
        tree: Worktree,
    },
    /// Apply the agent CPU limit's setting, as it is when this runs, to the running agents.
    RetuneAgentCpu(limits::Limiter),
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

/// One-line Compose status for the notice row, e.g. `main: + web :8080 · ~ db · - migrate · ! worker`:
/// `+` running, `~` still starting, `-` finished with exit code 0, `!` failing.
fn environment_summary(label: &str, environment: &compose::Environment) -> String {
    let mut summary = format!("{label}: ");
    if environment.services.is_empty() {
        summary.push_str("stack stopped");
    }
    for (index, service) in environment.services.iter().enumerate() {
        let marker = match (service.state.as_str(), service.health.as_str()) {
            ("running", "" | "healthy") => "+ ",
            ("running", "starting") => "~ ",
            ("exited", _) if service.exit_code == 0 => "- ",
            _ => "! ",
        };
        if index > 0 {
            summary.push_str(" · ");
        }
        summary.push_str(marker);
        summary.push_str(&service.name);
        for port in &service.ports {
            let _ = write!(summary, " :{}", port.host);
        }
    }
    summary
}

/// Shown when a new worktree's tab exists but the full job queue refused its Docker start.
const ENVIRONMENT_NOT_STARTED: &str = "Worktree created; its Docker environment was not started (command queue busy). Start it from F1 → F2.";

/// Shown when a tab closes. Closing one never touches its checkout or Docker environment.
const TAB_CLOSED: &str = "Tab closed; any checkout and Docker environment retained.";

impl App {
    const METADATA_INTERVAL: Duration = Duration::from_secs(1);
    /// An agent that ends successfully within this long of starting stays open, as a failed one
    /// does, rather than closing like a Kitty window: it most likely never got going (a tool that
    /// refuses to run can still exit 0), and closing it could end Crit before anyone saw why.
    const AGENT_STARTUP: Duration = Duration::from_secs(1);

    fn refresh_metadata(&mut self) {
        if self.metadata_pending || self.metadata_checked.elapsed() < Self::METADATA_INTERVAL {
            return;
        }
        self.metadata_checked = Instant::now();
        let repositories: Vec<_> = std::iter::once(&self.project)
            .chain(&self.saved_projects)
            .filter_map(|project| project.repository.clone())
            .collect();
        if !repositories.is_empty()
            && self
                .jobs
                .try_send(Job::RefreshMetadata(repositories))
                .is_ok()
        {
            self.metadata_pending = true;
        }
    }

    fn apply_metadata(&mut self, listings: Vec<(PathBuf, Vec<Worktree>)>) -> bool {
        self.metadata_pending = false;
        self.metadata_checked = Instant::now();
        let mut changed = false;
        let mut moved = false;
        let pane_open = self.worktree_pane.is_some();
        for (slot, project) in std::iter::once(&mut self.project)
            .chain(&mut self.saved_projects)
            .enumerate()
        {
            let Some(trees) = project
                .common_dir()
                .and_then(|common_dir| listings.iter().find(|(listed, _)| listed == common_dir))
                .map(|(_, trees)| trees)
            else {
                continue;
            };
            if project.trees != *trees {
                project.trees.clone_from(trees);
                // Only the current project's list can be on screen, and only with the pane open.
                changed |= slot == 0 && pane_open;
            }
            for index in 0..project.workspaces.len() {
                let current = &project.workspaces[index];
                if let Some(tree) = trees
                    .iter()
                    .find(|tree| tree.id == current.tree.id && tree.path == current.tree.path)
                {
                    if current.tree.branch != tree.branch || current.tree.head != tree.head {
                        let workspace = &mut project.workspaces[index];
                        workspace.tree.branch.clone_from(&tree.branch);
                        workspace.tree.head.clone_from(&tree.head);
                        changed = true;
                    }
                    continue;
                }
                // `git worktree move` keeps the branch but changes the path, and with it the
                // id. Follow the checkout when exactly one unopened one carries that branch.
                if current.tree.branch.is_empty() || current.pending {
                    continue;
                }
                let mut unopened = trees.iter().filter(|tree| {
                    tree.branch == current.tree.branch
                        && !project
                            .workspaces
                            .iter()
                            .any(|open| open.tree.id == tree.id)
                });
                if let (Some(tree), None) = (unopened.next(), unopened.next()) {
                    project.workspaces[index].tree = tree.clone();
                    changed = true;
                    moved = true;
                }
            }
        }
        self.state_dirty |= moved;
        changed
    }

    fn workspace_changed(&mut self) {
        // Hit targets belong to the last frame, not the newly selected topology.
        self.panes.clear();
        self.scroll_drag = None;
        self.split_divider = None;
        self.split_drag = None;
        self.selection = None;
        self.tab_hits.clear();
        self.worktree_hits.clear();
        self.state_dirty = true;
    }

    /// Lets go of the split divider, if it is being dragged; returns whether it was.
    pub fn finish_split_drag(&mut self) -> bool {
        let dragging = self.split_drag.take().is_some();
        self.state_dirty |= dragging;
        dragging
    }

    /// Opens or closes the worktree pane. The terminal's width changes with it, so the last
    /// frame's hit targets are dropped; the pane is never saved, so no save is requested.
    fn set_worktree_pane(&mut self, open: bool) {
        if open == self.worktree_pane.is_some() {
            return;
        }
        self.worktree_pane = open.then(WorktreePane::default);
        self.worktree_area = None;
        let pending_save = self.state_dirty;
        self.workspace_changed();
        self.state_dirty = pending_save;
    }

    /// Lists a checkout Git just created or reported, ahead of the next metadata poll.
    fn note_tree(&mut self, project: &Path, tree: &Worktree) {
        if let Some(project) = std::iter::once(&mut self.project)
            .chain(&mut self.saved_projects)
            .find(|open| open.common_dir() == Some(project))
            && !project.trees.iter().any(|known| known.id == tree.id)
        {
            project.trees.push(tree.clone());
        }
    }

    /// A mouse event inside the worktree pane. None of it reaches a terminal.
    fn worktree_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> Result<bool> {
        match mouse.kind {
            MouseEventKind::Moved => return Ok(false),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if let Some(pane) = &mut self.worktree_pane {
                    pane.scroll = if mouse.kind == MouseEventKind::ScrollUp {
                        pane.scroll.saturating_sub(1)
                    } else {
                        (pane.scroll + 1).min(self.project.trees.len().saturating_sub(1))
                    };
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = self
                    .worktree_hits
                    .iter()
                    .find(|hit| hit.area.contains((mouse.column, mouse.row).into()))
                    .map(|hit| hit.action);
                if let Some(action) = hit {
                    self.worktree_click(action)?;
                }
            }
            _ => {}
        }
        Ok(true)
    }

    /// The listing can change between frames, so a click's index is looked up afresh and a
    /// stale one names nothing.
    fn worktree_click(&mut self, action: WorktreeHitAction) -> Result<()> {
        match action {
            WorktreeHitAction::Close => self.set_worktree_pane(false),
            WorktreeHitAction::Open(index) => {
                let Some(tree) = self.project.trees.get(index) else {
                    return Ok(());
                };
                if !tree.is_checkout() {
                    let message = if tree.prunable {
                        "This checkout is missing on disk; delete it or recreate the branch."
                    } else {
                        "A bare repository has no files to open."
                    };
                    self.set_notice(message);
                    return Ok(());
                }
                // The same reuse rule as `open_project`: one tab per checkout.
                let workspace = match self
                    .project
                    .workspaces
                    .iter()
                    .position(|workspace| workspace.tree.id == tree.id)
                {
                    Some(workspace) => workspace,
                    None => {
                        self.project.workspaces.push(Workspace::new(tree.clone()));
                        self.project.workspaces.len() - 1
                    }
                };
                self.select_workspace(workspace, None)?;
            }
            WorktreeHitAction::Remove(index) => {
                let Some(tree) = self.project.trees.get(index) else {
                    return Ok(());
                };
                if tree.is_main || tree.bare {
                    self.set_notice("The main checkout cannot be removed.");
                } else if tree.locked {
                    self.set_notice("This worktree is locked.");
                } else {
                    self.modal = Some(Modal::Confirm(Confirm::RemoveWorktree {
                        tree: Some(tree.clone()),
                    }));
                }
            }
        }
        Ok(())
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

    /// Sends a drag or release to the program that took the press, clamped to its pane body.
    fn forward_capture(
        &self,
        mouse: crossterm::event::MouseEvent,
        id: u64,
        body: Rect,
    ) -> Result<()> {
        if let Some(session) = self.panes.iter().find_map(|pane| {
            let workspace = self.project.workspaces.get(pane.workspace)?;
            let session = workspace.sessions.get(pane.session)?;
            (session.id == id).then_some(session)
        }) {
            let column = mouse.column.clamp(body.x, body.right() - 1) - body.x;
            let row = mouse.row.clamp(body.y, body.bottom() - 1) - body.y;
            session.mouse(mouse, column, row)?;
        }
        Ok(())
    }

    /// Reads the system clipboard on a worker thread, so a hung clipboard owner cannot
    /// stall the UI. The text returns as `Event::ClipboardText` for `target`: the session
    /// that asked, or `None` for the dialog that was open then.
    fn paste_from_clipboard(&self, target: Option<u64>) -> Result<()> {
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("clipboard-read".into())
            .spawn(move || {
                let result = clipboard::paste().map_err(|error| format!("{error:#}"));
                let _ = tx.send(Event::ClipboardText { target, result });
            })?;
        Ok(())
    }

    fn snapshot(&self) -> restore::Snapshot {
        let mut active_project = 0;
        let projects = project_slots(&self.project, &self.saved_projects)
            // The snapshot stores UTF-8 paths. A project with a path that is not UTF-8 is not
            // saved, rather than making every later save fail.
            .filter(|(_, project)| {
                !project.workspaces.is_empty()
                    && project
                        .workspaces
                        .iter()
                        .all(|workspace| workspace.tree.path.to_str().is_some())
            })
            .enumerate()
            .map(|(index, (slot, project))| {
                if slot.is_none() {
                    active_project = index;
                }
                let mut secondary_session = None;
                let workspaces = project
                    .workspaces
                    .iter()
                    .enumerate()
                    .map(|(workspace_index, workspace)| {
                        let mut selected = 0;
                        let mut selected_restorable = false;
                        let sessions: Vec<_> = workspace
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
                                    selected_restorable = true;
                                }
                                if workspace_index == project.active
                                    && Some(index) == project.secondary_session
                                {
                                    secondary_session = Some(saved_index);
                                }
                                restore::SavedSession {
                                    // A shell running an agent's TUI reopens as that agent.
                                    kind: match session.kind.as_str() {
                                        "shell" => session.foreground_agent().unwrap_or("shell"),
                                        kind => kind,
                                    }
                                    .into(),
                                    resume: session.resume.clone(),
                                }
                            })
                            .collect();
                        // The fallback index 0 collides with a peer that is the first restorable
                        // terminal; any other restorable terminal can lead the split instead.
                        if workspace_index == project.active
                            && !selected_restorable
                            && secondary_session == Some(selected)
                            && sessions.len() > 1
                        {
                            selected = 1;
                        }
                        restore::SavedWorkspace {
                            path: workspace.tree.path.clone(),
                            source: workspace
                                .source
                                .clone()
                                .filter(|source| source.to_str().is_some()),
                            sessions,
                            selected,
                        }
                    })
                    .collect::<Vec<_>>();
                // Non-restorable sessions (for example Docker logs) must never
                // turn a saved split into two views of the same terminal.
                let keep_split = project.secondary_session.is_none()
                    || secondary_session
                        .is_some_and(|selected| selected != workspaces[project.active].selected);
                restore::SavedProject {
                    workspaces,
                    active: project.active,
                    secondary: project.secondary.filter(|_| keep_split),
                    secondary_session: secondary_session.filter(|_| keep_split),
                    focus_secondary: keep_split && project.focus_secondary,
                    layout: match if keep_split {
                        project.layout
                    } else {
                        LayoutMode::Full
                    } {
                        LayoutMode::Full => restore::SavedLayout::Full,
                        LayoutMode::Vertical => restore::SavedLayout::Vertical,
                        LayoutMode::Horizontal => restore::SavedLayout::Horizontal,
                    },
                    split_ratio: project.split_ratio,
                }
            })
            .collect();
        restore::Snapshot {
            version: restore::VERSION,
            projects,
            active_project,
            native_keys: self.native_keys,
            agent_cpu_limited: self.limiter.limited(),
            agent_cpu_percent: self.limiter.percent(),
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
        let snapshot = self.snapshot();
        if self.last_saved.as_ref() == Some(&snapshot) {
            return false;
        }
        if let Err(error) = owner.save(&snapshot) {
            self.set_warning(format!(
                "Workspace could not be saved: {error:#}. Saving disabled for this window."
            ));
            *store = None;
            return true;
        }
        self.last_saved = Some(snapshot);
        false
    }

    /// Whether a pane the next frame draws should wait: its app is partway through a
    /// synchronized update (DEC mode 2026), or has not repainted since the pane changed size.
    /// Drawing now would show half a repaint, so the frame waits for the app, or for the holds
    /// `Session::holds_frame` allows to run out. `next_panes` says what the frame draws, even
    /// right after a layout change; `panes` is what the last frame drew, and the fallback.
    /// A divider being dragged resizes its panes with every pointer move: they are drawn as they
    /// are, and waited for once it is let go.
    fn frame_held(&self) -> bool {
        let dragging = self.split_drag.is_some();
        let held = |workspace: usize, session: usize| {
            self.project
                .workspaces
                .get(workspace)
                .and_then(|workspace| workspace.sessions.get(session))
                .is_some_and(|session| {
                    if dragging {
                        session.synchronized()
                    } else {
                        session.holds_frame()
                    }
                })
        };
        if self.next_panes.is_empty() {
            self.panes
                .iter()
                .any(|pane| held(pane.workspace, pane.session))
        } else {
            self.next_panes
                .iter()
                .any(|&(workspace, session)| held(workspace, session))
        }
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
    pub fn focused_session_index(&self) -> usize {
        self.project
            .pane_session(self.project.layout != LayoutMode::Full && self.project.focus_secondary)
    }
    pub fn focused_session(&self) -> Option<&Session> {
        self.focused().sessions.get(self.focused_session_index())
    }

    fn select_session(&mut self, selected: usize) {
        let index = self.focused_index();
        if self.project.secondary_session.is_some() {
            let other = self.project.pane_session(!self.project.focus_secondary);
            if selected == other {
                self.project.focus_secondary = !self.project.focus_secondary;
            } else if self.project.focus_secondary {
                self.project.secondary_session = Some(selected);
            } else {
                self.project.workspaces[index].selected = selected;
            }
        } else {
            self.project.workspaces[index].selected = selected;
        }
        self.workspace_changed();
    }
    fn set_notice(&mut self, message: impl Into<String>) {
        const NOTICE_TAIL: usize = 16 * 1024;
        let mut message = message.into();
        if message.len() > NOTICE_TAIL {
            // Only the final rows are ever drawn; bound the per-frame wrapping work.
            let mut start = message.len() - NOTICE_TAIL;
            while !message.is_char_boundary(start) {
                start += 1;
            }
            message.replace_range(..start, "...");
        }
        self.notice = Some(message);
        self.notice_sticky = false;
    }

    /// A notice that terminal input does not dismiss: saving is off, or saved items were
    /// left out at startup. Esc or a Crit shortcut dismisses it, and any later notice replaces it.
    fn set_warning(&mut self, message: impl Into<String>) {
        self.set_notice(message);
        self.notice_sticky = true;
    }

    /// Adds `message` below what the notice row already says, keeping a warning a warning. For
    /// news that can arrive at any time, such as a failed update: it must not hide a startup
    /// warning, or an earlier failure, that the user has yet to read.
    fn append_notice(&mut self, message: &str) {
        let Some(shown) = self.notice.take() else {
            self.set_notice(message);
            return;
        };
        let sticky = self.notice_sticky;
        self.set_notice(format!("{shown}\n{message}"));
        self.notice_sticky = sticky;
    }

    /// Typing, pasting or clicking acknowledges an ordinary notice; a warning needs Esc.
    /// Returns whether a notice was cleared.
    fn clear_notice_on_input(&mut self, escape: bool) -> bool {
        (escape || !self.notice_sticky) && self.notice.take().is_some()
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

    /// The project in `slot`: `None` is the current project, `Some(i)` is `saved_projects[i]`.
    fn project_mut(&mut self, slot: Option<usize>) -> &mut Project {
        match slot {
            None => &mut self.project,
            Some(index) => &mut self.saved_projects[index],
        }
    }

    /// Where session `id` is, in any open project: the project's slot (as `project_mut` takes
    /// it), and the indices of its workspace and of the session in it.
    fn find_session(&self, id: u64) -> Option<(Option<usize>, usize, usize)> {
        project_slots(&self.project, &self.saved_projects).find_map(|(slot, project)| {
            project
                .workspaces
                .iter()
                .enumerate()
                .find_map(|(workspace, open)| {
                    let session = open.sessions.iter().position(|s| s.id == id)?;
                    Some((slot, workspace, session))
                })
        })
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

    /// `fresh_tab` (Home) opens another tab for a plain folder even if it is already open,
    /// and another session in the tab of a Git checkout, whose id must stay unique.
    fn open_project(
        &mut self,
        mut project: Project,
        agent: Option<AgentKind>,
        fresh_tab: bool,
    ) -> Result<()> {
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
        let reused = self
            .project
            .workspaces
            .iter()
            .position(|w| w.tree.id == tree.id)
            .filter(|_| !(fresh_tab && self.project.repository.is_none()));
        let index = if let Some(index) = reused {
            index
        } else {
            self.project.workspaces.push(Workspace::new(tree));
            self.project.workspaces.len() - 1
        };
        self.full_width();
        self.modal = None;
        self.notice = None;
        if fresh_tab && reused.is_some() {
            let kind = self.default_agent.clone();
            self.spawn(index, &kind, None)?;
        }
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
            Some(&self.limiter),
        )?;
        self.next_session += 1;
        let workspace = &mut self.project.workspaces[index];
        workspace.sessions.push(session);
        let selected = workspace.sessions.len() - 1;
        if index == self.focused_index() {
            self.select_session(selected);
        } else {
            self.project.workspaces[index].selected = selected;
            self.state_dirty = true;
        }
        Ok(())
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
        if let Some(selected) = self.project.secondary_session
            && index != self.project.active
        {
            if !self.project.focus_secondary {
                self.project.workspaces[self.project.active].selected = selected;
            }
            self.project.secondary_session = None;
        }
        if self.project.secondary_session.is_some() {
            // Selecting the shared tab keeps its currently focused terminal.
        } else if self.project.layout != LayoutMode::Full && self.project.secondary == Some(index) {
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
                self.select_session(selected);
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
            // Best effort: the selection above already happened, and a probe is only a
            // silent background check.
            let _ = self.env(index, EnvironmentOp::Probe);
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
        let selected = self.focused_session_index();
        self.workspace_changed();
        self.project.active = self.focused_index();
        self.project.workspaces[self.project.active].selected = selected;
        self.project.layout = LayoutMode::Full;
        self.project.secondary = None;
        self.project.secondary_session = None;
        self.project.focus_secondary = false;
    }

    fn split(&mut self, layout: LayoutMode) -> Result<()> {
        if self.project.layout == layout {
            self.full_width();
            return Ok(());
        }
        self.modal = Some(Modal::Split {
            layout,
            selected: 0,
        });
        Ok(())
    }

    fn split_terminal(&mut self, layout: LayoutMode) -> Result<()> {
        let index = self.focused_index();
        self.ensure_session(index)?;
        let selected = self.focused_session_index();
        let kind = match self.focused_session().map(|session| session.kind.as_str()) {
            Some("omp") => "omp",
            Some("opencode") => "opencode",
            _ => "shell",
        };
        self.spawn(index, kind, None)?;
        let secondary = self.project.workspaces[index].sessions.len() - 1;
        self.workspace_changed();
        self.project.workspaces[index].selected = selected;
        self.project.active = index;
        self.project.secondary = Some(index);
        self.project.secondary_session = Some(secondary);
        self.project.focus_secondary = true;
        self.project.layout = layout;
        Ok(())
    }
    /// Shows `index` next to the focused worktree; `focus_new` moves focus into it.
    fn split_with(&mut self, layout: LayoutMode, index: usize, focus_new: bool) -> Result<()> {
        self.full_width();
        self.project.secondary = Some(index);
        self.project.focus_secondary = focus_new;
        self.project.layout = layout;
        self.ensure_session(index)?;
        let workspace = &self.project.workspaces[index];
        if workspace.environment.is_none()
            && !workspace.pending
            && compose::has_config(&workspace.tree)
        {
            // Best effort, like the probe in `select_workspace`: the split already exists.
            let _ = self.env(index, EnvironmentOp::Probe);
        }
        Ok(())
    }
    fn new_tab_choices(&self) -> Vec<(String, NewTabAction)> {
        let mut choices = Vec::with_capacity(4);
        choices.push(("Home (~)".to_owned(), NewTabAction::Home));
        choices.push((
            "Open folder — no Git required".to_owned(),
            NewTabAction::Command(IntentCommand::OpenProject),
        ));
        if self.project.repository.is_some() {
            choices.push((
                format!("New branch / worktree in {}", self.project.name),
                NewTabAction::Command(IntentCommand::NewWorktree),
            ));
            choices.push((
                format!("Open existing worktree of {}", self.project.name),
                NewTabAction::Command(IntentCommand::OpenWorktree),
            ));
        }
        choices
    }
    /// Closes the focused tab, keeping its checkout and Docker environment.
    fn close_workspace(&mut self) {
        self.close_tab(None, self.focused_index());
        if !self.should_quit {
            self.set_notice(TAB_CLOSED);
        }
    }

    /// Closes tab `index` of the project in `slot` (as `project_mut` takes it), keeping its
    /// checkout and Docker environment. A project closes with its last tab: the current one
    /// gives way to the most recently opened other project, and without one Crit ends, as Kitty
    /// does when its last window closes. Crit's final save then holds no project, so the next
    /// launch starts at the chooser.
    fn close_tab(&mut self, slot: Option<usize>, index: usize) {
        self.workspace_changed();
        let project = self.project_mut(slot);
        if project.workspaces.len() > 1 {
            project.close_workspace(index);
            return;
        }
        match slot {
            Some(saved) => {
                self.saved_projects.remove(saved);
            }
            None => match self.saved_projects.pop() {
                Some(project) => self.project = project,
                None => {
                    self.project.workspaces.clear();
                    self.should_quit = true;
                }
            },
        }
    }

    /// Closes session `session` of workspace `workspace` in the project in `slot` (as
    /// `project_mut` takes it), as Kitty closes a window: its pane gives way as
    /// `Project::remove_session` describes, and a tab left without sessions closes as
    /// `close_tab` describes, which can end Crit.
    fn close_session(
        &mut self,
        slot: Option<usize>,
        workspace: usize,
        session: usize,
    ) -> Result<()> {
        let focused = slot.is_none()
            && workspace == self.focused_index()
            && session == self.focused_session_index();
        let project = self.project_mut(slot);
        project.remove_session(workspace, session);
        let emptied = project.workspaces[workspace].sessions.is_empty();
        self.workspace_changed();
        if emptied {
            self.close_tab(slot, workspace);
            if self.should_quit {
                return Ok(());
            }
            // Below what the notice row says: a program can end while it shows a warning.
            self.append_notice(TAB_CLOSED);
        }
        // A confirmation for what had the focus must not go on to act on what has it now.
        let stale = match &self.modal {
            Some(Modal::Confirm(Confirm::CloseSession)) => focused,
            Some(Modal::Confirm(
                Confirm::CloseWorktree { .. }
                | Confirm::StopEnvironment
                | Confirm::RemoveWorktree { tree: None },
            )) => focused && emptied,
            _ => false,
        };
        if stale {
            self.modal = None;
        }
        if focused && emptied {
            // As after Close tab, the tab that takes over the window gets a session.
            self.ensure_session(self.focused_index())?;
        }
        Ok(())
    }

    /// Session `id`'s program ended. A shell's pane closes like a Kitty window however it
    /// ended, and an agent's does when it quit (Ctrl+D or its own quit command) after
    /// `AGENT_STARTUP`. A failed agent stays open with its exit status in its header, and so
    /// does a Docker log follower, which ends along with the stack it follows.
    fn session_exited(
        &mut self,
        id: u64,
        code: String,
        success: bool,
        ran: Duration,
    ) -> Result<()> {
        // A session closed in Crit is gone before its program's exit is reported.
        let Some((slot, workspace, index)) = self.find_session(id) else {
            return Ok(());
        };
        let session = &mut self.project_mut(slot).workspaces[workspace].sessions[index];
        let closes = match session.kind.as_str() {
            "shell" => true,
            "omp" | "opencode" => success && ran >= Self::AGENT_STARTUP,
            _ => false,
        };
        if closes {
            return self.close_session(slot, workspace, index);
        }
        session.state = AgentState::Exited;
        session.message = Some(code);
        Ok(())
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
                "path": self.project.path().to_string_lossy(),
                "common_dir": self.project.common_dir().map(Path::to_string_lossy),
                "is_git": self.project.repository.is_some(),
            },
            "focused_worktree": {
                "id": self.focused().tree.id,
                "path": self.focused().tree.path.to_string_lossy(),
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

    fn open_path(
        &mut self,
        path: PathBuf,
        agent: Option<AgentKind>,
        fresh_tab: bool,
    ) -> Result<()> {
        let request = self.new_request();
        self.jobs
            .try_send(Job::OpenProject {
                request,
                path,
                agent,
                fresh_tab,
            })
            .context("command queue is busy")?;
        self.modal = Some(Modal::Loading {
            request,
            message: "Opening project...",
            retry: None,
        });
        Ok(())
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
                let notice = format!("Creating {branch} in a separate worktree...");
                self.jobs
                    .try_send(Job::Create {
                        repository,
                        branch,
                        base,
                        agent,
                        split: None,
                    })
                    .context("command queue is busy")?;
                self.set_notice(notice);
            }
            Plan::Open { target, agent } => {
                let target = targets
                    .iter()
                    .find(|candidate| candidate.id == target)
                    .context("The interpreter selected an unknown project or worktree")?;
                self.open_path(target.path.clone(), agent, false)?;
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
            Worktrees => self.action(Action::Worktrees)?,
            StartEnvironment => self.env(index, EnvironmentOp::Up)?,
            StopEnvironment => self.modal = Some(Modal::Confirm(Confirm::StopEnvironment)),
            RestartEnvironment => self.env(index, EnvironmentOp::Restart)?,
            RefreshEnvironment => self.env(index, EnvironmentOp::Status)?,
            EnvironmentLogs => {
                let mut args = compose::logs_args(&self.project.workspaces[index].tree)?;
                // The pane keeps 512 rows of history, so older lines would only be parsed and
                // dropped.
                args.extend(["--tail".to_owned(), "512".to_owned()]);
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
            RemoveWorktree => {
                self.modal = Some(Modal::Confirm(Confirm::RemoveWorktree { tree: None }))
            }
            Quit => self.action(Action::Quit)?,
            ToggleNative => self.action(Action::ToggleNative)?,
        }
        Ok(())
    }

    /// F3: turns the agent CPU limit off or on, for running agents as far as it can, and says
    /// what it does now.
    fn toggle_agent_cpu(&mut self) -> Result<()> {
        let limited = !self.limiter.limited();
        self.limiter.set_limited(limited);
        if self.limiter.uses_scope() {
            // systemd can take a while to answer, so the worker retunes the running agents, to
            // the setting as it is by then.
            if self
                .jobs
                .try_send(Job::RetuneAgentCpu(self.limiter.clone()))
                .is_err()
            {
                self.limiter.set_limited(!limited);
                anyhow::bail!("command queue is busy; press F3 again");
            }
        } else if limited {
            // Without a scope, running agents can only be given a lower priority.
            let sessions = std::iter::once(&self.project)
                .chain(&self.saved_projects)
                .flat_map(|project| &project.workspaces)
                .flat_map(|workspace| &workspace.sessions);
            for session in sessions {
                if matches!(session.kind.as_str(), "omp" | "opencode")
                    && session.alive.load(Ordering::Acquire)
                    && let Some(pid) = session.pid()
                {
                    limits::lower_priority(pid);
                }
            }
        }
        self.state_dirty = true;
        self.set_notice(self.limiter.describe());
        Ok(())
    }

    fn action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::ToggleNative => {
                self.native_keys = !self.native_keys;
                self.state_dirty = true;
                if !matches!(
                    &self.modal,
                    Some(Modal::Removal { .. } | Modal::OpeningSplit { .. })
                ) {
                    self.modal = None;
                }
                if self.use_kitty {
                    kitty::set_native(&mut io::stdout(), self.native_keys)?;
                }
            }
            Action::Launcher => self.open_intent()?,
            Action::Worktrees => self.set_worktree_pane(self.worktree_pane.is_none()),
            Action::ToggleAgentCpu => self.toggle_agent_cpu()?,
            Action::Quit => self.modal = Some(Modal::Confirm(Confirm::Quit)),
            Action::NewTab => {
                self.modal = Some(Modal::NewTab {
                    choices: self.new_tab_choices(),
                    selected: 0,
                })
            }
            Action::CloseWorktree => {
                self.modal = Some(Modal::Confirm(Confirm::CloseWorktree { selected: 2 }))
            }
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
                    self.select_session(index);
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

    /// Esc on a progress dialog only hides it: the job keeps running and its result
    /// arrives as a notice.
    fn note_hidden_progress(&mut self, modal: &Modal) {
        match modal {
            Modal::Removal {
                tree, error: None, ..
            } => {
                let label = self
                    .all_workspaces_mut()
                    .find(|workspace| workspace.tree.id == *tree)
                    .map_or_else(
                        || "the checkout".to_owned(),
                        |workspace| workspace.label().to_owned(),
                    );
                self.set_notice(format!(
                    "Still removing {label}; the result will appear here."
                ));
            }
            Modal::OpeningSplit { .. } => {
                self.set_notice("Still opening the split; the checkout will open as a tab.");
            }
            _ => {}
        }
    }

    fn modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(mut modal) = self.modal.take() else {
            return Ok(());
        };
        if key.code == KeyCode::Esc {
            self.note_hidden_progress(&modal);
            if let Modal::ExistingSplit { layout, .. } = modal {
                self.modal = Some(Modal::Split {
                    layout,
                    selected: 2,
                });
            }
            return Ok(());
        }
        let mut retain = true;
        let result = (|| -> Result<()> {
            match &mut modal {
                Modal::Loading { .. } | Modal::OpeningSplit { .. } => {}
                Modal::Removal { confirm, error, .. } => {
                    if error.is_some() && key.code == KeyCode::Enter {
                        self.modal = Some(Modal::Confirm(match confirm {
                            Confirm::CloseWorktree { .. } => Confirm::CloseWorktree { selected: 2 },
                            other => other.clone(),
                        }));
                        retain = false;
                    }
                }
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
                        if let Some(&(_, action)) = choices.get(*selected) {
                            match action {
                                NewTabAction::Home => {
                                    let home = project_path("~", &self.focused().tree.path)?;
                                    self.open_path(home, None, true)?;
                                }
                                NewTabAction::Command(command) => self.run_command(command)?,
                            }
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
                        // A failure after the swap must not bring the picker back: its
                        // indices describe the order from before.
                        retain = false;
                        self.switch_project(*selected, None)?;
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
                        if let Some(picked) = trees.get(*selected) {
                            // The picker listed when it opened; the Git poll has kept
                            // `Project::trees` current since, and only sends another listing
                            // when something changes again. A branch switched meanwhile would
                            // otherwise label the new tab until then.
                            let tree = self
                                .project
                                .trees
                                .iter()
                                .find(|tree| tree.id == picked.id && tree.path == picked.path)
                                .unwrap_or(picked);
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
                    navigate(key, selected, 3);
                    if key.code == KeyCode::Enter {
                        if *selected == 0 {
                            self.git_repository()?;
                            self.modal = Some(Modal::NewWorktree {
                                input: String::new(),
                                split: Some(*layout),
                            });
                        } else if *selected == 1 {
                            self.split_terminal(*layout)?;
                        } else {
                            let repository = self.focused_repository()?;
                            let request = self.new_request();
                            self.jobs
                                .try_send(Job::SplitTargets {
                                    request,
                                    repository,
                                    focused: self.focused().tree.id.clone(),
                                    layout: *layout,
                                })
                                .context("command queue is busy")?;
                            self.modal = Some(Modal::Loading {
                                request,
                                message: "Loading branches and worktrees...",
                                retry: None,
                            });
                        }
                        retain = false;
                    }
                }
                Modal::ExistingSplit {
                    layout,
                    choices,
                    selected,
                    error,
                } => {
                    let previous = *selected;
                    navigate(key, selected, choices.len());
                    if previous != *selected {
                        *error = None;
                    }
                    if key.code == KeyCode::Enter && *selected < choices.len() {
                        let repository = self.focused_repository()?;
                        let request = self.new_request();
                        let job = Job::OpenSplit {
                            request,
                            repository,
                            layout: *layout,
                            choices: std::mem::take(choices),
                            selected: *selected,
                        };
                        if let Err(failure) = self.jobs.try_send(job) {
                            let (mpsc::TrySendError::Full(job)
                            | mpsc::TrySendError::Disconnected(job)) = failure;
                            if let Job::OpenSplit {
                                choices: returned, ..
                            } = job
                            {
                                *choices = returned;
                            }
                            anyhow::bail!("command queue is busy");
                        }
                        self.modal = Some(Modal::OpeningSplit { request });
                        retain = false;
                    }
                }
                Modal::Preview { urls, selected } => {
                    navigate(key, selected, urls.len());
                    if key.code == KeyCode::Enter {
                        if let Some((_, url)) = urls.get(*selected) {
                            self.open_link(url.clone())?;
                        }
                        retain = false;
                    }
                }
                Modal::Confirm(confirm) => {
                    if let Confirm::CloseWorktree { selected } = confirm {
                        navigate(key, selected, 3);
                    }
                    let choice = match key.code {
                        KeyCode::Char('y' | 'Y') => Some(0),
                        KeyCode::Char('n' | 'N') => Some(2),
                        KeyCode::Enter => match confirm {
                            Confirm::CloseWorktree { selected } => Some(*selected),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(choice @ (0 | 1)) = choice {
                        let index = self.focused_index();
                        match confirm {
                            Confirm::Quit => self.should_quit = true,
                            Confirm::CloseWorktree { .. } if choice == 0 => {
                                retain = false;
                                self.close_workspace();
                                if !self.should_quit {
                                    self.ensure_session(self.focused_index())?;
                                }
                            }
                            Confirm::CloseSession => {
                                if self.focused_session().is_some() {
                                    self.close_session(None, index, self.focused_session_index())?;
                                }
                            }
                            Confirm::StopEnvironment => self.env(index, EnvironmentOp::Down)?,
                            Confirm::CloseWorktree { .. } | Confirm::RemoveWorktree { .. } => {
                                // The worktree pane names the checkout it deletes, which need
                                // not be the focused one, or have a tab at all.
                                let (target, open) = match confirm {
                                    Confirm::RemoveWorktree { tree: Some(tree) } => (
                                        tree.clone(),
                                        self.project
                                            .workspaces
                                            .iter()
                                            .position(|workspace| workspace.tree.id == tree.id),
                                    ),
                                    _ => (self.focused().tree.clone(), Some(index)),
                                };
                                let tree = target.id.clone();
                                let result = (|| -> Result<()> {
                                    let repository = self.git_repository()?;
                                    anyhow::ensure!(
                                        !target.is_main,
                                        "This is the main checkout, not a linked worktree. Choose Yes to close the tab without deleting the repository."
                                    );
                                    anyhow::ensure!(
                                        !open.is_some_and(
                                            |open| self.project.workspaces[open].pending
                                        ),
                                        "An environment operation is still running. Wait for it to finish, then try again."
                                    );
                                    self.jobs
                                        .try_send(Job::Remove {
                                            repository: repository.clone(),
                                            tree: target,
                                        })
                                        .context("command queue is busy")
                                })();
                                if result.is_ok()
                                    && let Some(open) = open
                                {
                                    // Docker must not start on this checkout while Git removes it.
                                    self.project.workspaces[open].pending = true;
                                }
                                self.notice = None;
                                self.modal = Some(Modal::Removal {
                                    tree,
                                    confirm: confirm.clone(),
                                    error: result.err().map(|error| format!("{error:#}")),
                                });
                            }
                        }
                        retain = false;
                    } else if choice == Some(2) {
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
                    if let Some(modal) = self.modal.take() {
                        self.note_hidden_progress(&modal);
                    }
                    return Ok(true);
                }
                Event::Input(crossterm::event::Event::Resize(_, _)) => {}
                Event::Input(_) => return Ok(false),
                _ => {}
            }
        }
        match event {
            Event::Input(crossterm::event::Event::Key(key)) => {
                // A key ends any drag. With a selection, a scrolled-back view and the notice it
                // clears, that is all crit itself shows of a key that only goes to a pane.
                let scroll_drag = self.scroll_drag.take().is_some();
                let split_drag = self.finish_split_drag();
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
                            // The text belongs to what has focus now, not to whatever has
                            // focus when the read finishes.
                            if self.native_keys || self.modal.is_none() {
                                if let Some(session) = self.focused_session() {
                                    self.paste_from_clipboard(Some(session.id))?;
                                }
                            } else {
                                self.paste_from_clipboard(None)?;
                            }
                            return Ok(false);
                        }
                        _ => {}
                    }
                }
                let selected = self.selection.take().is_some();
                let action = keys::action(key);
                if action == Some(Action::ToggleNative) {
                    if key.kind == KeyEventKind::Press {
                        self.action(Action::ToggleNative)?;
                    }
                } else if self.native_keys {
                    let scrolled = match self.focused_session() {
                        Some(session) => session.key(key)?,
                        None => false,
                    };
                    let cleared = self.clear_notice_on_input(key.code == KeyCode::Esc);
                    return Ok(scroll_drag || split_drag || selected || scrolled || cleared);
                } else if self.modal.is_some() {
                    self.modal_key(key)?;
                } else if let Some(action) = action {
                    self.notice = None;
                    self.action(action)?;
                } else {
                    // Every other key is the pane's, Ctrl+D too: an agent quits on it and a shell
                    // reads end of input, and the pane then closes as a Kitty window would (see
                    // `session_exited`).
                    //
                    // The program draws its answer, if any, through its own output; drawing now
                    // as well would put out a frame before the echo, and the echo would then wait
                    // out the frame interval.
                    let scrolled = match self.focused_session() {
                        Some(session) => session.key(key)?,
                        None => false,
                    };
                    let cleared = self.clear_notice_on_input(key.code == KeyCode::Esc);
                    return Ok(scroll_drag || split_drag || selected || scrolled || cleared);
                }
            }
            Event::Input(crossterm::event::Event::Paste(text)) => {
                let selected = self.selection.take().is_some();
                let split_drag = self.finish_split_drag();
                // Routed like keys: the terminal gets it in native mode or with no dialog,
                // even behind a progress dialog. Like a key, it draws through the program.
                if self.native_keys || self.modal.is_none() {
                    if let Some(session) = self.focused_session() {
                        let scrolled = session.paste(&text)?;
                        let cleared = self.clear_notice_on_input(false);
                        return Ok(selected || split_drag || scrolled || cleared);
                    }
                } else {
                    match &mut self.modal {
                        Some(Modal::Intent(dialog)) if !dialog.pending && !dialog.submitted => {
                            paste_input(&mut dialog.input, &text, 4096);
                        }
                        Some(Modal::NewWorktree { input, .. }) => paste_input(input, &text, 256),
                        Some(Modal::OpenProject(picker)) => picker.paste(&text),
                        _ => {}
                    }
                }
            }
            Event::Input(crossterm::event::Event::Mouse(mouse)) if self.modal.is_none() => {
                if let Some(grab) = self.split_drag {
                    if matches!(
                        mouse.kind,
                        MouseEventKind::Drag(MouseButton::Left)
                            | MouseEventKind::Up(MouseButton::Left)
                    ) {
                        if let Some(divider) = self.split_divider {
                            let ratio = divider.ratio_at(mouse.column, grab);
                            if SplitDivider::left_width(divider.available_width, ratio)
                                != SplitDivider::left_width(
                                    divider.available_width,
                                    self.project.split_ratio,
                                )
                            {
                                self.project.split_ratio = ratio;
                            }
                        }
                        if matches!(mouse.kind, MouseEventKind::Up(_)) {
                            self.finish_split_drag();
                        }
                        return Ok(true);
                    }
                    if matches!(mouse.kind, MouseEventKind::Down(_)) {
                        self.finish_split_drag();
                    }
                }
                if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && let Some(divider) = self.split_divider
                    && Rect::new(
                        divider.handle.x,
                        divider.track.y,
                        divider.handle.width,
                        divider.track.height,
                    )
                    .contains((mouse.column, mouse.row).into())
                    && (mouse.column != divider.handle.x
                        || mouse.row == divider.handle.y
                        || self
                            .panes
                            .first()
                            .is_some_and(|pane| pane.scrollbar.is_none()))
                {
                    self.selection = None;
                    self.scroll_drag = None;
                    self.split_drag = Some(mouse.column - divider.handle.x);
                    return Ok(true);
                }
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
                if let Some((id, grab)) = self.scroll_drag
                    && matches!(
                        mouse.kind,
                        MouseEventKind::Drag(MouseButton::Left)
                            | MouseEventKind::Up(MouseButton::Left)
                    )
                {
                    if let Some((session, bar)) = self.panes.iter().find_map(|pane| {
                        let workspace = self.project.workspaces.get(pane.workspace)?;
                        let session = workspace.sessions.get(pane.session)?;
                        (session.id == id).then_some((session, pane.scrollbar?))
                    }) {
                        session.scroll_to(bar.offset_at(mouse.row, grab));
                    }
                    if matches!(mouse.kind, MouseEventKind::Up(_)) {
                        self.scroll_drag = None;
                    }
                    return Ok(true);
                }
                if matches!(mouse.kind, MouseEventKind::Down(_)) {
                    self.clear_notice_on_input(false);
                }
                if matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)) {
                    // A press that reached a program owns its drag and release wherever
                    // the pointer goes. A press Crit consumed leaves nothing to forward.
                    let Some((id, body)) = self.mouse_capture else {
                        return Ok(false);
                    };
                    if matches!(mouse.kind, MouseEventKind::Up(_)) {
                        self.mouse_capture = None;
                    }
                    self.forward_capture(mouse, id, body)?;
                    return Ok(true);
                }
                if let Some(area) = self.worktree_area
                    && area.contains((mouse.column, mouse.row).into())
                {
                    return self.worktree_mouse(mouse);
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
                    let (index, selected, secondary, body, scrollbar) = (
                        pane.workspace,
                        pane.session,
                        pane.secondary,
                        pane.body,
                        pane.scrollbar,
                    );
                    if matches!(mouse.kind, crossterm::event::MouseEventKind::Down(_))
                        && self.project.focus_secondary != secondary
                    {
                        self.project.focus_secondary = secondary;
                        self.state_dirty = true;
                    }
                    let workspace = &self.project.workspaces[index];
                    if let Some(session) = workspace.sessions.get(selected) {
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
                            self.paste_from_clipboard(Some(session.id))?;
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
                                self.mouse_capture = Some((session.id, body));
                            }
                        } else {
                            session.mouse(mouse, mouse.column - body.x, mouse.row - body.y)?;
                            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                                self.mouse_capture = Some((session.id, body));
                            }
                        }
                    }
                }
                if mouse.kind == MouseEventKind::Moved {
                    return Ok(false);
                }
            }
            Event::Input(crossterm::event::Event::Resize(cols, rows)) => {
                self.interaction_visible = cols >= 30 && rows >= 10;
                self.panes.clear();
                self.scroll_drag = None;
                self.finish_split_drag();
                self.split_divider = None;
                self.selection = None;
                self.mouse_capture = None;
                self.tab_hits.clear();
                self.worktree_hits.clear();
            }
            // Pointer motion over a dialog changes nothing worth a frame.
            Event::Input(crossterm::event::Event::Mouse(mouse))
                if mouse.kind == MouseEventKind::Moved =>
            {
                return Ok(false);
            }
            // A dialog hides the pane, but a release still ends the drag its program began.
            Event::Input(crossterm::event::Event::Mouse(mouse))
                if matches!(mouse.kind, MouseEventKind::Up(_)) =>
            {
                if let Some((id, body)) = self.mouse_capture.take() {
                    self.forward_capture(mouse, id, body)?;
                }
            }
            Event::Input(_) => {}
            Event::Metadata(listings) => return Ok(self.apply_metadata(listings)),
            Event::Dirty(id) => {
                return Ok(self.panes.iter().any(|p| {
                    self.project.workspaces[p.workspace]
                        .sessions
                        .get(p.session)
                        .is_some_and(|s| s.id == id)
                }));
            }
            Event::Exited {
                session,
                code,
                success,
                ran,
            } => self.session_exited(session, code, success, ran)?,
            Event::Clipboard { ty, data } => {
                clipboard::copy_encoded(&ty, &data).context("forwarding a pane's OSC 52 copy")?;
                return Ok(false);
            }
            Event::ClipboardText { target, result } => match result {
                Err(error) => self.set_notice(error),
                Ok(text) => {
                    // The read took a while: paste only where it was asked for.
                    let terminal_has_input = self.native_keys || self.modal.is_none();
                    let still_there = match target {
                        Some(id) => {
                            terminal_has_input
                                && self
                                    .focused_session()
                                    .is_some_and(|session| session.id == id)
                        }
                        None => !terminal_has_input,
                    };
                    if still_there {
                        return self.handle(Event::Input(crossterm::event::Event::Paste(text)));
                    }
                    self.set_notice(
                        "Clipboard paste dropped: focus changed before the clipboard answered.",
                    );
                }
            },
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
            Event::Projects { request, projects } => {
                if let Some(Modal::OpenProject(picker)) = &mut self.modal
                    && picker.request == request
                {
                    picker.set_projects(projects);
                }
            }
            Event::ProjectOpened {
                request,
                result,
                agent,
                fresh_tab,
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
                    Ok(project) => self.open_project(project, agent, fresh_tab)?,
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
                        self.note_tree(&project, &tree);
                        let has_compose = compose::has_config(&tree);
                        if self.project.common_dir() != Some(project.as_path()) {
                            let mut not_started = false;
                            if let Some(saved) = self
                                .saved_projects
                                .iter_mut()
                                .find(|saved| saved.common_dir() == Some(project.as_path()))
                            {
                                let mut workspace = Workspace::new(tree);
                                workspace.source =
                                    preparation_error.as_ref().map(|_| source.clone());
                                if has_compose && preparation_error.is_none() {
                                    // The tab exists whether or not the queue took the start.
                                    workspace.pending = self
                                        .jobs
                                        .try_send(Job::Environment {
                                            tree: workspace.tree.clone(),
                                            operation: EnvironmentOp::Up,
                                            source: workspace.source.clone(),
                                        })
                                        .is_ok();
                                    not_started = !workspace.pending;
                                }
                                saved.workspaces.push(workspace);
                            }
                            if let Some(error) = preparation_error {
                                self.set_notice(format!("Worktree created; local environment preparation failed: {error}"));
                            } else if not_started {
                                self.set_notice(ENVIRONMENT_NOT_STARTED);
                            }
                            return Ok(true);
                        }
                        let mut not_started = false;
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
                                not_started = self.env(index, EnvironmentOp::Up).is_err();
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
                        } else if not_started {
                            self.set_notice(ENVIRONMENT_NOT_STARTED);
                        } else {
                            self.set_notice(if has_compose {
                                "Worktree created; starting its isolated Docker environment."
                            } else {
                                "Worktree created. No Compose config found; terminal is ready."
                            });
                        }
                    }
                    Err(error) => {
                        // A restored dialog could not be answered while the terminal owns the keys.
                        if !self.native_keys
                            && self.modal.is_none()
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
                        // A background probe never touches the notice.
                        if active && let Some(summary) = summary {
                            self.set_notice(summary);
                        }
                    }
                    Err(error) if !matches!(operation, EnvironmentOp::Probe) => {
                        self.set_notice(error)
                    }
                    Err(_) => {}
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
            Event::SplitTargets {
                request,
                project,
                layout,
                result,
            } => {
                if self.project.common_dir() == Some(project.as_path())
                    && matches!(&self.modal, Some(Modal::Loading { request: pending, .. }) if *pending == request)
                {
                    match result {
                        Ok(choices) => {
                            self.modal = Some(Modal::ExistingSplit {
                                layout,
                                choices,
                                selected: 0,
                                error: None,
                            })
                        }
                        Err(error) => {
                            self.modal = Some(Modal::Split {
                                layout,
                                selected: 2,
                            });
                            self.set_notice(error);
                        }
                    }
                }
            }
            Event::SplitOpened {
                request,
                project,
                layout,
                choices,
                selected,
                source,
                preparation_error,
                result,
            } => {
                // Esc hides the progress dialog, so the result can arrive without it.
                let waiting = matches!(&self.modal, Some(Modal::OpeningSplit { request: pending }) if *pending == request);
                if let Ok((tree, _)) = &result {
                    self.note_tree(&project, tree);
                }
                if self.project.common_dir() != Some(project.as_path()) {
                    // The project was left while the checkout opened: only report it.
                    if waiting {
                        self.modal = None;
                    }
                    match result {
                        Ok((tree, _)) => self.set_notice(format!(
                            "{} is ready in the project you left; open it there from Open existing worktree.",
                            tree.label()
                        )),
                        Err(error) => self.set_notice(error),
                    }
                    return Ok(true);
                }
                match result {
                    Ok((tree, created)) if tree.id != self.focused().tree.id => {
                        let index = if let Some(index) = self
                            .project
                            .workspaces
                            .iter()
                            .position(|workspace| workspace.tree.id == tree.id)
                        {
                            index
                        } else {
                            let mut workspace = Workspace::new(tree);
                            workspace.source = preparation_error.as_ref().map(|_| source);
                            self.project.workspaces.push(workspace);
                            self.project.workspaces.len() - 1
                        };
                        if waiting {
                            self.modal = None;
                        }
                        let mut not_started = false;
                        if created
                            && preparation_error.is_none()
                            && compose::has_config(&self.project.workspaces[index].tree)
                            && !self.project.workspaces[index].pending
                        {
                            not_started = self.env(index, EnvironmentOp::Up).is_err();
                        }
                        if waiting {
                            self.split_with(layout, index, false)?;
                        } else {
                            // The layout may have changed since Esc, so only add the tab.
                            self.workspace_changed();
                        }
                        if let Some(error) = preparation_error {
                            self.set_notice(format!("Worktree created; local environment preparation failed: {error}. Fix the files, then start its environment again."));
                        } else if not_started {
                            self.set_notice(ENVIRONMENT_NOT_STARTED);
                        } else if !waiting {
                            let label = self.project.workspaces[index].label().to_owned();
                            self.set_notice(format!("{label} is ready as a tab."));
                        }
                    }
                    result => {
                        // The picker comes back only where it can be used: not after Esc hid the
                        // progress dialog, and not in native mode, where the terminal owns the keys.
                        let restore = waiting && !self.native_keys;
                        let error = match result {
                            Err(error) => error,
                            Ok(_) if restore => "This checkout is already in the focused pane. Choose another, or go back and use Split terminal.".to_owned(),
                            Ok(_) => "This checkout is already in the focused pane.".to_owned(),
                        };
                        if restore {
                            self.modal = Some(Modal::ExistingSplit {
                                layout,
                                choices,
                                selected,
                                error: Some(error),
                            });
                        } else {
                            if waiting {
                                self.modal = None;
                            }
                            self.set_notice(error);
                        }
                    }
                }
            }
            Event::Removed { tree, result } => match result {
                Ok(warning) => {
                    self.workspace_changed();
                    // The progress dialog is this removal's, whichever tab (if any) held the
                    // checkout, and the pane stops listing it now instead of at the next poll.
                    if matches!(&self.modal, Some(Modal::Removal { tree: pending, .. }) if *pending == tree)
                    {
                        self.modal = None;
                    }
                    for project in
                        std::iter::once(&mut self.project).chain(&mut self.saved_projects)
                    {
                        project.trees.retain(|listed| listed.id != tree);
                    }
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
                        self.close_tab(None, index);
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
                        self.close_tab(Some(project), index);
                    }
                    self.set_notice(match warning {
                        Some(warning) => format!(
                            "Checkout removed; Git branch retained. Docker could not be checked ({warning}); if this worktree's stack still exists, run: docker compose -p crit-{tree} down"
                        ),
                        None => {
                            "Checkout removed; Git branch and Docker volumes retained.".to_owned()
                        }
                    });
                }
                Err(error) => {
                    if let Some(workspace) = self.all_workspaces_mut().find(|w| w.tree.id == tree) {
                        workspace.pending = false;
                    }
                    if let Some(Modal::Removal {
                        tree: pending,
                        error: failure,
                        ..
                    }) = &mut self.modal
                        && *pending == tree
                    {
                        *failure = Some(error);
                    } else {
                        self.set_notice(error);
                    }
                }
            },
            Event::CpuLimit(message) => self.append_notice(&message),
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

/// The project's display name: its main checkout's folder. A bare repository has no checkout,
/// so its entry is the repository itself (`myapp/.bare`, `app.git`), which is named after the
/// folder that holds it instead.
fn project_name(main_tree: &Path, common_dir: &Path) -> String {
    let name = folder_name(main_tree);
    if main_tree != common_dir {
        return name;
    }
    if name.starts_with('.') {
        return main_tree
            .parent()
            .map(folder_name)
            .filter(|parent| !parent.is_empty())
            .unwrap_or(name);
    }
    match name.strip_suffix(".git") {
        Some(stem) if !stem.is_empty() => stem.to_owned(),
        _ => name,
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
    for c in text.chars() {
        // Pasted line breaks and tabs become one space, so lines do not run together.
        let c = if !c.is_control() {
            c
        } else if c.is_whitespace() && !input.is_empty() && !input.ends_with(' ') {
            ' '
        } else {
            continue;
        };
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

/// Serializes an intent destination's path as UTF-8 text; invalid bytes become U+FFFD.
fn lossy_path<S: serde::Serializer>(
    path: &Path,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&path.to_string_lossy())
}

fn intent_targets(
    base: &Path,
    repository: Option<&worktree::Repository>,
    known_projects: &[PathBuf],
) -> Result<Vec<IntentTarget>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let discovered = worktree::projects(base, home.as_deref());
    let mut trees = repository
        .map(worktree::list)
        .transpose()?
        .unwrap_or_default();
    // A bare repository, or a checkout deleted outside Crit, is not a place to open.
    trees.retain(Worktree::is_checkout);
    Ok(ranked_intent_targets(trees, known_projects, discovered))
}

/// Request bytes one destination costs beyond its name and path: keys, quotes, id, branch.
const INTENT_TARGET_OVERHEAD: usize = 60;
/// What the destinations may take of the interpreter's 64 KiB request cap; the context and
/// conversation need the rest.
const INTENT_TARGET_BUDGET: usize = 48 * 1024;

/// Destinations for Ask Crit, most relevant first: the active repository's worktrees, the
/// open projects, then the discovered ones. Discovered projects stop at the byte budget, so
/// a huge scan cannot make every request exceed the cap.
fn ranked_intent_targets(
    trees: Vec<Worktree>,
    known: &[PathBuf],
    mut discovered: Vec<PathBuf>,
) -> Vec<IntentTarget> {
    let mut targets = Vec::with_capacity(trees.len() + known.len() + discovered.len());
    let mut bytes = 0;
    for tree in trees {
        bytes += tree.path.as_os_str().len() + 2 * tree.branch.len() + INTENT_TARGET_OVERHEAD;
        targets.push(IntentTarget {
            id: targets.len(),
            kind: "worktree",
            name: tree.branch.clone(),
            path: tree.path,
            branch: Some(tree.branch),
        });
    }
    let mut open = known.to_vec();
    open.sort();
    open.dedup();
    discovered.retain(|path| open.binary_search(path).is_err());
    let projects = open
        .into_iter()
        .map(|path| (path, false))
        .chain(discovered.into_iter().map(|path| (path, true)));
    for (path, trimmed) in projects {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let cost = path.as_os_str().len() + name.len() + INTENT_TARGET_OVERHEAD;
        if trimmed && bytes + cost > INTENT_TARGET_BUDGET {
            break;
        }
        bytes += cost;
        targets.push(IntentTarget {
            id: targets.len(),
            kind: "project",
            name,
            path,
            branch: None,
        });
    }
    targets
}

fn prepare_local_environment(source: &Path, target: &Worktree) -> Result<()> {
    let repository = worktree::discover(source)?;
    let source = crate::selected_tree(&repository, source)?;
    compose::prepare_local_files(&source, target)
}

/// Runs one Docker operation and reports it as the `Environment` event.
fn environment_event(tree: Worktree, operation: EnvironmentOp, source: Option<PathBuf>) -> Event {
    let mut prepared = false;
    let result = (|| {
        if matches!(operation, EnvironmentOp::Up | EnvironmentOp::Restart)
            && let Some(source) = source
        {
            // A source that no longer exists can never be retried; use the files in place.
            if source.try_exists().unwrap_or(true) {
                prepare_local_environment(&source, &tree)?;
            }
            prepared = true;
        }
        match operation {
            EnvironmentOp::Up => compose::up(&tree),
            EnvironmentOp::Down => compose::down(&tree),
            EnvironmentOp::Restart => compose::restart(&tree),
            EnvironmentOp::Status | EnvironmentOp::Probe => compose::status(&tree),
        }
    })();
    Event::Environment {
        tree: tree.id,
        operation,
        prepared,
        result: result.map_err(|e| format!("{e:#}")),
    }
}

fn start_worker(tx: SyncSender<Event>) -> Result<SyncSender<Job>> {
    let (jobs, rx) = mpsc::sync_channel(16);
    std::thread::Builder::new()
        .name("commands".into())
        .spawn(move || {
            // Polls list a repository again only after something Git would list changed.
            let mut listing_cache = worktree::ListingCache::default();
            while let Ok(job) = rx.recv() {
                let event = match job {
                    Job::Projects { request, base } => {
                        let home = std::env::var_os("HOME").map(PathBuf::from);
                        let projects = worktree::projects(&base, home.as_deref());
                        Event::Projects { request, projects }
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
                        fresh_tab,
                    } => Event::ProjectOpened {
                        request,
                        result: Project::open(&path).map_err(|e| format!("{e:#}")),
                        agent,
                        fresh_tab,
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
                        result: worktree::list(&repository)
                            .map(|mut trees| {
                                trees.retain(Worktree::is_checkout);
                                trees
                            })
                            .map_err(|e| format!("{e:#}")),
                        project: repository.common_dir,
                    },
                    Job::SplitTargets { request, repository, focused, layout } => Event::SplitTargets {
                        request,
                        layout,
                        result: worktree::split_targets(&repository, &focused).map_err(|error| format!("{error:#}")),
                        project: repository.common_dir,
                    },
                    Job::OpenSplit { request, repository, layout, choices, selected } => {
                        let result = choices.get(selected)
                            .context("Selected split target is no longer available")
                            .and_then(|target| worktree::open_split_target(&repository, target))
                            .map_err(|error| format!("{error:#}"));
                        let preparation_error = result.as_ref().ok()
                            .filter(|(_, created)| *created)
                            .and_then(|(tree, _)| prepare_local_environment(&repository.root, tree).err())
                            .map(|error| format!("{error:#}"));
                        Event::SplitOpened {
                            request, layout, choices, selected,
                            project: repository.common_dir,
                            source: repository.root,
                            preparation_error,
                            result,
                        }
                    }
                    Job::RefreshMetadata(repositories) => {
                        let mut listings = Vec::new();
                        for repository in repositories {
                            // Checkout can temporarily make Git metadata unreadable.
                            // Keep the previous labels and retry without notice spam.
                            // An unchanged repository is left out: its labels stand.
                            if let Ok(Some(found)) = listing_cache.refresh(&repository) {
                                listings.push((repository.common_dir, found));
                            }
                        }
                        // Sent even when empty: it ends the poll (`metadata_pending`).
                        Event::Metadata(listings)
                    }
                    Job::Remove { repository, tree } => {
                        let result = (|| -> Result<Option<String>> {
                            // Only an environment Crit started can hold containers, so a tree
                            // without one is removable even where Docker cannot run. Docker
                            // that cannot answer is no reason to refuse either: the removal
                            // goes ahead and the notice names the stack to clean up.
                            let warning = match compose::existing_status(&tree) {
                                Ok(Some(environment)) if !environment.services.is_empty() => {
                                    anyhow::bail!(
                                        "Stop this worktree's Docker environment first: Esc, then F1 → F2 → Stop environment."
                                    )
                                }
                                Ok(_) => None,
                                Err(error) => Some(format!("{error:#}").trim_end().to_owned()),
                            };
                            worktree::remove(&repository, &tree)?;
                            Ok(warning)
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
                        // Docker can run for minutes; other jobs must not queue behind it.
                        // `Workspace::pending` keeps one operation per worktree in flight.
                        let id = tree.id.clone();
                        let environment_tx = tx.clone();
                        match std::thread::Builder::new()
                            .name("environment".into())
                            .spawn(move || {
                                let _ =
                                    environment_tx.send(environment_event(tree, operation, source));
                            }) {
                            Ok(_) => continue,
                            Err(error) => Event::Environment {
                                tree: id,
                                operation,
                                prepared: false,
                                result: Err(format!(
                                    "could not start the environment thread: {error}"
                                )),
                            },
                        }
                    }
                    // A retune that worked has nothing to say: F3's notice already did.
                    Job::RetuneAgentCpu(limiter) => match limiter.retune() {
                        Ok(()) => continue,
                        Err(error) => Event::CpuLimit(error),
                    },
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
        })?;
    Ok(jobs)
}

/// Set while a `TerminalGuard` holds the terminal, so the panic hook and the guard restore it
/// exactly once: a second `?1049l` would move the cursor back over the panic message, and a
/// second `PopKeyboardEnhancementFlags` would pop the shell's own keyboard mode.
static TERMINAL_OWNED: AtomicBool = AtomicBool::new(false);

struct TerminalGuard {
    use_kitty: bool,
}
impl TerminalGuard {
    fn enter(use_kitty: bool) -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        TERMINAL_OWNED.store(true, Ordering::Release);
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

    /// Hands the terminal back. Only the first call does anything.
    fn restore(use_kitty: bool) {
        if !TERMINAL_OWNED.swap(false, Ordering::AcqRel) {
            return;
        }
        if use_kitty {
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
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore(self.use_kitty);
    }
}

/// Restores the terminal before the default hook prints a UI-thread panic. Otherwise the
/// message lands on the alternate screen, which leaving it discards. Panics on other threads
/// keep the previous behavior, so the UI stays usable after them.
fn install_panic_hook(use_kitty: bool) {
    let ui = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui {
            TerminalGuard::restore(use_kitty);
        }
        previous(info);
    }));
}

/// Where ratatui writes the outer terminal's frames: one buffer, which `draw_frame` sends to
/// stdout in a single `write(2)`. Written to stdout itself, a frame would go out in up to 30
/// pieces, each a wake-up and a partial parse for the outer terminal: stdout's `LineWriter`
/// passes bytes on every 1 KiB, and the backend flushes after each cursor command. So `flush`
/// keeps the bytes, and only `draw_frame` sends them. It leaves the buffer empty, so that what
/// `TerminalGuard` and `kitty` write to stdout directly between frames keeps its order. Clones
/// share the buffer, as ratatui keeps its backend's writer to itself.
#[derive(Clone, Default)]
struct FrameWriter(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl FrameWriter {
    /// Sends what was written to stdout at once and empties the buffer, keeping its capacity.
    /// It is emptied even when the write fails, so that no stale bytes lead the next frame.
    fn send(&self) -> io::Result<()> {
        let mut frame = self.0.borrow_mut();
        let mut stdout = io::stdout().lock();
        let sent = stdout.write_all(&frame).and_then(|()| stdout.flush());
        frame.clear();
        sent
    }
}

impl Write for FrameWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The outer terminal's size, read with one `ioctl` on stdout, which `run` has checked is a
/// terminal. crossterm's `terminal::size` opens and closes `/dev/tty` around the same `ioctl`,
/// and a full-screen ratatui viewport calls it on every frame, so `run` gives its terminal a
/// fixed viewport, which `draw_frame` fits to this size.
fn window_size() -> io::Result<ratatui::layout::Size> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(ratatui::layout::Size::new(size.ws_col, size.ws_row))
}

/// Draws one frame on the real terminal as a single synchronized update (DEC mode 2026), so
/// that Kitty never paints it half written. Terminals without the mode ignore it. `output` is
/// the writer of `terminal`'s backend; the whole frame leaves in one write. `size` is the
/// terminal's, from `window_size`: when it has changed, the viewport is resized and cleared, as
/// a full-screen one resizes itself, so that the frame is drawn whole and nothing of the last
/// one stays.
fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<FrameWriter>>,
    output: &FrameWriter,
    size: ratatui::layout::Size,
    render: impl FnOnce(&mut ratatui::Frame),
) -> Result<()> {
    queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let area = Rect::from(size);
    let fitted = if terminal.get_frame().area() == area {
        Ok(())
    } else {
        terminal.resize(area)
    };
    let drawn = fitted.and_then(|()| terminal.draw(render).map(drop));
    // Ended even when drawing failed, so the terminal is not left waiting out its timeout.
    queue!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    output.send()?;
    drawn?;
    Ok(())
}

fn choose_project(
    terminal: &mut Terminal<CrosstermBackend<FrameWriter>>,
    output: &FrameWriter,
    rx: &mpsc::Receiver<Event>,
    jobs: &SyncSender<Job>,
    path: &Path,
    notice: Option<&str>,
    use_kitty: bool,
) -> Result<Option<(Project, bool, bool)>> {
    let launch_path = std::env::current_dir().context("the current folder no longer exists")?;
    let base = if path.is_dir() {
        path.to_path_buf()
    } else {
        launch_path.clone()
    };
    let mut notice = notice
        .unwrap_or("Choose New terminal for a shell here, or open a project or any folder.")
        .to_owned();
    let mut picker = ProjectPicker::new(base, jobs, 0)?;
    picker.terminal_path = Some(launch_path.clone());
    let mut native_keys = false;
    let mut interaction_visible = true;
    // Drawn again only after an event it handles: mouse capture reports every pointer move,
    // and the chooser shows nothing of the pointer.
    let mut redraw = true;
    loop {
        if redraw {
            draw_frame(terminal, output, window_size()?, |frame| {
                let bounds = frame.area();
                interaction_visible = bounds.width >= 30 && bounds.height >= 10;
                ui::draw_project_chooser(frame, &picker, &notice, native_keys);
            })?;
        }
        redraw = true;
        match rx.recv()? {
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
            Event::Input(crossterm::event::Event::Resize(..)) => {}
            Event::Projects { request, projects } if picker.request == request => {
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
            _ => redraw = false,
        }
    }
}

/// Reopens the saved projects without ever failing as a whole: a folder, worktree, chat or
/// agent that cannot be reopened is left out and described in the returned lines, and a split
/// that loses a pane falls back to full width on the survivor. The first element is `None`
/// when no project could be reopened.
fn restore_projects(
    snapshot: &restore::Snapshot,
    socket: &Path,
    limiter: &limits::Limiter,
    tx: &SyncSender<Event>,
    size: ratatui::layout::Size,
    next_session: &mut u64,
) -> (Option<(Project, Vec<Project>)>, Vec<String>) {
    // One `opencode db` call covers every saved OpenCode chat and the newest chat of each
    // folder whose OpenCode pane remembers none; OMP chats are read per file.
    let mut opencode_ids = Vec::new();
    let mut opencode_folders = Vec::new();
    for workspace in snapshot
        .projects
        .iter()
        .flat_map(|project| &project.workspaces)
    {
        for session in &workspace.sessions {
            match (session.kind.as_str(), session.resume.as_deref()) {
                ("opencode", Some(identity)) => opencode_ids.push(identity),
                ("opencode", None) => opencode_folders.extend(workspace.path.to_str()),
                _ => {}
            }
        }
    }
    let opencode_chats = integration::opencode_chats(&opencode_ids, &opencode_folders)
        .map_err(|error| format!("{error:#}"));
    // Every pane reopens as the next session, sized for the window, under the agent CPU limit.
    let mut spawn = |kind: &str, resume: Option<integration::Resume<'_>>, cwd: &Path| {
        let id = *next_session;
        *next_session += 1;
        Session::spawn(
            id,
            kind,
            resume,
            cwd,
            socket,
            tx.clone(),
            size.height.saturating_sub(4),
            size.width.saturating_sub(2),
            None,
            Some(limiter),
        )
    };
    let mut skipped = Vec::new();
    let mut projects = Vec::with_capacity(snapshot.projects.len());
    let mut current = None;
    for (index, saved) in snapshot.projects.iter().enumerate() {
        let Some(mut project) = restore_project(saved, &opencode_chats, &mut spawn, &mut skipped)
        else {
            continue;
        };
        project.order = projects.len() as u64;
        if index == snapshot.active_project {
            current = Some(projects.len());
        }
        projects.push(project);
    }
    if projects.is_empty() {
        return (None, skipped);
    }
    // If the current project itself was left out, the first one that reopened takes its place.
    let current = projects.remove(current.unwrap_or(0));
    (Some((current, projects)), skipped)
}

/// One saved project, or `None` when none of its folders reopened. Its panes are started with
/// `spawn`. Every item that cannot be reopened adds a line to `skipped`.
fn restore_project(
    saved: &restore::SavedProject,
    opencode_chats: &Result<integration::OpencodeChats, String>,
    spawn: &mut impl FnMut(&str, Option<integration::Resume<'_>>, &Path) -> Result<Session>,
    skipped: &mut Vec<String>,
) -> Option<Project> {
    let mut project: Option<Project> = None;
    // Where each saved workspace, and each saved session of the active one, ended up.
    let mut workspaces: Vec<Option<usize>> = Vec::with_capacity(saved.workspaces.len());
    let mut active_sessions: Vec<Option<usize>> = Vec::new();
    for (index, saved_workspace) in saved.workspaces.iter().enumerate() {
        let path = &saved_workspace.path;
        let mut opened = match Project::open(path) {
            Ok(opened) => opened,
            Err(error) => {
                skipped.push(format!("{}: {error:#}", path.display()));
                workspaces.push(None);
                continue;
            }
        };
        if let Some(project) = &project
            && !project.same_project(&opened)
        {
            skipped.push(format!(
                "{}: no longer part of {}",
                path.display(),
                project.name
            ));
            workspaces.push(None);
            continue;
        }
        let tree = opened.workspaces.remove(0).tree;
        let project = project.get_or_insert(opened);
        let mut workspace = Workspace::new(tree);
        workspace.source = saved_workspace.source.clone();
        let mut sessions = Vec::with_capacity(saved_workspace.sessions.len());
        for saved_session in &saved_workspace.sessions {
            // An agent that remembers no chat picks up its folder's newest one.
            let resume = match (saved_session.kind.as_str(), saved_session.resume.as_deref()) {
                (_, Some(identity)) => Some(integration::Resume::Chat(identity)),
                ("omp", None) => Some(integration::Resume::Continue),
                ("opencode", None) => opencode_chats
                    .as_ref()
                    .ok()
                    .and_then(|chats| chats.latest.get(path.to_str()?))
                    .map(|chat| integration::Resume::Chat(chat.as_str())),
                _ => None,
            };
            let spawned = check_resume(saved_session, opencode_chats)
                .and_then(|()| spawn(&saved_session.kind, resume, &workspace.tree.path));
            match spawned {
                Ok(session) => {
                    sessions.push(Some(workspace.sessions.len()));
                    workspace.sessions.push(session);
                }
                Err(error) => {
                    skipped.push(format!(
                        "{} session in {}: {error:#}",
                        saved_session.kind,
                        path.display()
                    ));
                    sessions.push(None);
                }
            }
        }
        workspace.selected = sessions
            .get(saved_workspace.selected)
            .copied()
            .flatten()
            .unwrap_or(0);
        workspaces.push(Some(project.workspaces.len()));
        project.workspaces.push(workspace);
        if index == saved.active {
            active_sessions = sessions;
        }
    }
    let mut project = project?;
    let active = workspaces.get(saved.active).copied().flatten();
    let secondary = saved
        .secondary
        .map(|index| workspaces.get(index).copied().flatten());
    let secondary_session = saved
        .secondary_session
        .map(|index| active_sessions.get(index).copied().flatten());
    // The split survives only if both panes do; otherwise it is full width on what is left,
    // which keeps every invariant `Snapshot::validate` checks.
    if let Some(active) = active
        && let Some(Some(secondary)) = secondary
        && secondary_session.is_none_or(|session| {
            session.is_some_and(|session| session != project.workspaces[active].selected)
        })
    {
        project.active = active;
        project.secondary = Some(secondary);
        project.secondary_session = secondary_session.flatten();
        project.focus_secondary = saved.focus_secondary;
        project.layout = match saved.layout {
            restore::SavedLayout::Full => LayoutMode::Full,
            restore::SavedLayout::Vertical => LayoutMode::Vertical,
            restore::SavedLayout::Horizontal => LayoutMode::Horizontal,
        };
    } else {
        project.active = active.or(secondary.flatten()).unwrap_or(0);
    }
    project.split_ratio = saved.split_ratio;
    Some(project)
}

/// Why a saved chat cannot be resumed. OMP chats are read here; every OpenCode chat was looked
/// up at once in `opencode`.
fn check_resume(
    session: &restore::SavedSession,
    opencode: &Result<integration::OpencodeChats, String>,
) -> Result<()> {
    let Some(identity) = session.resume.as_deref() else {
        return Ok(());
    };
    integration::validate_resume(&session.kind, identity)?;
    if session.kind == "opencode" {
        match opencode {
            Ok(chats) => anyhow::ensure!(
                chats.existing.contains(identity),
                "saved OpenCode chat does not exist: {identity}"
            ),
            Err(error) => anyhow::bail!("{error}"),
        }
    }
    Ok(())
}

pub fn run(
    path: &Path,
    agent: &str,
    use_kitty: bool,
    direct_start: bool,
    fresh: bool,
    agent_cpu: Option<u8>,
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
    install_panic_hook(use_kitty);
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
    let output = FrameWriter::default();
    // Fixed, as `draw_frame` fits it to the size read for each frame.
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(output.clone()),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(window_size()?.into()),
        },
    )?;
    // Everything startup has to say, shown together. `sticky` keeps it past terminal input:
    // it reports that saving is off or that saved items were left out.
    let mut notices: Vec<String> = Vec::new();
    let mut sticky = false;
    let mut store = match restore::Store::open() {
        Ok(store) => Some(store),
        Err(error) => {
            notices.push(format!(
                "Workspace restoration/saving unavailable: {error:#}. This window will not replace the saved workspace."
            ));
            sticky = true;
            None
        }
    };
    // Only the window that owns the saved workspace updates the agents, at most once a day.
    let mut checks = updates::start(store.as_ref().map(restore::Store::directory));
    let snapshot = if fresh {
        None
    } else {
        match store.as_ref().map(restore::Store::load).transpose() {
            Ok(snapshot) => snapshot.flatten(),
            Err(error) => {
                notices.push(format!(
                    "Saved workspace could not be loaded: {error:#}. Original state retained; saving disabled. Use --fresh to replace it."
                ));
                sticky = true;
                store = None;
                None
            }
        }
    };
    // `--agent-cpu` turns the limit on at its share; otherwise the saved choice stands, even
    // once every tab was closed.
    let (cpu_limited, cpu_percent) = match (agent_cpu, &snapshot) {
        (Some(percent), _) => (true, percent),
        (None, Some(saved)) => (saved.agent_cpu_limited, saved.agent_cpu_percent),
        (None, None) => (true, limits::DEFAULT_PERCENT),
    };
    let snapshot = snapshot.filter(|snapshot| !snapshot.projects.is_empty());
    let server = integration::start(tx.clone())?;
    let limiter = limits::Limiter::new(server.directory(), cpu_percent, cpu_limited);
    notices.extend(limiter.startup_notice());
    let mut next_session = 1;
    let (restored, skipped) = match &snapshot {
        Some(snapshot) => restore_projects(
            snapshot,
            &server.socket_path,
            &limiter,
            &tx,
            terminal.size()?,
            &mut next_session,
        ),
        None => (None, Vec::new()),
    };
    if !skipped.is_empty() {
        // Keep the file as it was before the first checkpoint replaces it. A snapshot only
        // loads through a store, so both exist here.
        let kept = match (&store, &snapshot) {
            (Some(store), Some(snapshot)) => store.back_up(snapshot),
            _ => Err(anyhow::anyhow!("no saved workspace is open")),
        };
        let items = format!(
            "{} item{}",
            skipped.len(),
            if skipped.len() == 1 { "" } else { "s" }
        );
        let summary = match kept {
            Ok(backup) if restored.is_some() => format!(
                "Reopened the saved workspace without the {items} above. The previous workspace file is kept at {}.",
                backup.display()
            ),
            Ok(backup) => format!(
                "None of the saved projects could be reopened. The previous workspace file is kept at {}.",
                backup.display()
            ),
            Err(error) => {
                store = None;
                format!(
                    "The items above could not be reopened, and the previous workspace file could not be kept ({error:#}); saving is disabled for this window."
                )
            }
        };
        notices.push(format!("{}\n{summary}", skipped.join("\n")));
        sticky = true;
    }
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
                result => {
                    notices.extend(
                        result
                            .and_then(Result::err)
                            .map(|error| format!("{error:#}")),
                    );
                    let shown = notices.join("\n");
                    choose_project(
                        &mut terminal,
                        &output,
                        &rx,
                        &jobs,
                        path,
                        (!shown.is_empty()).then_some(shown.as_str()),
                        use_kitty,
                    )
                }
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
        split_divider: None,
        split_drag: None,
        project,
        saved_projects,
        native_keys,
        limiter: limiter.clone(),
        modal: None,
        notice: None,
        notice_sticky: false,
        panes: Vec::with_capacity(2),
        next_panes: Vec::with_capacity(2),
        scroll_drag: None,
        mouse_capture: None,
        selection: None,
        tab_hits: Vec::new(),
        worktree_pane: None,
        worktree_hits: Vec::new(),
        worktree_area: None,
        interaction_visible: true,
        should_quit: false,
        default_agent: agent.into(),
        next_session,
        next_request: 1,
        pending_creations: VecDeque::new(),
        state_dirty: true,
        last_saved: None,
        metadata_pending: false,
        metadata_checked: Instant::now() - App::METADATA_INTERVAL,
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
            notices.push(format!(
                "{error:#}. F1 → ask to start another agent or shell."
            ));
        }
    }
    if app.project.repository.is_some() && compose::has_config(&app.project.workspaces[active].tree)
    {
        app.env(active, EnvironmentOp::Probe)?;
    }
    // An explicit --repo/--agent also opens after a restore, as one more tab.
    if was_restored
        && direct_start
        && let Err(error) =
            Project::open(path).and_then(|project| app.open_project(project, None, false))
    {
        notices.push(format!("Could not open {}: {error:#}", path.display()));
    }
    if !notices.is_empty() {
        let notice = notices.join("\n");
        if sticky {
            app.set_warning(notice);
        } else {
            app.set_notice(notice);
        }
    }
    // Frames go out at least 16 ms apart, so that what is typed shows at once. While app output
    // keeps them coming, each within 100 ms of the one before, they go out at least 33 ms
    // apart: still smooth, for half the drawing, in crit and in the outer terminal. Typing keeps
    // the quick pace for 100 ms after each key: fast typing and key repeat draw frames that close
    // together too, and an echo must not wait out the slow pace.
    let quick_interval = Duration::from_millis(16);
    let streaming_interval = Duration::from_millis(33);
    let streaming_gap = Duration::from_millis(100);
    let frame_interval = |streaming: bool, last_typed: Instant| {
        if streaming && last_typed.elapsed() >= streaming_gap {
            streaming_interval
        } else {
            quick_interval
        }
    };
    // The longest a frame stays held while panes keep repainting: a window drag resizes
    // them with every pointer move, and an app can stream output after a resize. Either
    // would otherwise freeze the screen until the resize is forgotten.
    let max_hold = Duration::from_millis(120);
    // As if the last frame and keystroke were a quiet spell ago.
    let mut last_draw = Instant::now() - streaming_gap;
    let mut last_typed = last_draw;
    // Whether the last frame came within `streaming_gap` of the one before it.
    let mut streaming = false;
    let mut held_since: Option<Instant> = None;
    let mut redraw = true;
    // How long after `last_draw` the tab bar's animations (spinners, a blinking decision
    // mark, a scrolling label) next change; `None` while nothing on it moves, so an idle
    // window draws nothing.
    let mut tick: Option<Duration> = None;
    while !app.should_quit {
        app.refresh_metadata();
        let failures = checks.poll();
        if !failures.is_empty() {
            app.append_notice(&failures.join("\n"));
            redraw = true;
        }
        if tick.is_some_and(|tick| last_draw.elapsed() >= tick) {
            redraw = true;
        }
        if redraw && last_draw.elapsed() >= frame_interval(streaming, last_typed) {
            // Sized before the hold is checked: the resize is what makes an app repaint, and
            // a frame drawn before that would show the pane's old content cut to its new size.
            let size = window_size()?;
            ui::resize_sessions(&mut app, size)?;
            let overdue = held_since.is_some_and(|since| since.elapsed() >= max_hold);
            if overdue || !app.frame_held() {
                draw_frame(&mut terminal, &output, size, |frame| {
                    tick = ui::draw(frame, &mut app);
                })?;
                redraw = false;
                let now = Instant::now();
                streaming = now.duration_since(last_draw) < streaming_gap;
                last_draw = now;
                held_since = None;
            } else if held_since.is_none() {
                held_since = Some(Instant::now());
            }
        }
        redraw |= app.checkpoint(&mut store);
        let event = if redraw {
            // A frame held for an app's update or repaint is retried every few ms, so that it
            // goes out right after the app is done, or once the hold runs out.
            let wait = if app.frame_held() {
                Duration::from_millis(4)
            } else {
                frame_interval(streaming, last_typed).saturating_sub(last_draw.elapsed())
            };
            match rx.recv_timeout(wait) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            }
        } else {
            // Up to the next animation frame, else the metadata poll.
            let wait = tick.map_or(App::METADATA_INTERVAL, |tick| {
                tick.saturating_sub(last_draw.elapsed())
                    .min(App::METADATA_INTERVAL)
            });
            match rx.recv_timeout(wait) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            }
        };
        if let Some(event) = event {
            let mut handle = |event: Event| {
                if matches!(
                    event,
                    Event::Input(
                        crossterm::event::Event::Key(_) | crossterm::event::Event::Paste(_)
                    )
                ) {
                    last_typed = Instant::now();
                }
                match app.handle(event) {
                    Ok(changed) => redraw |= changed,
                    Err(error) => {
                        app.set_notice(format!("{error:#}"));
                        redraw = true;
                    }
                }
            };
            handle(event);
            for _ in 0..127 {
                let Ok(event) = rx.try_recv() else {
                    break;
                };
                handle(event);
            }
        }
    }
    // An agent started in a shell does not mark the state dirty; recheck before the final save.
    app.state_dirty = true;
    app.checkpoint(&mut store);
    signal_handle.close();
    drop(app);
    // Panes still starting leave FIFOs that would keep the server's folder from going.
    limiter.remove_gates();
    drop(server);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_panes_route_focus_resize_and_close_to_distinct_sessions() {
        for close_secondary in [false, true] {
            let (mut app, _queued, _events) = interaction_app();
            let directory = tempfile::tempdir().unwrap();
            for id in 1..=2 {
                app.project.workspaces[0].sessions.push(
                    Session::spawn(
                        id,
                        "shell",
                        None,
                        directory.path(),
                        &app.socket,
                        app.tx.clone(),
                        10,
                        40,
                        Some(("/bin/sleep".into(), vec!["30".into()])),
                        None,
                    )
                    .unwrap(),
                );
            }
            app.project.layout = LayoutMode::Vertical;
            app.project.secondary = Some(0);
            app.project.secondary_session = Some(1);
            app.project.split_ratio = 6_000;
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
            ui::resize_sessions(&mut app, ratatui::layout::Size::new(120, 24)).unwrap();
            terminal
                .draw(|frame| {
                    ui::draw(frame, &mut app);
                })
                .unwrap();
            for pane in &app.panes {
                assert_eq!(
                    app.project.workspaces[0].sessions[pane.session]
                        .parser
                        .lock()
                        .screen()
                        .size(),
                    (pane.body.height, pane.body.width)
                );
            }
            assert!(app.handle(Event::Dirty(1)).unwrap());
            assert!(app.handle(Event::Dirty(2)).unwrap());
            for side in [1, 0] {
                let body = app.panes[side].body;
                app.handle(Event::Input(crossterm::event::Event::Mouse(
                    crossterm::event::MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: body.x + 1,
                        row: body.y + 1,
                        modifiers: KeyModifiers::NONE,
                    },
                )))
                .unwrap();
                assert_eq!(app.focused_session().unwrap().id, side as u64 + 1);
            }
            app.project.focus_secondary = true;
            ui::resize_sessions(&mut app, ratatui::layout::Size::new(40, 10)).unwrap();
            assert_eq!(
                app.focused_session().unwrap().parser.lock().screen().size(),
                (7, 38)
            );
            // Selecting a session already visible in the peer moves focus, not its PTY.
            app.action(Action::FocusSession(0)).unwrap();
            assert!(!app.project.focus_secondary);
            assert_eq!(app.project.secondary_session, Some(1));
            app.project.focus_secondary = close_secondary;
            let peer = if close_secondary { 1 } else { 2 };
            app.action(Action::CloseSession).unwrap();
            app.modal_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                .unwrap();
            assert_eq!(app.focused_session().unwrap().id, peer);
            assert_eq!(app.focused().sessions.len(), 1);
            assert!(app.project.layout == LayoutMode::Full);
            assert!(app.project.secondary.is_none());
            assert!(app.project.secondary_session.is_none());
        }
    }

    #[test]
    fn held_frames_wait_for_the_app_to_finish_painting() {
        let (mut app, _queued, _events) = interaction_app();
        let directory = tempfile::tempdir().unwrap();
        app.project.workspaces[0].sessions.push(
            Session::spawn(
                1,
                "shell",
                None,
                directory.path(),
                &app.socket,
                app.tx.clone(),
                10,
                40,
                Some(("/bin/sleep".into(), vec!["30".into()])),
                None,
            )
            .unwrap(),
        );
        // Before a frame has said what it draws, nothing is held.
        assert!(!app.frame_held());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        ui::resize_sessions(&mut app, ratatui::layout::Size::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        assert!(!app.panes.is_empty());
        assert!(!app.frame_held());
        let parser = app.project.workspaces[0].sessions[0].parser.clone();
        parser.lock().process(b"\x1b[?2026h");
        assert!(app.frame_held());
        parser.lock().process(b"\x1b[?2026l");
        assert!(!app.frame_held());
        // A layout change empties `panes`; the frame still waits for the sessions it will draw.
        parser.lock().process(b"\x1b[?2026h");
        assert!(app.frame_held());
        app.panes.clear();
        assert!(app.frame_held());
        // Without that list it falls back to what the last frame drew, and with neither, holds
        // nothing.
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        app.next_panes.clear();
        assert!(app.frame_held());
        app.panes.clear();
        assert!(!app.frame_held());
    }

    #[test]
    fn layout_changes_wait_for_the_pane_to_repaint() {
        let (mut app, _queued, _events) = interaction_app();
        let directory = tempfile::tempdir().unwrap();
        let session = sleeping_session(&app, directory.path(), 1);
        let parser = session.parser.clone();
        app.project.workspaces[0].sessions.push(session);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        draw_frame(&mut app, &mut terminal);
        assert_eq!(parser.lock().screen().size(), (21, 118));

        // A pane that has painted nothing shows a placeholder, so no resize holds its frame.
        app.handle(key_event(KeyCode::F(2))).unwrap();
        ui::resize_sessions(&mut app, terminal.size().unwrap()).unwrap();
        assert_eq!(parser.lock().screen().size(), (21, 78));
        assert!(!app.frame_held());
        draw_frame(&mut app, &mut terminal);

        // One that has painted is held, though the layout change emptied `panes`.
        app.project.workspaces[0].sessions[0].mark_output();
        app.handle(key_event(KeyCode::F(2))).unwrap();
        assert!(app.panes.is_empty());
        ui::resize_sessions(&mut app, terminal.size().unwrap()).unwrap();
        assert_eq!(parser.lock().screen().size(), (21, 118));
        assert_eq!(app.next_panes, [(0, 0)]);
        assert!(app.frame_held());
        // The pointer resizes a dragged divider's panes with every move; they are not waited for.
        app.split_drag = Some(0);
        assert!(!app.frame_held());
        app.split_drag = None;
        assert!(app.frame_held());
        // Once its app has repainted and gone quiet, the frame is drawn.
        app.project.workspaces[0].sessions[0].note_output();
        std::thread::sleep(Duration::from_millis(40));
        assert!(!app.frame_held());
    }

    #[test]
    fn late_update_failures_join_the_notice_row_instead_of_replacing_it() {
        let (mut app, _queued, _events) = interaction_app();
        let omp = "OMP update failed: boom. Continuing with the installed version.";
        app.append_notice(omp);
        assert_eq!(app.notice.as_deref(), Some(omp));
        assert!(!app.notice_sticky);
        // A startup warning stays on screen, and stays a warning, with the failure beneath it.
        app.set_warning("Saved workspace could not be loaded.");
        let opencode = "OpenCode update failed: refused. Continuing with the installed version.";
        app.append_notice(opencode);
        assert_eq!(
            app.notice,
            Some(format!("Saved workspace could not be loaded.\n{opencode}"))
        );
        assert!(app.notice_sticky);
        app.clear_notice_on_input(false);
        assert!(app.notice.is_some());
    }

    #[test]
    fn shared_snapshot_remaps_sessions_and_never_restores_duplicate_terminals() {
        let (mut app, _queued, _events) = interaction_app();
        let directory = tempfile::tempdir().unwrap();
        app.project.workspaces.truncate(1);
        for id in 1..=3 {
            app.project.workspaces[0].sessions.push(
                Session::spawn(
                    id,
                    "shell",
                    None,
                    directory.path(),
                    &app.socket,
                    app.tx.clone(),
                    10,
                    40,
                    Some(("/bin/sleep".into(), vec!["30".into()])),
                    None,
                )
                .unwrap(),
            );
        }
        app.project.workspaces[0].sessions[0].kind = "logs".into();
        app.project.workspaces[0].selected = 1;
        app.project.layout = LayoutMode::Horizontal;
        app.project.secondary = Some(0);
        app.project.secondary_session = Some(2);
        app.project.focus_secondary = true;
        let store = restore::Store::open_in(directory.path()).unwrap();
        let snapshot = app.snapshot();
        assert_eq!(snapshot.projects[0].workspaces[0].selected, 0);
        assert_eq!(snapshot.projects[0].secondary_session, Some(1));
        store.save(&snapshot).unwrap();
        assert_eq!(store.load().unwrap(), Some(snapshot));

        app.project.secondary_session = Some(0);
        let snapshot = app.snapshot();
        assert_eq!(snapshot.projects[0].layout, restore::SavedLayout::Full);
        assert!(snapshot.projects[0].secondary_session.is_none());
        store.save(&snapshot).unwrap();
        assert_eq!(store.load().unwrap(), Some(snapshot));

        // A focused terminal that is not restorable must not collapse the split when its
        // peer is the first restorable one: shell B leads and shell A follows.
        app.project.workspaces[0].sessions[0].kind = "shell".into();
        app.project.workspaces[0].sessions[1].kind = "logs".into();
        app.project.layout = LayoutMode::Vertical;
        app.project.secondary_session = Some(0);
        let snapshot = app.snapshot();
        assert_eq!(snapshot.projects[0].layout, restore::SavedLayout::Vertical);
        assert_eq!(snapshot.projects[0].workspaces[0].selected, 1);
        assert_eq!(snapshot.projects[0].secondary_session, Some(0));
        store.save(&snapshot).unwrap();
        assert_eq!(store.load().unwrap(), Some(snapshot));
    }

    #[test]
    fn deletion_refusal_survives_background_environment_completion() {
        let (mut app, _queued, _events) = interaction_app();
        app.project.active = 1;
        app.project.workspaces[1].environment = Some(compose::Environment {
            project: "crit-other".into(),
            services: vec![compose::Service {
                name: "web".into(),
                state: "running".into(),
                ..Default::default()
            }],
        });
        app.action(Action::CloseWorktree).unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        // The worker checks current Docker state, not the cached environment.
        assert!(matches!(
            &app.modal,
            Some(Modal::Removal { error: None, .. })
        ));
        app.handle(Event::Removed {
            tree: "other".into(),
            result: Err("Docker environment is running".into()),
        })
        .unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::Removal { tree, error: Some(_), .. }) if tree == "other")
        );
        app.handle(Event::Environment {
            tree: "other".into(),
            operation: EnvironmentOp::Probe,
            prepared: false,
            result: Ok(compose::Environment::default()),
        })
        .unwrap();
        assert!(matches!(
            &app.modal,
            Some(Modal::Removal { error: Some(_), .. })
        ));
        assert_eq!(app.focused().tree.id, "other");
        assert_eq!(app.project.workspaces.len(), 2);
        app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.modal.is_none());
        assert_eq!(app.project.workspaces.len(), 2);
    }

    #[test]
    fn deletion_waits_for_result_and_keeps_a_failed_checkout_open() {
        let (mut app, _queued, _events) = interaction_app();
        app.project.active = 1;
        app.action(Action::CloseWorktree).unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        // The checkout is being removed: nothing else may start Docker work on it.
        assert!(app.project.workspaces[1].pending);
        for visible in [true, false] {
            app.interaction_visible = visible;
            // Only Esc leaves the progress dialog; other keys cannot answer it early.
            app.handle(Event::Input(crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))))
            .unwrap();
            assert!(matches!(
                &app.modal,
                Some(Modal::Removal { error: None, .. })
            ));
            assert_eq!(app.project.workspaces.len(), 2);
        }
        app.handle(Event::Removed {
            tree: "other".into(),
            result: Err("local changes".into()),
        })
        .unwrap();
        assert!(matches!(
            &app.modal,
            Some(Modal::Removal { error: Some(_), .. })
        ));
        assert_eq!(app.focused().tree.id, "other");
        assert!(!app.project.workspaces[1].pending);
        app.interaction_visible = true;
        app.modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(app.modal.is_none());
        assert_eq!(app.project.workspaces.len(), 2);
    }

    #[test]
    fn escaped_removal_and_split_finish_in_background() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, queued, _events) = interaction_app();
        // The surviving tab already has a session, so closing the focused one spawns nothing.
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[0].sessions.push(session);
        app.project.active = 1;
        let escape = || {
            Event::Input(crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
        };
        let delete = |app: &mut App| {
            app.action(Action::CloseWorktree).unwrap();
            for code in [KeyCode::Up, KeyCode::Enter] {
                app.modal_key(KeyEvent::new(code, KeyModifiers::NONE))
                    .unwrap();
            }
            assert!(matches!(
                &app.modal,
                Some(Modal::Removal { error: None, .. })
            ));
            assert!(matches!(queued.recv().unwrap(), Job::Remove { .. }));
        };

        // A refusal that arrives after Esc has no dialog left to show it: it is a notice.
        delete(&mut app);
        app.handle(escape()).unwrap();
        assert!(app.modal.is_none());
        assert_eq!(
            app.notice.as_deref(),
            Some("Still removing other; the result will appear here.")
        );
        app.handle(Event::Removed {
            tree: "other".into(),
            result: Err("local changes".into()),
        })
        .unwrap();
        assert!(app.modal.is_none());
        assert_eq!(app.notice.as_deref(), Some("local changes"));
        assert_eq!(app.project.workspaces.len(), 2);

        // Success still closes the tab, whichever Esc path hid the dialog.
        delete(&mut app);
        app.interaction_visible = false;
        app.handle(escape()).unwrap();
        app.interaction_visible = true;
        assert!(app.modal.is_none());
        assert!(app.notice.as_deref().unwrap().starts_with("Still removing"));
        app.handle(Event::Removed {
            tree: "other".into(),
            result: Ok(None),
        })
        .unwrap();
        assert_eq!(app.project.workspaces.len(), 1);
        assert_eq!(app.focused().tree.id, "main");
        assert_eq!(
            app.notice.as_deref(),
            Some("Checkout removed; Git branch and Docker volumes retained.")
        );

        // A split checkout that finishes after Esc lands as a tab and leaves the layout alone.
        let mut third = app.project.workspaces[0].tree.clone();
        third.id = "third".into();
        third.branch = "third".into();
        let opened = |app: &App, request, result| Event::SplitOpened {
            request,
            project: app.project.common_dir().unwrap().to_owned(),
            layout: LayoutMode::Vertical,
            choices: Vec::new(),
            selected: 0,
            source: app.project.path().to_owned(),
            preparation_error: None,
            result,
        };
        app.modal = Some(Modal::OpeningSplit { request: 9 });
        app.handle(escape()).unwrap();
        assert!(app.modal.is_none());
        assert_eq!(
            app.notice.as_deref(),
            Some("Still opening the split; the checkout will open as a tab.")
        );
        // A dialog opened meanwhile is not the result's to answer.
        app.modal = Some(Modal::Confirm(Confirm::Quit));
        let event = opened(&app, 9, Ok((third, true)));
        app.handle(event).unwrap();
        assert!(matches!(app.modal, Some(Modal::Confirm(Confirm::Quit))));
        app.modal = None;
        assert_eq!(app.project.workspaces.len(), 2);
        assert_eq!(app.project.workspaces[1].tree.id, "third");
        assert!(app.project.layout == LayoutMode::Full);
        assert!(app.project.secondary.is_none());
        assert_eq!(app.focused().tree.id, "main");
        assert_eq!(app.notice.as_deref(), Some("third is ready as a tab."));

        app.modal = Some(Modal::OpeningSplit { request: 10 });
        app.handle(escape()).unwrap();
        let event = opened(&app, 10, Err("branch no longer exists".into()));
        app.handle(event).unwrap();
        assert!(app.modal.is_none());
        assert_eq!(app.notice.as_deref(), Some("branch no longer exists"));
    }

    #[test]
    fn metadata_events_update_tabs_and_split_headers_without_changing_topology() {
        let (mut app, _queued, _events) = interaction_app();
        let directory = tempfile::tempdir().unwrap();
        let session = Session::spawn(
            1,
            "shell",
            None,
            directory.path(),
            &directory.path().join("unused.sock"),
            app.tx.clone(),
            22,
            100,
            Some((
                "/bin/sh".into(),
                vec!["-c".into(), "IFS= read -r line".into()],
            )),
            None,
        )
        .unwrap();
        let parser = session.parser.clone();
        let alive = session.alive.clone();
        app.project.workspaces[0].sessions.push(session);
        let (mut background, _, _) = interaction_app();
        background.project.name = "background".into();
        background.project.order = 1;
        background.project.repository = Some(worktree::Repository {
            root: PathBuf::from("/background"),
            common_dir: PathBuf::from("/background/.git"),
        });
        for workspace in &mut background.project.workspaces {
            workspace.tree.id = format!("background-{}", workspace.tree.id);
            workspace.tree.path = PathBuf::from("/background").join(&workspace.tree.id);
        }
        app.saved_projects.push(background.project);
        app.project.layout = LayoutMode::Vertical;
        app.project.secondary = Some(1);
        app.project.focus_secondary = true;
        app.project.split_ratio = 6_500;
        app.notice = Some("preserve existing notice".into());
        let mut switched = app.project.workspaces[0].tree.clone();
        switched.branch = "switched".into();
        let mut detached = app.project.workspaces[1].tree.clone();
        detached.branch.clear();
        detached.head = "0123456789abcdef".into();
        let mut renamed = app.saved_projects[0].workspaces[0].tree.clone();
        renamed.branch = "renamed".into();
        let listings = vec![
            (
                PathBuf::from("/interaction-fixture/.git"),
                vec![switched, detached],
            ),
            (PathBuf::from("/background/.git"), vec![renamed]),
        ];
        assert!(app.handle(Event::Metadata(listings)).unwrap());
        assert_eq!(app.project.active, 0);
        assert_eq!(app.project.secondary, Some(1));
        assert!(app.project.focus_secondary);
        assert!(app.project.layout == LayoutMode::Vertical);
        assert_eq!(app.project.split_ratio, 6_500);
        assert_eq!(app.saved_projects[0].workspaces[0].label(), "renamed");
        assert_eq!(app.notice.as_deref(), Some("preserve existing notice"));
        assert_eq!(app.project.workspaces[0].sessions.len(), 1);
        assert_eq!(app.project.workspaces[0].selected, 0);
        assert_eq!(app.project.workspaces[0].sessions[0].id, 1);
        assert!(Arc::ptr_eq(
            &app.project.workspaces[0].sessions[0].parser,
            &parser
        ));
        assert!(Arc::ptr_eq(
            &app.project.workspaces[0].sessions[0].alive,
            &alive
        ));

        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 24)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let row = |y| {
            (0..160)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(row(0).contains("switched"));
        assert!(row(0).contains("detached HEAD"));
        assert!(row(0).contains("renamed"));
        assert!(row(1).contains("switched"));
        assert!(row(1).contains("detached HEAD"));
        assert!(!app.handle(Event::Metadata(Vec::new())).unwrap());
        assert_eq!(app.project.workspaces[1].label(), "detached HEAD");
    }

    #[test]
    fn metadata_polling_keeps_only_one_refresh_in_flight() {
        let (mut app, queued, _events) = interaction_app();
        app.refresh_metadata();
        assert!(matches!(
            queued.try_recv().unwrap(),
            Job::RefreshMetadata(_)
        ));
        app.metadata_checked = Instant::now() - App::METADATA_INTERVAL;
        app.refresh_metadata();
        assert!(queued.try_recv().is_err());
        assert!(!app.handle(Event::Metadata(Vec::new())).unwrap());
        app.refresh_metadata();
        assert!(queued.try_recv().is_err());
        app.metadata_checked = Instant::now() - App::METADATA_INTERVAL;
        app.refresh_metadata();
        assert!(matches!(
            queued.try_recv().unwrap(),
            Job::RefreshMetadata(_)
        ));
    }

    #[test]
    fn split_grip_drag_resizes_both_panes_without_changing_focus() {
        // Either pane-edge grip must grab without a one-column jump.
        for grab in 0..2 {
            let (mut app, _queued, _events) = interaction_app();
            app.project.layout = LayoutMode::Vertical;
            app.project.secondary = Some(1);
            app.project.focus_secondary = true;
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
            terminal
                .draw(|frame| {
                    ui::draw(frame, &mut app);
                })
                .unwrap();
            let handle = app.split_divider.unwrap().handle;
            let mouse = |kind, column, row| {
                Event::Input(crossterm::event::Event::Mouse(
                    crossterm::event::MouseEvent {
                        kind,
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    },
                ))
            };
            let initial_ratio = app.project.split_ratio;
            app.handle(mouse(
                MouseEventKind::Down(MouseButton::Left),
                handle.x + grab,
                handle.y,
            ))
            .unwrap();
            app.handle(mouse(
                MouseEventKind::Drag(MouseButton::Left),
                handle.x + grab,
                handle.y,
            ))
            .unwrap();
            assert_eq!(app.project.split_ratio, initial_ratio);
            app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 78 + grab, 0))
                .unwrap();
            terminal
                .draw(|frame| {
                    ui::draw(frame, &mut app);
                })
                .unwrap();
            assert_eq!(app.split_divider.unwrap().track.x, 79);
            assert_eq!(app.panes[0].body.width, 77);
            assert_eq!(app.panes[1].body.width, 39);
            assert!(app.project.focus_secondary);
            assert!(app.selection.is_none());

            // Capture continues outside the grip, but neither pane can collapse.
            app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 0, 0))
                .unwrap();
            terminal
                .draw(|frame| {
                    ui::draw(frame, &mut app);
                })
                .unwrap();
            assert_eq!(app.panes[0].body.width, 26);
            app.handle(mouse(
                MouseEventKind::Up(MouseButton::Left),
                u16::MAX,
                u16::MAX,
            ))
            .unwrap();
            terminal
                .draw(|frame| {
                    ui::draw(frame, &mut app);
                })
                .unwrap();
            assert_eq!(app.panes[1].body.width, 26);
            assert!(app.split_drag.is_none());
            let released = app.project.split_ratio;
            app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 0, 0))
                .unwrap();
            assert_eq!(app.project.split_ratio, released);
        }
    }

    #[test]
    fn split_drag_cannot_survive_resize_or_closing_a_pane() {
        let (mut app, _queued, _events) = interaction_app();
        app.project.layout = LayoutMode::Vertical;
        app.project.secondary = Some(1);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let mouse = |kind, column, row| {
            Event::Input(crossterm::event::Event::Mouse(
                crossterm::event::MouseEvent {
                    kind,
                    column,
                    row,
                    modifiers: KeyModifiers::NONE,
                },
            ))
        };
        let handle = app.split_divider.unwrap().handle;
        app.handle(mouse(
            MouseEventKind::Down(MouseButton::Left),
            handle.x,
            handle.y,
        ))
        .unwrap();
        app.handle(Event::Input(crossterm::event::Event::Resize(40, 10)))
            .unwrap();
        let ratio = app.project.split_ratio;
        app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 0, 0))
            .unwrap();
        assert_eq!(app.project.split_ratio, ratio);
        assert!(app.split_drag.is_none());
        let mut small = Terminal::new(ratatui::backend::TestBackend::new(40, 10)).unwrap();
        small
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        assert!(app.split_divider.is_none());
        assert_eq!(app.panes.len(), 1);

        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let handle = app.split_divider.unwrap().handle;
        app.handle(mouse(
            MouseEventKind::Down(MouseButton::Left),
            handle.x,
            handle.y,
        ))
        .unwrap();
        app.close_workspace();
        app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 0, 0))
            .unwrap();
        assert_eq!(app.project.split_ratio, ratio);
        assert!(app.split_divider.is_none());
        assert!(app.split_drag.is_none());
    }

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
            None,
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
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
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
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
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
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        assert_eq!(app.panes[0].body.bottom(), 13);
        app.notice = None;
        ui::resize_sessions(&mut app, ratatui::layout::Size::new(40, 14)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
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
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
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
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
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

    /// The exit of session `session` as its wait thread reports it, long after its start.
    fn exited(session: u64, success: bool) -> Event {
        let code = if success {
            "Success"
        } else {
            "Exited with code 1"
        };
        Event::Exited {
            session,
            code: code.into(),
            success,
            ran: Duration::from_secs(5),
        }
    }

    #[test]
    fn ctrl_d_goes_to_the_agent() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let output = directory.path().join("received");
        let mut session = recording_session(1, directory.path(), app.tx.clone(), &output);
        session.kind = "omp".into();
        app.project.workspaces[0].sessions.push(session);
        app.handle(Event::Input(crossterm::event::Event::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        ))))
        .unwrap();
        wait_for_file(&output, b"\x04");
        assert!(app.modal.is_none());
        assert_eq!(app.focused().sessions.len(), 1);
        assert_eq!(app.focused_session().unwrap().id, 1);
    }

    #[test]
    fn exited_split_pane_leaves_the_other_full_width_and_focused() {
        let directory = tempfile::tempdir().unwrap();
        // Two sessions of one workspace side by side, the second one focused; either can end.
        for (ended, survivor) in [(2, 1), (1, 2)] {
            let (mut app, _queued, _events) = interaction_app();
            for id in 1..=2 {
                let session = sleeping_session(&app, directory.path(), id);
                app.project.workspaces[0].sessions.push(session);
            }
            app.project.layout = LayoutMode::Vertical;
            app.project.secondary = Some(0);
            app.project.secondary_session = Some(1);
            app.project.focus_secondary = true;
            app.handle(exited(ended, true)).unwrap();
            assert!(app.project.layout == LayoutMode::Full);
            assert_eq!(app.project.secondary, None);
            assert_eq!(app.project.secondary_session, None);
            assert_eq!(app.focused().sessions.len(), 1);
            assert_eq!(app.focused_session().unwrap().id, survivor);
            // A session closing inside a tab has nothing to say.
            assert!(app.notice.is_none());
        }

        // Two workspaces side by side: the focused one's only session ends, and its tab closes.
        let (mut app, _queued, _events) = interaction_app();
        for (workspace, id) in [(0, 1), (1, 2)] {
            let session = sleeping_session(&app, directory.path(), id);
            app.project.workspaces[workspace].sessions.push(session);
        }
        app.project.layout = LayoutMode::Horizontal;
        app.project.secondary = Some(1);
        app.project.focus_secondary = true;
        app.handle(exited(2, true)).unwrap();
        assert!(app.project.layout == LayoutMode::Full);
        assert_eq!(app.project.secondary, None);
        assert_eq!(app.project.workspaces.len(), 1);
        assert_eq!(app.focused().tree.id, "main");
        assert_eq!(app.focused_session().unwrap().id, 1);
        assert_eq!(app.notice.as_deref(), Some(TAB_CLOSED));
        assert!(!app.should_quit);
    }

    #[test]
    fn exited_session_gives_way_to_its_neighbour() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        for id in 1..=4 {
            let session = sleeping_session(&app, directory.path(), id);
            app.project.workspaces[0].sessions.push(session);
        }
        let ids = |app: &App| -> Vec<u64> { app.focused().sessions.iter().map(|s| s.id).collect() };
        // The selected session gives way to the next one.
        app.project.workspaces[0].selected = 1;
        app.handle(exited(2, true)).unwrap();
        assert_eq!(ids(&app), [1, 3, 4]);
        assert_eq!(app.focused_session().unwrap().id, 3);
        // One that is not shown leaves the selection on its session.
        app.handle(exited(1, false)).unwrap();
        assert_eq!(ids(&app), [3, 4]);
        assert_eq!(app.focused_session().unwrap().id, 3);
        // The last one gives way to the one before it.
        app.project.workspaces[0].selected = 1;
        app.handle(exited(4, true)).unwrap();
        assert_eq!(ids(&app), [3]);
        assert_eq!(app.focused_session().unwrap().id, 3);
        assert!(app.notice.is_none());
        assert!(app.state_dirty);
    }

    #[test]
    fn exited_last_session_closes_its_tab_and_the_last_tab_ends_crit() {
        let directory = tempfile::tempdir().unwrap();
        let start = || {
            let (mut app, queued, events) = interaction_app();
            for (workspace, id) in [(0, 1), (1, 2)] {
                let session = sleeping_session(&app, directory.path(), id);
                app.project.workspaces[workspace].sessions.push(session);
            }
            (app, queued, events)
        };
        // A tab in the background closes without moving the focus.
        let (mut app, _queued, _events) = start();
        app.handle(exited(2, true)).unwrap();
        assert_eq!(app.project.workspaces.len(), 1);
        assert_eq!(app.focused_session().unwrap().id, 1);
        assert_eq!(app.notice.as_deref(), Some(TAB_CLOSED));

        let (mut app, _queued, _events) = start();
        // An agent that quits closes like a shell that ends.
        app.project.workspaces[1].sessions[0].kind = "omp".into();
        app.handle(exited(1, true)).unwrap();
        assert_eq!(app.project.workspaces.len(), 1);
        assert_eq!(app.focused().tree.id, "other");
        assert_eq!(app.focused_session().unwrap().id, 2);
        assert_eq!(app.notice.as_deref(), Some(TAB_CLOSED));
        assert!(!app.should_quit);
        app.handle(exited(2, true)).unwrap();
        assert!(app.should_quit);
        // As Kitty after its last window: the next launch starts at the chooser.
        assert!(app.snapshot().projects.is_empty());
    }

    #[test]
    fn failed_agent_stays_with_its_exit_status() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let mut session = sleeping_session(&app, directory.path(), 1);
        session.kind = "opencode".into();
        app.project.workspaces[0].sessions.push(session);
        app.handle(exited(1, false)).unwrap();
        let session = app.focused_session().unwrap();
        assert_eq!(session.id, 1);
        assert_eq!(session.state, AgentState::Exited);
        assert_eq!(session.message.as_deref(), Some("Exited with code 1"));
        assert_eq!(app.project.workspaces.len(), 2);
        assert!(app.notice.is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn agent_that_quits_as_it_starts_stays_but_a_shell_closes() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        app.project.workspaces.truncate(1);
        for id in 1..=2 {
            let session = sleeping_session(&app, directory.path(), id);
            app.project.workspaces[0].sessions.push(session);
        }
        app.project.workspaces[0].sessions[0].kind = "omp".into();
        let at_once = |session| Event::Exited {
            session,
            code: "Success".into(),
            success: true,
            ran: App::AGENT_STARTUP - Duration::from_millis(1),
        };
        app.handle(at_once(2)).unwrap();
        assert_eq!(app.focused().sessions.len(), 1);
        // The last pane, which would take Crit with it.
        app.handle(at_once(1)).unwrap();
        assert!(!app.should_quit);
        let session = app.focused_session().unwrap();
        assert_eq!(session.state, AgentState::Exited);
        assert_eq!(session.message.as_deref(), Some("Success"));
    }

    #[test]
    fn exits_in_background_projects_close_there() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[0].sessions.push(session);
        let (mut background, _, _) = interaction_app();
        background.project.name = "background".into();
        background.project.order = 1;
        // Its first tab has sessions 2 and 3, with 3 selected; its second tab has 4 alone.
        for (workspace, id) in [(0, 2), (0, 3), (1, 4)] {
            let session = sleeping_session(&app, directory.path(), id);
            background.project.workspaces[workspace]
                .sessions
                .push(session);
        }
        background.project.workspaces[0].selected = 1;
        app.saved_projects.push(background.project);
        // A hidden session goes, and its tab keeps its selected session.
        app.handle(exited(2, true)).unwrap();
        let saved = &app.saved_projects[0].workspaces[0];
        assert_eq!(saved.sessions.len(), 1);
        assert_eq!(saved.sessions[0].id, 3);
        assert_eq!(saved.selected, 0);
        assert!(app.notice.is_none());
        // A background tab left without sessions closes, and the current project stays put.
        app.handle(exited(4, true)).unwrap();
        assert_eq!(app.saved_projects[0].workspaces.len(), 1);
        assert_eq!(app.notice.as_deref(), Some(TAB_CLOSED));
        assert_eq!(app.project.workspaces.len(), 2);
        assert_eq!(app.focused_session().unwrap().id, 1);
        // So does a background project left without tabs.
        app.notice = None;
        app.handle(exited(3, true)).unwrap();
        assert!(app.saved_projects.is_empty());
        assert_eq!(app.notice.as_deref(), Some(TAB_CLOSED));
        assert_eq!(app.focused_session().unwrap().id, 1);

        // The current project, left without tabs, gives way to the other one.
        let (mut app, _queued, _events) = interaction_app();
        app.project.workspaces.truncate(1);
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[0].sessions.push(session);
        let (mut background, _, _) = interaction_app();
        background.project.name = "background".into();
        background.project.order = 1;
        let session = sleeping_session(&app, directory.path(), 2);
        background.project.workspaces[0].sessions.push(session);
        app.saved_projects.push(background.project);
        app.handle(exited(1, true)).unwrap();
        assert!(!app.should_quit);
        assert_eq!(app.project.name, "background");
        assert!(app.saved_projects.is_empty());
        assert_eq!(app.focused_session().unwrap().id, 2);
        assert_eq!(app.notice.as_deref(), Some(TAB_CLOSED));
    }

    #[test]
    fn a_confirmation_about_the_focus_goes_when_its_subject_closes() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        for (workspace, id) in [(0, 1), (0, 2), (0, 3), (1, 4), (1, 5)] {
            let session = sleeping_session(&app, directory.path(), id);
            app.project.workspaces[workspace].sessions.push(session);
        }
        let asks = |app: &App, close_tab: bool| match &app.modal {
            Some(Modal::Confirm(Confirm::CloseWorktree { .. })) => close_tab,
            Some(Modal::Confirm(Confirm::CloseSession)) => !close_tab,
            _ => false,
        };
        // The focused session goes: Close session would end the next one.
        app.modal = Some(Modal::Confirm(Confirm::CloseSession));
        app.handle(exited(1, true)).unwrap();
        assert!(app.modal.is_none());
        // The next focused session goes too, but its tab stays: Close tab still means that tab.
        app.modal = Some(Modal::Confirm(Confirm::CloseWorktree { selected: 2 }));
        app.handle(exited(2, true)).unwrap();
        assert!(asks(&app, true));
        // A session of another tab goes: the focused session is still the one to close.
        app.modal = Some(Modal::Confirm(Confirm::CloseSession));
        app.handle(exited(4, true)).unwrap();
        assert!(asks(&app, false));
        // The focused session goes with its tab: Close tab would close the next one.
        app.modal = Some(Modal::Confirm(Confirm::CloseWorktree { selected: 2 }));
        app.handle(exited(3, true)).unwrap();
        assert!(app.modal.is_none());
        assert_eq!(app.focused_session().unwrap().id, 5);
    }

    #[test]
    fn ctrl_shift_w_on_the_last_session_closes_its_tab() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        for (workspace, id) in [(0, 1), (1, 2)] {
            let session = sleeping_session(&app, directory.path(), id);
            app.project.workspaces[workspace].sessions.push(session);
        }
        app.action(Action::CloseSession).unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.modal.is_none());
        assert_eq!(app.project.workspaces.len(), 1);
        assert_eq!(app.focused_session().unwrap().id, 2);
        assert_eq!(app.notice.as_deref(), Some(TAB_CLOSED));
        app.action(Action::CloseSession).unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.should_quit);
    }

    #[test]
    fn open_intent_tolerates_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;
        let (mut app, _queued, _events) = interaction_app();
        let bad = |path: &[u8]| PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()));
        app.project.repository = Some(worktree::Repository {
            root: bad(b"/interaction-fixture/bad-\xff"),
            common_dir: bad(b"/interaction-fixture/bad-\xff/.git"),
        });
        app.project.workspaces[0].tree.path = bad(b"/interaction-fixture/bad-\xff");
        app.open_intent().unwrap();
        let Some(Modal::Intent(dialog)) = &app.modal else {
            panic!("expected interpreter dialog");
        };
        for path in [
            &dialog.context["active_project"]["path"],
            &dialog.context["active_project"]["common_dir"],
            &dialog.context["focused_worktree"]["path"],
        ] {
            assert!(path.is_string());
        }
    }

    #[test]
    fn input_reaching_the_terminal_clears_the_notice() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[0].sessions.push(session);
        for native_keys in [false, true] {
            app.native_keys = native_keys;
            // Pointer motion is not an acknowledgement.
            app.notice = Some("Tab closed".into());
            app.handle(mouse_event(MouseEventKind::Moved, 0, 0))
                .unwrap();
            assert!(app.notice.is_some());
            for event in [
                Event::Input(crossterm::event::Event::Key(KeyEvent::new(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE,
                ))),
                Event::Input(crossterm::event::Event::Paste("a".into())),
                mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0),
            ] {
                app.notice = Some("Tab closed".into());
                // The notice row must disappear on the next frame.
                assert!(app.handle(event).unwrap());
                assert!(app.notice.is_none());
            }
        }
    }

    #[test]
    fn input_forwarded_to_a_pane_draws_no_frame_of_its_own() {
        // The program draws its answer, if any, through its own output.
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[0].sessions.push(session);
        for native_keys in [false, true] {
            app.native_keys = native_keys;
            assert!(!app.handle(key_event(KeyCode::Char('a'))).unwrap());
            assert!(
                !app.handle(Event::Input(crossterm::event::Event::Paste("a".into())))
                    .unwrap()
            );
        }
    }

    #[test]
    fn input_forwarded_to_a_scrolled_back_pane_redraws_it_live() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        session
            .parser
            .lock()
            .process("line\r\n".repeat(30).as_bytes());
        app.project.workspaces[0].sessions.push(session);
        let scrollback = |app: &App| {
            app.project.workspaces[0].sessions[0]
                .parser
                .lock()
                .screen()
                .scrollback()
        };
        for native_keys in [false, true] {
            app.native_keys = native_keys;
            for event in [
                key_event(KeyCode::Char('a')),
                Event::Input(crossterm::event::Event::Paste("a".into())),
            ] {
                app.project.workspaces[0].sessions[0].scroll_to(5);
                assert_eq!(scrollback(&app), 5);
                assert!(app.handle(event).unwrap());
                assert_eq!(scrollback(&app), 0);
            }
        }
    }

    #[test]
    fn input_forwarded_to_a_pane_redraws_to_clear_the_selection() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[0].sessions.push(session);
        for native_keys in [false, true] {
            app.native_keys = native_keys;
            for event in [
                key_event(KeyCode::Char('a')),
                Event::Input(crossterm::event::Event::Paste("a".into())),
            ] {
                let selection = Selection::new(
                    1,
                    Rect::new(0, 1, 40, 10),
                    app.project.workspaces[0].sessions[0].parser.lock().screen(),
                    0,
                    1,
                );
                app.selection = Some(selection);
                assert!(app.handle(event).unwrap());
                assert!(app.selection.is_none());
            }
        }
    }

    #[test]
    fn sticky_warnings_survive_terminal_input_until_escape() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[0].sessions.push(session);
        let key = |code| {
            Event::Input(crossterm::event::Event::Key(KeyEvent::new(
                code,
                KeyModifiers::NONE,
            )))
        };
        for native_keys in [false, true] {
            app.native_keys = native_keys;
            for event in [
                key(KeyCode::Char('a')),
                Event::Input(crossterm::event::Event::Paste("a".into())),
                mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0),
            ] {
                app.set_warning("Saving disabled");
                app.handle(event).unwrap();
                assert_eq!(app.notice.as_deref(), Some("Saving disabled"));
            }
            // Esc dismisses it over a pane with a session and over one without.
            for active in [0, 1] {
                app.project.active = active;
                app.set_warning("Saving disabled");
                assert!(app.handle(key(KeyCode::Esc)).unwrap());
                assert!(app.notice.is_none());
            }
            app.project.active = 0;
            // An ordinary notice replaces a warning, and clears on the next key again.
            app.set_warning("Saving disabled");
            app.set_notice("Tab closed");
            app.handle(key(KeyCode::Char('a'))).unwrap();
            assert!(app.notice.is_none());
        }
        // A failed save raises exactly such a warning.
        let state = directory.path().join("crit");
        let mut store = Some(restore::Store::open_in(&state).unwrap());
        std::fs::remove_dir_all(&state).unwrap();
        app.set_notice("Tab closed");
        app.state_dirty = true;
        assert!(app.checkpoint(&mut store));
        assert!(store.is_none());
        assert!(app.notice_sticky);
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Saving disabled for this window"))
        );
    }

    #[test]
    fn restore_leaves_out_missing_items_and_keeps_the_rest() {
        let directory = tempfile::tempdir().unwrap();
        let kept = directory.path().join("kept");
        let gone = directory.path().join("gone");
        std::fs::create_dir(&kept).unwrap();
        std::fs::create_dir(&gone).unwrap();
        let saved = |path: &Path, kinds: &[&str], split: bool| restore::SavedProject {
            workspaces: vec![restore::SavedWorkspace {
                path: path.to_path_buf(),
                source: None,
                sessions: kinds
                    .iter()
                    .map(|kind| restore::SavedSession {
                        kind: (*kind).into(),
                        resume: None,
                    })
                    .collect(),
                selected: 0,
            }],
            active: 0,
            secondary: split.then_some(0),
            secondary_session: split.then_some(1),
            focus_secondary: split,
            layout: if split {
                restore::SavedLayout::Vertical
            } else {
                restore::SavedLayout::Full
            },
            split_ratio: restore::default_split_ratio(),
        };
        let snapshot = |projects, active_project| restore::Snapshot {
            version: restore::VERSION,
            projects,
            active_project,
            native_keys: false,
            agent_cpu_limited: true,
            agent_cpu_percent: limits::DEFAULT_PERCENT,
        };
        // The shared split's second chat cannot start, and the current project is deleted.
        let both = snapshot(
            vec![
                saved(&kept, &["shell", "bogus"], true),
                saved(&gone, &["shell"], false),
            ],
            1,
        );
        std::fs::remove_dir(&gone).unwrap();
        let (tx, _events) = mpsc::sync_channel(64);
        let socket = directory.path().join("unused.sock");
        let size = ratatui::layout::Size::new(80, 24);
        let limiter = limits::Limiter::off();
        let mut next_session = 1;
        let (restored, skipped) =
            restore_projects(&both, &socket, &limiter, &tx, size, &mut next_session);
        let (current, others) = restored.expect("the surviving project reopens");
        assert_eq!(current.name, "kept");
        assert!(others.is_empty());
        assert_eq!(current.workspaces[0].sessions.len(), 1);
        assert!(current.layout == LayoutMode::Full);
        assert_eq!(
            (
                current.secondary,
                current.secondary_session,
                current.focus_secondary
            ),
            (None, None, false)
        );
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped.iter().any(|line| line.contains("bogus session in")));
        assert!(
            skipped
                .iter()
                .any(|line| line.contains(gone.to_str().unwrap()))
        );

        // What is left is a state that can be saved again.
        let (mut app, _queued, _app_events) = interaction_app();
        app.project = current;
        app.saved_projects = others;
        restore::Store::open_in(directory.path().join("state"))
            .unwrap()
            .save(&app.snapshot())
            .unwrap();

        // With every project gone nothing reopens, and no session id is used up.
        let before = next_session;
        let only_gone = snapshot(vec![saved(&gone, &["shell"], false)], 0);
        let (restored, skipped) =
            restore_projects(&only_gone, &socket, &limiter, &tx, size, &mut next_session);
        assert!(restored.is_none());
        assert_eq!(skipped.len(), 1);
        assert_eq!(next_session, before);
    }

    /// The child half of `panic_message_survives_leaving_the_alternate_screen`. It holds the
    /// terminal as Crit does, then panics on its thread, and does nothing unless asked to.
    #[test]
    fn panic_child_holding_the_terminal() {
        if std::env::var_os("CRIT_TEST_PANIC_CHILD").is_none() {
            return;
        }
        let _guard = TerminalGuard::enter(false).unwrap();
        install_panic_hook(false);
        print!("ui-frame\r\n");
        io::stdout().flush().unwrap();
        panic!("PANIC-MARKER on the UI thread");
    }

    #[test]
    fn panic_message_survives_leaving_the_alternate_screen() {
        let (tx, events) = mpsc::sync_channel(4096);
        let session = Session::spawn(
            1,
            "shell",
            None,
            Path::new("/"),
            Path::new("/unused.sock"),
            tx,
            30,
            120,
            Some((
                "/usr/bin/env".into(),
                vec![
                    "CRIT_TEST_PANIC_CHILD=1".into(),
                    std::env::current_exe().unwrap().display().to_string(),
                    "--exact".into(),
                    "app::tests::panic_child_holding_the_terminal".into(),
                    "--nocapture".into(),
                    "--test-threads=1".into(),
                ],
            )),
            None,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Event::Exited { .. }) => break,
                Ok(_) => {}
                Err(_) => panic!("the child did not exit"),
            }
        }
        // The message is printed after the terminal is restored, so once it is on the screen
        // it is on the normal one. A lost message never arrives.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !session
            .parser
            .lock()
            .screen()
            .contents()
            .contains("PANIC-MARKER")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let parser = session.parser.lock();
        let screen = parser.screen();
        assert!(
            screen.contents().contains("PANIC-MARKER"),
            "the panic message was discarded with the alternate screen: {:?}",
            screen.contents()
        );
        assert!(!screen.alternate_screen());
    }

    #[test]
    fn non_utf8_projects_are_left_out_of_the_snapshot() {
        use std::os::unix::ffi::OsStringExt;
        let non_utf8 = || PathBuf::from(std::ffi::OsString::from_vec(b"/x/caf\xe9".to_vec()));
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let mut odd = Project::terminal(directory.path()).unwrap();
        odd.workspaces[0].tree.path = non_utf8();
        odd.order = 1;
        app.saved_projects.push(odd);
        app.project.workspaces[0].source = Some(non_utf8());
        let snapshot = app.snapshot();
        assert_eq!(snapshot.projects.len(), 1);
        assert_eq!(snapshot.projects[0].workspaces[0].source, None);
        // Saving would otherwise fail on the path and switch saving off for the window.
        restore::Store::open_in(directory.path().join("state"))
            .unwrap()
            .save(&snapshot)
            .unwrap();
    }

    #[test]
    fn tiny_terminal_keeps_last_usable_pty_size() {
        use ratatui::layout::Size;
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        let parser = session.parser.clone();
        app.project.workspaces[0].sessions.push(session);
        ui::resize_sessions(&mut app, Size::new(80, 24)).unwrap();
        assert_eq!(parser.lock().screen().size(), (21, 78));
        // Nothing is drawn below 30x10, so the program keeps its last usable size.
        for tiny in [
            Size::new(3, 10),
            Size::new(40, 4),
            Size::new(29, 10),
            Size::new(30, 9),
        ] {
            ui::resize_sessions(&mut app, tiny).unwrap();
            assert_eq!(parser.lock().screen().size(), (21, 78));
        }
        ui::resize_sessions(&mut app, Size::new(30, 10)).unwrap();
        assert_eq!(parser.lock().screen().size(), (7, 28));
    }

    #[test]
    fn notice_dismissing_press_keeps_its_selection() {
        use ratatui::layout::Size;
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        session.parser.lock().process(b"hello");
        app.project.workspaces[0].sessions.push(session);
        let size = Size::new(40, 14);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(40, 14)).unwrap();
        app.notice = Some("Environment ready".into());
        ui::resize_sessions(&mut app, size).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let reduced = app.panes[0].body;
        app.handle(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            reduced.x,
            reduced.y,
        ))
        .unwrap();
        assert!(app.notice.is_none());
        // The press removed the notice, so the next frame's body is one row taller.
        ui::resize_sessions(&mut app, size).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let full = app.panes[0].body;
        assert_eq!(full.height, reduced.height + 1);
        app.handle(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            reduced.x + 4,
            reduced.y,
        ))
        .unwrap();
        let selection = app.selection.as_ref().expect("the press began a selection");
        assert_eq!(selection.body, full);
        assert!(selection.moved);
        assert_eq!(selection.text(), "hello");
        // A change to the left edge, top edge or width still invalidates the frozen rows.
        ui::resize_sessions(&mut app, Size::new(50, 14)).unwrap();
        assert!(app.selection.is_none());
    }

    #[test]
    fn worktree_dialogs_name_detached_checkouts() {
        let (mut app, _queued, _events) = interaction_app();
        let mut detached = app.project.workspaces[0].tree.clone();
        detached.branch.clear();
        detached.head = "0123456789abcdef".into();
        app.project.workspaces[0].tree = detached.clone();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        for (modal, expected) in [
            (
                Modal::NewWorktree {
                    input: String::new(),
                    split: None,
                },
                "from detached HEAD",
            ),
            (
                Modal::NewWorktree {
                    input: String::new(),
                    split: Some(LayoutMode::Vertical),
                },
                "side by side with detached HEAD.",
            ),
            (
                Modal::OpenWorktree {
                    trees: vec![detached.clone()],
                    selected: 0,
                },
                "detached HEAD  /interaction-fixture/main",
            ),
            (
                Modal::ExistingSplit {
                    layout: LayoutMode::Vertical,
                    choices: vec![worktree::SplitTarget::Worktree(detached.clone())],
                    selected: 0,
                    error: None,
                },
                "detached HEAD  /interaction-fixture/main",
            ),
        ] {
            app.modal = Some(modal);
            terminal
                .draw(|frame| {
                    ui::draw(frame, &mut app);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rows: Vec<String> = (0..buffer.area.height)
                .map(|row| {
                    (0..buffer.area.width)
                        .map(|column| buffer[(column, row)].symbol())
                        .collect()
                })
                .collect();
            assert!(
                rows.iter().any(|row| row.contains(expected)),
                "{expected:?} missing from {rows:#?}"
            );
        }
    }

    #[test]
    fn long_notices_keep_their_tail() {
        let (mut app, _queued, _events) = interaction_app();
        app.set_notice("x".repeat(300_000) + "\nfinal cause");
        let notice = app.notice.as_deref().unwrap();
        assert!(notice.len() <= 16 * 1024 + 3);
        assert!(notice.starts_with("..."));
        assert!(notice.ends_with("final cause"));
        // A cut inside a multi-byte character moves on to the next boundary.
        app.set_notice("界".repeat(20_000));
        let notice = app.notice.as_deref().unwrap();
        assert!(notice.len() <= 16 * 1024 + 3);
        assert!(notice.starts_with("...界"));
        app.set_notice("short");
        assert_eq!(app.notice.as_deref(), Some("short"));
        // Paragraph scrolls by a u16, so an unbounded notice showed a middle slice.
        app.set_notice(
            (0..70_000)
                .map(|row| format!("line{row}\n"))
                .collect::<String>(),
        );
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(40, 14)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let last: String = (0..40)
            .map(|column| buffer[(column, 13)].symbol())
            .collect();
        assert!(last.contains("line69999"), "{last:?}");
    }

    #[test]
    fn notices_keep_split_panes() {
        let (mut app, _queued, _events) = interaction_app();
        app.project.secondary = Some(1);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 18)).unwrap();
        for layout in [LayoutMode::Horizontal, LayoutMode::Vertical] {
            app.project.layout = layout;
            // The window is tall enough for both panes; the rows a notice takes are not.
            for notice in [None, Some("one\ntwo\nthree")] {
                app.notice = notice.map(Into::into);
                terminal
                    .draw(|frame| {
                        ui::draw(frame, &mut app);
                    })
                    .unwrap();
                assert_eq!(app.panes.len(), 2);
            }
        }
    }

    #[test]
    fn notice_rows_keep_newest_output() {
        use ratatui::layout::Size;
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        let parser = session.parser.clone();
        app.project.workspaces[0].sessions.push(session);
        let size = Size::new(80, 24);
        ui::resize_sessions(&mut app, size).unwrap();
        {
            let mut parser = parser.lock();
            for row in 1..=30 {
                parser.process(format!("line{row}\r\n").as_bytes());
            }
            parser.process(b"ERROR\r\nprompt$ ");
        }
        let assert_newest_output_visible = || {
            let parser = parser.lock();
            let screen = parser.screen();
            let visible: Vec<String> = screen.rows(0, 78).collect();
            let cursor = usize::from(screen.cursor_position().0);
            assert!(visible[cursor].starts_with("prompt$"), "{visible:?}");
            assert_eq!(visible[cursor - 1], "ERROR");
            assert_eq!(visible[cursor - 2], "line30");
        };
        app.notice = Some("Environment ready".into());
        ui::resize_sessions(&mut app, size).unwrap();
        assert_newest_output_visible();
        {
            // The row the notice took left through the top into history, not the bin.
            let mut parser = parser.lock();
            assert_eq!(parser.screen().size(), (20, 78));
            parser.screen_mut().set_scrollback(usize::MAX);
            assert_eq!(parser.screen().scrollback(), 12);
            assert_eq!(parser.screen().rows(0, 78).next().as_deref(), Some("line1"));
            parser.screen_mut().set_scrollback(0);
        }
        app.notice = None;
        ui::resize_sessions(&mut app, size).unwrap();
        assert_newest_output_visible();
    }

    #[test]
    fn clicking_the_focused_pane_does_not_mark_state_dirty() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[0].sessions.push(session);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        app.state_dirty = false;
        let body = app.panes[0].body;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.handle(mouse_event(kind, body.x + 1, body.y + 1))
                .unwrap();
        }
        assert!(!app.state_dirty, "an unchanged focus has nothing to save");

        // Clicking the other pane really moves focus, and that is saved.
        app.project.layout = LayoutMode::Vertical;
        app.project.secondary = Some(1);
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let other = app.panes[1].body;
        app.handle(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            other.x + 1,
            other.y + 1,
        ))
        .unwrap();
        assert!(app.project.focus_secondary);
        assert!(app.state_dirty);
    }

    #[test]
    fn unchanged_state_is_not_saved_again() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let mut store = Some(restore::Store::open_in(directory.path()).unwrap());
        app.state_dirty = true;
        assert!(!app.checkpoint(&mut store));
        assert_eq!(
            store.as_ref().unwrap().load().unwrap(),
            Some(app.snapshot())
        );

        // What another save left behind survives a checkpoint that finds no change.
        let other = restore::Snapshot {
            version: restore::VERSION,
            projects: Vec::new(),
            active_project: 0,
            native_keys: false,
            agent_cpu_limited: true,
            agent_cpu_percent: limits::DEFAULT_PERCENT,
        };
        store.as_ref().unwrap().save(&other).unwrap();
        app.state_dirty = true;
        assert!(!app.checkpoint(&mut store));
        assert_eq!(store.as_ref().unwrap().load().unwrap(), Some(other));

        app.native_keys = true;
        app.state_dirty = true;
        assert!(!app.checkpoint(&mut store));
        assert_eq!(
            store.as_ref().unwrap().load().unwrap(),
            Some(app.snapshot())
        );
    }

    #[test]
    fn pointer_motion_does_not_request_a_redraw() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[0].sessions.push(session);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let body = app.panes[0].body;
        // Over the pane, then over the tab bar.
        for (column, row) in [(body.x + 1, body.y + 1), (1, 0)] {
            assert!(
                !app.handle(mouse_event(MouseEventKind::Moved, column, row))
                    .unwrap()
            );
        }
        // A click is real input.
        assert!(
            app.handle(mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                body.x + 1,
                body.y + 1,
            ))
            .unwrap()
        );
        app.modal = Some(Modal::Confirm(Confirm::Quit));
        assert!(
            !app.handle(mouse_event(MouseEventKind::Moved, 1, 1))
                .unwrap()
        );
    }

    #[test]
    fn drag_and_release_follow_the_pane_that_took_the_press() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let outputs = [
            directory.path().join("left"),
            directory.path().join("right"),
        ];
        for (id, output) in (1..).zip(&outputs) {
            let session = recording_session(id, directory.path(), app.tx.clone(), output);
            app.project.workspaces[0].sessions.push(session);
        }
        app.project.layout = LayoutMode::Vertical;
        app.project.secondary = Some(0);
        app.project.secondary_session = Some(1);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        ui::resize_sessions(&mut app, ratatui::layout::Size::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let (left, right) = (app.panes[0].body, app.panes[1].body);
        assert!(right.x >= left.right());

        app.handle(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            left.x + 2,
            left.y + 2,
        ))
        .unwrap();
        assert!(app.mouse_capture.is_some());
        // The pointer crosses into the other pane: the drag and the release stay with the
        // pressed program, clamped to its right edge.
        let (x, y) = (right.x + 5, right.y + 4);
        app.handle(mouse_event(MouseEventKind::Drag(MouseButton::Left), x, y))
            .unwrap();
        app.handle(mouse_event(MouseEventKind::Up(MouseButton::Left), x, y))
            .unwrap();
        assert!(app.mouse_capture.is_none());
        let edge = left.width;
        let received = format!("\x1b[<0;3;3M\x1b[<32;{edge};5M\x1b[<0;{edge};5m");
        wait_for_file(&outputs[0], received.as_bytes());
        // Nothing reached the other pane: its program sees only what is sent to it later.
        app.project.workspaces[0].sessions[1].paste("Z").unwrap();
        wait_for_file(&outputs[1], b"Z");

        // Crit consumed no press this time, so a stray drag belongs to nothing.
        app.handle(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            left.x + 1,
            left.y + 1,
        ))
        .unwrap();
        app.project.workspaces[0].sessions[0].paste("Y").unwrap();
        wait_for_file(&outputs[0], format!("{received}Y").as_bytes());

        // A resize invalidates the remembered pane.
        app.handle(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            left.x + 1,
            left.y + 1,
        ))
        .unwrap();
        assert!(app.mouse_capture.is_some());
        app.handle(Event::Input(crossterm::event::Event::Resize(120, 24)))
            .unwrap();
        assert!(app.mouse_capture.is_none());

        // A release over a dialog hides the pane, but still ends the drag at the program that
        // took the press, so nothing stays captured for a later click.
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        app.handle(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            left.x + 1,
            left.y + 1,
        ))
        .unwrap();
        assert!(app.mouse_capture.is_some());
        app.modal = Some(Modal::Confirm(Confirm::Quit));
        app.handle(mouse_event(MouseEventKind::Up(MouseButton::Left), x, y))
            .unwrap();
        app.modal = None;
        assert!(app.mouse_capture.is_none());
        let received = format!("{received}Y\x1b[<0;2;2M\x1b[<0;2;2M\x1b[<0;{edge};5m");
        wait_for_file(&outputs[0], received.as_bytes());
        let tab = app
            .tab_hits
            .iter()
            .find(|hit| hit.workspace == Some(0))
            .unwrap()
            .area;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.handle(mouse_event(kind, tab.x, tab.y)).unwrap();
        }
        app.project.workspaces[0].sessions[0].paste("W").unwrap();
        wait_for_file(&outputs[0], format!("{received}W").as_bytes());
    }

    #[test]
    fn clipboard_text_reaches_the_focused_session_and_failures_become_notices() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let output = directory.path().join("pasted");
        let session = recording_session(1, directory.path(), app.tx.clone(), &output);
        app.project.workspaces[0].sessions.push(session);
        app.handle(Event::ClipboardText {
            target: Some(1),
            result: Ok("from the clipboard".into()),
        })
        .unwrap();
        wait_for_file(&output, b"from the clipboard");
        assert!(app.notice.is_none());

        // An open dialog takes the text instead, exactly as a native paste would.
        app.modal = Some(Modal::NewWorktree {
            input: String::new(),
            split: None,
        });
        app.handle(Event::ClipboardText {
            target: None,
            result: Ok("feature/login".into()),
        })
        .unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::NewWorktree { input, .. }) if input == "feature/login")
        );

        app.handle(Event::ClipboardText {
            target: None,
            result: Err("no clipboard tool".into()),
        })
        .unwrap();
        assert_eq!(app.notice.as_deref(), Some("no clipboard tool"));
    }

    #[test]
    fn failed_background_probe_is_silent() {
        let (mut app, _queued, _events) = interaction_app();
        let failure = |operation| Event::Environment {
            tree: "other".into(),
            operation,
            prepared: false,
            result: Err("docker missing".into()),
        };
        app.project.workspaces[1].pending = true;
        app.handle(failure(EnvironmentOp::Probe)).unwrap();
        assert!(app.notice.is_none());
        assert!(!app.project.workspaces[1].pending);

        // An operation the user asked for still reports its failure.
        app.project.workspaces[1].pending = true;
        app.handle(failure(EnvironmentOp::Status)).unwrap();
        assert_eq!(app.notice.as_deref(), Some("docker missing"));
        assert!(!app.project.workspaces[1].pending);
    }

    #[test]
    fn successful_background_probe_keeps_the_notice() {
        let (mut app, _queued, _events) = interaction_app();
        let environment = |tree: &str, operation| Event::Environment {
            tree: tree.into(),
            operation,
            prepared: false,
            result: Ok(compose::Environment::default()),
        };
        app.set_notice("Saving disabled for this window.");
        for tree in ["main", "other"] {
            app.handle(environment(tree, EnvironmentOp::Probe)).unwrap();
            assert_eq!(
                app.notice.as_deref(),
                Some("Saving disabled for this window.")
            );
        }
        // An explicit refresh still reports what it found.
        app.handle(environment("main", EnvironmentOp::Status))
            .unwrap();
        assert_eq!(app.notice.as_deref(), Some("main: stack stopped"));
    }

    #[test]
    fn environment_summary_distinguishes_starting_completed_and_failed_services() {
        let service = |name: &str, state: &str, health: &str, exit_code| compose::Service {
            name: name.into(),
            state: state.into(),
            health: health.into(),
            exit_code,
            ports: Vec::new(),
        };
        let environment = compose::Environment {
            project: "crit-main".into(),
            services: vec![
                service("db", "running", "starting", 0),
                service("migrate", "exited", "", 0),
                service("web", "running", "", 0),
                service("worker", "exited", "", 3),
            ],
        };
        assert_eq!(
            environment_summary("main", &environment),
            "main: ~ db · - migrate · + web · ! worker"
        );
    }

    #[test]
    fn late_clipboard_text_is_dropped_when_focus_moved_on() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let outputs = [
            directory.path().join("main"),
            directory.path().join("other"),
        ];
        for (workspace, (id, output)) in (1..).zip(&outputs).enumerate() {
            let session = recording_session(id, directory.path(), app.tx.clone(), output);
            app.project.workspaces[workspace].sessions.push(session);
        }
        let text = |target| Event::ClipboardText {
            target,
            result: Ok("make deploy".into()),
        };
        let dropped = "Clipboard paste dropped: focus changed before the clipboard answered.";

        // Asked in "main"; focus moved to "other" while the clipboard tool was answering.
        app.project.active = 1;
        app.handle(text(Some(1))).unwrap();
        assert_eq!(app.notice.as_deref(), Some(dropped));

        // Asked with no dialog open; a dialog has opened since.
        app.project.active = 0;
        app.notice = None;
        app.modal = Some(Modal::NewWorktree {
            input: String::new(),
            split: None,
        });
        app.handle(text(Some(1))).unwrap();
        assert!(matches!(&app.modal, Some(Modal::NewWorktree { input, .. }) if input.is_empty()));
        assert_eq!(app.notice.as_deref(), Some(dropped));

        // Asked from a dialog that has closed since: the terminal behind it must not get it.
        app.modal = None;
        app.notice = None;
        app.handle(text(None)).unwrap();
        assert_eq!(app.notice.as_deref(), Some(dropped));

        // Nothing reached either program: each sees only what is sent to it later.
        for (workspace, output) in outputs.iter().enumerate() {
            app.project.workspaces[workspace].sessions[0]
                .paste("!")
                .unwrap();
            wait_for_file(output, b"!");
        }
    }

    #[test]
    fn native_paste_reaches_the_terminal_behind_a_progress_dialog() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let output = directory.path().join("typed");
        let session = recording_session(1, directory.path(), app.tx.clone(), &output);
        app.project.workspaces[0].sessions.push(session);
        app.modal = Some(Modal::Removal {
            tree: "main".into(),
            confirm: Confirm::RemoveWorktree { tree: None },
            error: None,
        });
        let key = |code| {
            Event::Input(crossterm::event::Event::Key(KeyEvent::new(
                code,
                KeyModifiers::NONE,
            )))
        };
        let paste = |text: &str| Event::Input(crossterm::event::Event::Paste(text.into()));

        // Typed keys already reach the terminal in native mode; a paste must follow them.
        app.native_keys = true;
        app.handle(paste("pasted")).unwrap();
        app.handle(key(KeyCode::Char('k'))).unwrap();
        wait_for_file(&output, b"pastedk");

        // Crit's own keys leave the progress dialog alone, and so does a paste.
        app.native_keys = false;
        app.handle(paste("dropped")).unwrap();
        app.native_keys = true;
        app.handle(key(KeyCode::Char('z'))).unwrap();
        wait_for_file(&output, b"pastedkz");
    }

    #[test]
    fn native_mode_failures_report_without_dialogs() {
        let (mut app, queued, _events) = interaction_app();
        // A failed creation would bring back the branch dialog.
        app.modal = Some(Modal::NewWorktree {
            input: "taken".into(),
            split: None,
        });
        app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        queued.recv().unwrap();
        app.native_keys = true;
        app.handle(Event::Created {
            project: PathBuf::from("/interaction-fixture/.git"),
            source: PathBuf::from("/interaction-fixture/main"),
            preparation_error: None,
            result: Err("branch is already checked out".into()),
            agent: None,
            split: None,
        })
        .unwrap();
        assert!(app.modal.is_none());
        assert_eq!(app.notice.as_deref(), Some("branch is already checked out"));

        // A failed split would bring back the picker.
        app.notice = None;
        app.modal = Some(Modal::OpeningSplit { request: 5 });
        app.handle(Event::SplitOpened {
            request: 5,
            project: PathBuf::from("/interaction-fixture/.git"),
            layout: LayoutMode::Vertical,
            choices: Vec::new(),
            selected: 0,
            source: PathBuf::from("/interaction-fixture/main"),
            preparation_error: None,
            result: Err("branch no longer exists".into()),
        })
        .unwrap();
        assert!(app.modal.is_none());
        assert_eq!(app.notice.as_deref(), Some("branch no longer exists"));
    }

    #[test]
    fn creation_results_survive_a_full_job_queue() {
        let directory = tempfile::tempdir().unwrap();
        // A worktree with a Compose file wants its environment started.
        let created = |name: &str| {
            let path = directory.path().join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("compose.yaml"), "services: {}\n").unwrap();
            Worktree {
                id: name.into(),
                path,
                branch: name.into(),
                head: String::new(),
                is_main: false,
                locked: false,
                bare: false,
                prunable: false,
            }
        };
        let fill = |app: &App| {
            app.jobs
                .try_send(Job::Projects {
                    request: 0,
                    base: PathBuf::new(),
                })
                .unwrap();
        };
        let not_started = |app: &App| {
            assert!(
                app.notice
                    .as_deref()
                    .is_some_and(|notice| notice.contains("not started"))
            );
        };
        let common_dir = PathBuf::from("/interaction-fixture/.git");
        let source = PathBuf::from("/interaction-fixture/main");

        // A saved project still gets its new tab.
        let (mut app, _queued, _events) = interaction_app();
        let mut saved = Project::terminal(directory.path()).unwrap();
        saved.repository = Some(worktree::Repository {
            root: PathBuf::from("/saved-fixture"),
            common_dir: PathBuf::from("/saved-fixture/.git"),
        });
        saved.order = 1;
        app.saved_projects.push(saved);
        fill(&app);
        assert!(
            app.handle(Event::Created {
                project: PathBuf::from("/saved-fixture/.git"),
                source: source.clone(),
                preparation_error: None,
                result: Ok(created("saved-new")),
                agent: None,
                split: None,
            })
            .unwrap()
        );
        assert_eq!(app.saved_projects[0].workspaces.len(), 2);
        assert!(!app.saved_projects[0].workspaces[1].pending);
        not_started(&app);

        // The current project still selects and splits it.
        let (mut app, _queued, _events) = interaction_app();
        fill(&app);
        app.handle(Event::Created {
            project: common_dir.clone(),
            source: source.clone(),
            preparation_error: None,
            result: Ok(created("created-here")),
            agent: None,
            split: Some(LayoutMode::Vertical),
        })
        .unwrap();
        assert_eq!(app.project.workspaces.len(), 3);
        assert_eq!(app.focused().tree.id, "created-here");
        assert!(app.project.layout == LayoutMode::Vertical);
        assert!(!app.project.workspaces[2].pending);
        not_started(&app);

        // An opened split checkout is still shown.
        let (mut app, _queued, _events) = interaction_app();
        app.modal = Some(Modal::OpeningSplit { request: 3 });
        fill(&app);
        app.handle(Event::SplitOpened {
            request: 3,
            project: common_dir,
            layout: LayoutMode::Horizontal,
            choices: Vec::new(),
            selected: 0,
            source,
            preparation_error: None,
            result: Ok((created("opened"), true)),
        })
        .unwrap();
        assert!(app.modal.is_none());
        assert_eq!(app.project.secondary, Some(2));
        assert!(app.project.layout == LayoutMode::Horizontal);
        assert!(!app.project.workspaces[2].pending);
        not_started(&app);
    }

    #[test]
    fn failed_switch_closes_the_picker() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        for (name, order) in [("alpha", 1), ("gamma", 2)] {
            let mut project = Project::terminal(directory.path()).unwrap();
            project.name = name.into();
            project.order = order;
            app.saved_projects.push(project);
        }
        // Nothing can start in the project switched to.
        app.default_agent = "bogus".into();
        app.modal = Some(Modal::SwitchProject { selected: 1 });
        assert!(
            app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_err()
        );
        // The projects are already reordered; a picker restored now would highlight the wrong one.
        assert_eq!(app.project.name, "gamma");
        assert!(app.modal.is_none());
        let saved: Vec<_> = app.saved_projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(saved, ["fixture", "alpha"]);
    }

    #[test]
    fn unqueued_intent_creation_leaves_no_creating_notice() {
        let (mut app, _queued, _events) = interaction_app();
        // The one queue slot still holds the dialog's own destination scan.
        app.open_intent().unwrap();
        let Some(Modal::Intent(dialog)) = &mut app.modal else {
            panic!("expected interpreter dialog");
        };
        dialog.loading = false;
        dialog.pending = true;
        let request = dialog.request;
        app.handle(Event::IntentResolved {
            request,
            result: Ok(Plan::CreateWorktree {
                branch: "login-fix".into(),
                base: None,
                agent: None,
            }),
        })
        .unwrap();
        assert!(matches!(
            &app.modal,
            Some(Modal::Intent(dialog))
                if dialog.error.as_deref().is_some_and(|error| error.starts_with("command queue is busy"))
        ));
        assert!(app.notice.is_none());
        assert!(app.pending_creations.is_empty());
    }

    #[test]
    fn new_tab_menu_has_no_project_switch_entry() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let mut other = Project::terminal(directory.path()).unwrap();
        other.order = 1;
        app.saved_projects.push(other);
        let choices = app.new_tab_choices();
        assert!(!choices.iter().any(|(_, action)| matches!(
            action,
            NewTabAction::Command(IntentCommand::SwitchProject)
        )));
        // Home, Open folder, and the two worktree entries of the Git project remain.
        assert_eq!(choices.len(), 4);
    }

    #[test]
    fn home_opens_a_new_tab_each_time() {
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (mut app, queued, _events) = interaction_app();
        app.project = Project::terminal(home.path()).unwrap();
        // Delivers the job the app queued, the way the worker would answer it.
        let deliver = |app: &mut App| {
            let Job::OpenProject {
                request, fresh_tab, ..
            } = queued.recv().unwrap()
            else {
                panic!("expected project opening");
            };
            app.handle(Event::ProjectOpened {
                request,
                agent: None,
                fresh_tab,
                result: Ok(Project::terminal(home.path()).unwrap()),
            })
            .unwrap();
            fresh_tab
        };
        for tabs in [2, 3] {
            app.action(Action::NewTab).unwrap();
            app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap();
            assert!(deliver(&mut app), "Home asks for a fresh tab");
            assert_eq!(app.project.workspaces.len(), tabs);
            assert_eq!(app.focused_index(), tabs - 1);
            assert_eq!(app.focused().tree.path, home.path().canonicalize().unwrap());
        }

        // Reopening the folder any other way still selects the tab it already has.
        app.open_path(home.path().to_owned(), None, false).unwrap();
        assert!(!deliver(&mut app));
        assert_eq!(app.project.workspaces.len(), 3);
        assert_eq!(app.focused_index(), 0);

        // Tabs for the same folder survive a restart.
        let store = restore::Store::open_in(state.path()).unwrap();
        let snapshot = app.snapshot();
        assert_eq!(snapshot.projects[0].workspaces.len(), 3);
        store.save(&snapshot).unwrap();
        assert_eq!(store.load().unwrap(), Some(snapshot));
    }

    #[test]
    fn git_home_gets_a_fresh_session_in_its_existing_tab() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[1].sessions.push(session);
        app.next_session = 2;
        let reopened = Project {
            repository: app.project.repository.clone(),
            name: "fixture".into(),
            workspaces: vec![Workspace::new(app.project.workspaces[1].tree.clone())],
            trees: Vec::new(),
            active: 0,
            secondary: None,
            secondary_session: None,
            focus_secondary: false,
            layout: LayoutMode::Full,
            split_ratio: restore::default_split_ratio(),
            order: 0,
        };
        app.open_project(reopened, None, true).unwrap();
        // A checkout keeps one tab, whose id metadata and removal look up; it gains a session.
        assert_eq!(app.project.workspaces.len(), 2);
        assert_eq!(app.focused().tree.id, "other");
        assert_eq!(app.focused().sessions.len(), 2);
        assert_eq!(app.focused_session().unwrap().id, 2);
    }

    #[test]
    fn non_utf8_destinations_still_load_in_ask_crit() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let (mut app, queued, _events) = interaction_app();
        app.open_intent().unwrap();
        let Job::IntentTargets { request, .. } = queued.recv().unwrap() else {
            panic!("expected the destination scan");
        };
        let exact = std::ffi::OsString::from_vec(b"/x/caf\xe9".to_vec());
        app.handle(Event::IntentTargets {
            request,
            result: Ok(vec![IntentTarget {
                id: 0,
                kind: "project",
                name: "caf".into(),
                path: PathBuf::from(&exact),
                branch: None,
            }]),
        })
        .unwrap();
        let Some(Modal::Intent(dialog)) = &app.modal else {
            panic!("expected interpreter dialog");
        };
        assert!(!dialog.stale);
        assert!(dialog.error.is_none());
        assert!(dialog.context["targets"][0]["path"].is_string());
        // Navigation still gets the exact bytes.
        assert_eq!(
            dialog.targets[0].path.as_os_str().as_bytes(),
            exact.as_bytes()
        );
    }

    #[test]
    fn stale_project_scan_keeps_the_picker_selection() {
        let (mut app, queued, _events) = interaction_app();
        app.run_command(IntentCommand::OpenProject).unwrap();
        let Job::Projects { request, .. } = queued.recv().unwrap() else {
            panic!("expected a project scan");
        };
        let scan = |request| Event::Projects {
            request,
            projects: ["alpha", "beta", "gamma"]
                .iter()
                .map(|name| PathBuf::from("/projects").join(name))
                .collect(),
        };
        let selected = |app: &App| match &app.modal {
            Some(Modal::OpenProject(picker)) => picker.selected,
            _ => panic!("expected project picker"),
        };
        app.handle(scan(request)).unwrap();
        for _ in 0..2 {
            app.modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(selected(&app), 2);
        // The scan of a picker that was closed meanwhile must not reset this one's highlight.
        app.handle(scan(request + 1)).unwrap();
        assert_eq!(selected(&app), 2);
    }

    #[test]
    fn intent_targets_trim_discovered_projects_to_budget() {
        let tree = |branch: &str| Worktree {
            id: branch.into(),
            path: PathBuf::from("/work/app").join(branch),
            branch: branch.into(),
            head: String::new(),
            is_main: false,
            locked: false,
            bare: false,
            prunable: false,
        };
        let open = vec![
            PathBuf::from("/work/open/app"),
            PathBuf::from("/work/open/api"),
        ];
        // A scan of a huge organization, which also finds the projects that are open.
        let discovered: Vec<_> = (0..4_000)
            .map(|n| PathBuf::from(format!("/work/org/project-{n:04}")))
            .chain(open.iter().cloned())
            .collect();
        let targets = ranked_intent_targets(
            vec![tree("main"), tree("fix-login")],
            &open,
            discovered.clone(),
        );

        let order: Vec<_> = targets
            .iter()
            .take(5)
            .map(|target| (target.kind, target.path.to_str().unwrap()))
            .collect();
        assert_eq!(
            order,
            [
                ("worktree", "/work/app/main"),
                ("worktree", "/work/app/fix-login"),
                ("project", "/work/open/api"),
                ("project", "/work/open/app"),
                ("project", "/work/org/project-0000"),
            ]
        );
        assert!(
            targets
                .iter()
                .enumerate()
                .all(|(index, target)| target.id == index)
        );
        // Open projects are listed once, and the discovered tail is cut at the budget.
        let listed = |path: &str| targets.iter().filter(|t| t.path == Path::new(path)).count();
        assert_eq!(listed("/work/open/app"), 1);
        assert_eq!(listed("/work/org/project-3999"), 0);
        let size = serde_json::to_string(&targets).unwrap().len();
        assert!(
            (INTENT_TARGET_BUDGET - 4 * 1024..56 * 1024).contains(&size),
            "{size} bytes"
        );

        // A small scan is kept whole.
        let small = ranked_intent_targets(Vec::new(), &open, discovered[..3].to_vec());
        assert_eq!(small.len(), 5);
    }

    #[test]
    fn closing_focused_primary_collapses_to_visible_peer() {
        // The visible peer stays, whichever tabs surround it.
        for order in [["b", "a", "c"], ["c", "a", "b"], ["a", "b", "c"]] {
            let (mut app, _queued, _events) = interaction_app();
            app.project.workspaces = order
                .into_iter()
                .map(|branch| {
                    Workspace::new(Worktree {
                        id: branch.into(),
                        path: PathBuf::from("/interaction-fixture").join(branch),
                        branch: branch.into(),
                        head: String::new(),
                        is_main: false,
                        locked: false,
                        bare: false,
                        prunable: false,
                    })
                })
                .collect();
            let position = |label: &str| order.iter().position(|branch| *branch == label).unwrap();
            app.project.active = position("a");
            app.project.secondary = Some(position("b"));
            app.project.layout = LayoutMode::Vertical;
            app.close_workspace();
            assert!(app.project.layout == LayoutMode::Full, "{order:?}");
            assert!(app.project.secondary.is_none(), "{order:?}");
            assert_eq!(app.project.workspaces.len(), 2, "{order:?}");
            assert_eq!(app.focused().label(), "b", "{order:?}");
        }
    }

    #[test]
    fn paste_input_separates_pasted_lines_with_single_spaces() {
        let mut input = String::new();
        paste_input(
            &mut input,
            "\r\nfix login\r\n\r\nthen\tdeploy\x1b[2J\n",
            4096,
        );
        assert_eq!(input, "fix login then deploy[2J ");

        // The byte limit still applies to the separator.
        let mut input = String::new();
        paste_input(&mut input, "ab", 3);
        paste_input(&mut input, "\ncd", 3);
        assert_eq!(input, "ab ");
    }

    #[test]
    fn moved_worktree_is_reassociated_by_branch() {
        let (mut app, queued, _events) = interaction_app();
        let directory = tempfile::tempdir().unwrap();
        let session = sleeping_session(&app, directory.path(), 1);
        app.project.workspaces[1].sessions.push(session);
        let metadata = |trees: Vec<Worktree>| {
            Event::Metadata(vec![(PathBuf::from("/interaction-fixture/.git"), trees)])
        };
        let main = app.project.workspaces[0].tree.clone();
        // `git worktree move` keeps the branch but changes the path, and with it the id.
        let mut moved = app.project.workspaces[1].tree.clone();
        moved.id = "other-moved".into();
        moved.path = PathBuf::from("/interaction-fixture/other-moved");

        // Two unopened checkouts on the branch are ambiguous, so nothing is guessed.
        let mut twin = moved.clone();
        twin.id = "other-twin".into();
        assert!(
            !app.handle(metadata(vec![main.clone(), moved.clone(), twin]))
                .unwrap()
        );
        assert_eq!(app.project.workspaces[1].tree.id, "other");
        // An environment operation still addressed to the old id finishes first.
        app.project.workspaces[1].pending = true;
        assert!(
            !app.handle(metadata(vec![main.clone(), moved.clone()]))
                .unwrap()
        );
        app.project.workspaces[1].pending = false;

        assert!(
            app.handle(metadata(vec![main.clone(), moved.clone()]))
                .unwrap()
        );
        assert_eq!(app.project.workspaces.len(), 2);
        assert_eq!(app.project.workspaces[1].tree.id, "other-moved");
        assert_eq!(app.project.workspaces[1].tree.path, moved.path);
        assert_eq!(app.snapshot().projects[0].workspaces[1].path, moved.path);
        assert!(app.state_dirty);

        // A later rename in the new location updates the label like any other checkout.
        moved.branch = "renamed".into();
        assert!(
            app.handle(metadata(vec![main.clone(), moved.clone()]))
                .unwrap()
        );
        assert_eq!(app.project.workspaces[1].label(), "renamed");

        // Reopening the moved checkout selects its tab instead of adding another.
        app.run_command(IntentCommand::OpenWorktree).unwrap();
        let Job::List {
            request,
            repository,
        } = queued.recv().unwrap()
        else {
            panic!("expected worktree listing");
        };
        app.handle(Event::Worktrees {
            request,
            project: repository.common_dir,
            result: Ok(vec![main, moved]),
        })
        .unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.project.workspaces.len(), 2);
        assert_eq!(app.focused().tree.id, "other-moved");
    }

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Input(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
        ))
    }

    /// A live pane running `/bin/sleep`.
    fn sleeping_session(app: &App, directory: &Path, id: u64) -> Session {
        Session::spawn(
            id,
            "shell",
            None,
            directory,
            &app.socket,
            app.tx.clone(),
            10,
            40,
            Some(("/bin/sleep".into(), vec!["30".into()])),
            None,
        )
        .unwrap()
    }

    /// A pane whose program appends every byte it receives to `output`, with mouse
    /// reporting on. Returns once the program has put its terminal in raw mode.
    fn recording_session(
        id: u64,
        directory: &Path,
        tx: SyncSender<Event>,
        output: &Path,
    ) -> Session {
        let session = Session::spawn(
            id,
            "shell",
            None,
            directory,
            &directory.join("unused.sock"),
            tx,
            10,
            40,
            Some((
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "stty raw -echo; printf ready; exec cat > \"$0\"".into(),
                    output.to_string_lossy().into_owned(),
                ],
            )),
            None,
        )
        .unwrap();
        // Enabled here rather than by the program, whose own output would race the test.
        session.parser.lock().process(b"\x1b[?1002h\x1b[?1006h");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !session.parser.lock().screen().contents().contains("ready") {
            assert!(
                Instant::now() < deadline,
                "the recording program never started"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        session
    }

    /// Waits until `path` holds `expected.len()` bytes, then requires them to match.
    fn wait_for_file(path: &Path, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let contents = std::fs::read(path).unwrap_or_default();
            if contents.len() >= expected.len() {
                assert_eq!(
                    String::from_utf8_lossy(&contents),
                    String::from_utf8_lossy(expected)
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{} holds {:?}, expected {:?}",
                path.display(),
                String::from_utf8_lossy(&contents),
                String::from_utf8_lossy(expected)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn interaction_app() -> (App, mpsc::Receiver<Job>, mpsc::Receiver<Event>) {
        let (tx, events) = mpsc::sync_channel(8);
        let (jobs, queued) = mpsc::sync_channel(1);
        let root = PathBuf::from("/interaction-fixture");
        let app = App {
            split_divider: None,
            split_drag: None,
            project: Project {
                secondary_session: None,
                trees: Vec::new(),
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
                            bare: false,
                            prunable: false,
                        })
                    })
                    .collect(),
                active: 0,
                secondary: None,
                focus_secondary: false,
                layout: LayoutMode::Full,
                order: 0,
                split_ratio: restore::default_split_ratio(),
            },
            saved_projects: Vec::new(),
            native_keys: false,
            limiter: limits::Limiter::off(),
            modal: None,
            notice: None,
            notice_sticky: false,
            panes: Vec::new(),
            next_panes: Vec::new(),
            scroll_drag: None,
            mouse_capture: None,
            selection: None,
            tab_hits: Vec::new(),
            worktree_pane: None,
            worktree_hits: Vec::new(),
            worktree_area: None,
            interaction_visible: true,
            should_quit: false,
            default_agent: "shell".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
            last_saved: None,
            metadata_pending: false,
            metadata_checked: Instant::now() - App::METADATA_INTERVAL,
            socket: root.join("agent.sock"),
            tx,
            jobs,
            use_kitty: false,
        };
        (app, queued, events)
    }

    #[test]
    fn existing_split_discovery_cannot_reopen_a_canceled_picker() {
        let (mut app, queued, _events) = interaction_app();
        app.modal = Some(Modal::Split {
            layout: LayoutMode::Vertical,
            selected: 2,
        });
        app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let Job::SplitTargets {
            request,
            repository,
            layout,
            ..
        } = queued.recv().unwrap()
        else {
            panic!("expected discovery");
        };
        app.modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        app.modal = Some(Modal::Confirm(Confirm::Quit));
        app.handle(Event::SplitTargets {
            request,
            project: repository.common_dir,
            layout,
            result: Ok(vec![worktree::SplitTarget::Branch("unused".into())]),
        })
        .unwrap();
        assert!(matches!(app.modal, Some(Modal::Confirm(Confirm::Quit))));
        assert_eq!(app.project.workspaces.len(), 2);
        assert!(queued.try_recv().is_err());
    }

    #[test]
    fn existing_split_keeps_choices_on_queue_failure_and_refusal() {
        let (mut app, queued, _events) = interaction_app();
        app.modal = Some(Modal::ExistingSplit {
            layout: LayoutMode::Vertical,
            choices: vec![worktree::SplitTarget::Branch("available".into())],
            selected: 0,
            error: None,
        });
        app.jobs
            .try_send(Job::Projects {
                request: 0,
                base: PathBuf::new(),
            })
            .unwrap();
        assert!(
            app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_err()
        );
        let Some(Modal::ExistingSplit { choices, .. }) = &app.modal else {
            panic!("picker lost");
        };
        assert!(
            matches!(choices.as_slice(), [worktree::SplitTarget::Branch(branch)] if branch == "available")
        );
        queued.recv().unwrap();
        app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let Job::OpenSplit {
            request,
            repository,
            layout,
            choices,
            selected,
        } = queued.recv().unwrap()
        else {
            panic!("expected opening");
        };
        // Other keys cannot answer the progress dialog, and neither can native key routing.
        app.modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        app.action(Action::ToggleNative).unwrap();
        assert!(
            matches!(app.modal, Some(Modal::OpeningSplit { request: pending }) if pending == request)
        );
        // The picker comes back only while the keyboard is Crit's.
        app.action(Action::ToggleNative).unwrap();
        app.handle(Event::SplitOpened {
            request,
            project: repository.common_dir,
            source: repository.root,
            layout,
            choices,
            selected,
            preparation_error: None,
            result: Err("branch no longer exists".into()),
        })
        .unwrap();
        app.handle(Event::Environment {
            tree: "main".into(),
            operation: EnvironmentOp::Probe,
            prepared: false,
            result: Ok(compose::Environment {
                project: "main".into(),
                services: Vec::new(),
            }),
        })
        .unwrap();
        assert!(
            matches!(&app.modal, Some(Modal::ExistingSplit { error: Some(error), .. }) if error == "branch no longer exists")
        );
        app.modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(app.modal, Some(Modal::Split { selected: 2, .. })));
        assert_eq!(app.project.workspaces.len(), 2);
        assert!(app.project.secondary.is_none());
    }

    #[test]
    fn existing_split_reuses_sessions_and_keeps_the_focused_terminal_first() {
        let (mut app, _queued, _events) = interaction_app();
        let directory = tempfile::tempdir().unwrap();
        for (workspace, id) in [(0, 1), (0, 2), (1, 3)] {
            app.project.workspaces[workspace].sessions.push(
                Session::spawn(
                    id,
                    "shell",
                    None,
                    directory.path(),
                    &app.socket,
                    app.tx.clone(),
                    10,
                    40,
                    Some(("/bin/sleep".into(), vec!["30".into()])),
                    None,
                )
                .unwrap(),
            );
        }
        app.project.secondary = Some(0);
        app.project.secondary_session = Some(1);
        app.project.focus_secondary = true;
        app.project.layout = LayoutMode::Vertical;
        let tree = app.project.workspaces[1].tree.clone();
        app.modal = Some(Modal::OpeningSplit { request: 7 });
        app.handle(Event::SplitOpened {
            request: 7,
            project: app.project.common_dir().unwrap().to_owned(),
            layout: LayoutMode::Horizontal,
            choices: Vec::new(),
            selected: 0,
            source: app.project.path().to_owned(),
            preparation_error: None,
            result: Ok((tree, false)),
        })
        .unwrap();
        assert_eq!(app.focused_session().unwrap().id, 2);
        assert_eq!(app.project.workspaces.len(), 2);
        assert_eq!(app.project.workspaces[0].sessions.len(), 2);
        assert_eq!(app.project.workspaces[1].sessions.len(), 1);
        assert_eq!(app.project.workspaces[1].sessions[0].id, 3);
        assert_eq!(app.project.active, 0);
        assert_eq!(app.project.secondary, Some(1));
        assert!(app.project.secondary_session.is_none());
        assert!(!app.project.focus_secondary);
        assert!(app.project.layout == LayoutMode::Horizontal);
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
                fresh_tab: false,
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
            fresh_tab: false,
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
            fresh_tab: false,
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
        app.jobs
            .try_send(Job::Projects {
                request: 0,
                base: PathBuf::new(),
            })
            .unwrap();
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
        app.jobs
            .try_send(Job::Projects {
                request: 0,
                base: PathBuf::new(),
            })
            .unwrap();
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
        app.modal = Some(Modal::Confirm(Confirm::CloseWorktree { selected: 2 }));
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
            Some(Modal::Confirm(Confirm::CloseWorktree { .. }))
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
        app.modal = Some(Modal::Confirm(Confirm::CloseWorktree { selected: 2 }));
        app.handle(Event::Removed {
            tree: "other".into(),
            result: Ok(None),
        })
        .unwrap();
        assert_eq!(app.focused().tree.id, "third");
        assert!(app.project.layout == LayoutMode::Vertical);
        assert_eq!(app.project.secondary, Some(1));
        assert!(matches!(
            app.modal,
            Some(Modal::Confirm(Confirm::CloseWorktree { .. }))
        ));
    }

    #[test]
    fn removal_reports_unchecked_docker_state() {
        let (mut app, _queued, _events) = interaction_app();
        app.handle(Event::Removed {
            tree: "other".into(),
            result: Ok(Some("daemon down".into())),
        })
        .unwrap();
        assert_eq!(app.project.workspaces.len(), 1);
        let notice = app.notice.as_deref().unwrap();
        assert!(notice.contains("could not be checked"), "{notice}");
        assert!(notice.contains("daemon down"), "{notice}");
        assert!(
            notice.contains("docker compose -p crit-other down"),
            "{notice}"
        );
    }

    #[test]
    fn removed_preparation_source_is_forgotten() {
        let directory = tempfile::tempdir().unwrap();
        let (app, _queued, _events) = interaction_app();
        let (tx, events) = mpsc::sync_channel(8);
        let jobs = start_worker(tx).unwrap();
        // A checkout with no Compose file, so Up fails before Docker is involved.
        let mut tree = app.project.workspaces[0].tree.clone();
        tree.path = directory.path().to_path_buf();
        jobs.send(Job::Environment {
            tree,
            operation: EnvironmentOp::Up,
            source: Some(directory.path().join("removed-source")),
        })
        .unwrap();
        let Event::Environment {
            prepared, result, ..
        } = events.recv_timeout(Duration::from_secs(10)).unwrap()
        else {
            panic!("expected the environment result");
        };
        assert!(prepared, "a missing source must not stop the start");
        assert!(result.unwrap_err().contains("no compose.yaml"));
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
    fn bare_repository_projects_are_named_after_their_folder() {
        for (main_tree, common_dir, name) in [
            ("/x/myapp/.bare", "/x/myapp/.bare", "myapp"),
            ("/x/app.git", "/x/app.git", "app"),
            ("/x/repo", "/x/repo/.git", "repo"),
        ] {
            assert_eq!(
                project_name(Path::new(main_tree), Path::new(common_dir)),
                name
            );
        }
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
            split_divider: None,
            split_drag: None,
            project: Project::terminal(parent.path()).unwrap(),
            saved_projects: Vec::new(),
            native_keys: false,
            limiter: limits::Limiter::off(),
            modal: None,
            notice: None,
            notice_sticky: false,
            panes: Vec::new(),
            next_panes: Vec::new(),
            scroll_drag: None,
            mouse_capture: None,
            selection: None,
            tab_hits: Vec::new(),
            worktree_pane: None,
            worktree_hits: Vec::new(),
            worktree_area: None,
            interaction_visible: true,
            should_quit: false,
            default_agent: "shell".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
            last_saved: None,
            metadata_pending: false,
            metadata_checked: Instant::now() - App::METADATA_INTERVAL,
            socket: parent.path().join("agent.sock"),
            tx,
            jobs,
            use_kitty: false,
        };
        app.open_project(project, None, false).unwrap();
        assert_eq!(app.project.name, "jyq");
        assert_eq!(app.saved_projects.len(), 1);
        app.open_project(Project::open(&home).unwrap(), None, false)
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
            split_divider: None,
            split_drag: None,
            project: Project::terminal(directory.path()).unwrap(),
            saved_projects: Vec::new(),
            native_keys: false,
            limiter: limits::Limiter::off(),
            modal: None,
            notice: None,
            notice_sticky: false,
            panes: Vec::new(),
            next_panes: Vec::new(),
            scroll_drag: None,
            mouse_capture: None,
            selection: None,
            tab_hits: Vec::new(),
            worktree_pane: None,
            worktree_hits: Vec::new(),
            worktree_area: None,
            interaction_visible: true,
            should_quit: false,
            default_agent: "omp".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
            last_saved: None,
            metadata_pending: false,
            metadata_checked: Instant::now() - App::METADATA_INTERVAL,
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
        assert!(matches!(queued.recv().unwrap(), Job::Projects { .. }));
    }

    #[test]
    fn tab_bar_lists_every_open_project_in_opening_order_and_tabs_wrap_across_them() {
        let (tx, _events) = mpsc::sync_channel(8);
        let (jobs, queued) = mpsc::sync_channel(8);
        let project = |name: &str, order: u64, branches: &[&str]| Project {
            secondary_session: None,
            trees: Vec::new(),
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
                        bare: false,
                        prunable: false,
                    })
                })
                .collect(),
            active: 0,
            secondary: None,
            focus_secondary: false,
            layout: LayoutMode::Full,
            order,
            split_ratio: restore::default_split_ratio(),
        };
        let mut app = App {
            split_divider: None,
            split_drag: None,
            project: project("alpha", 0, &["main", "dev"]),
            saved_projects: vec![project("beta", 1, &["main"])],
            native_keys: false,
            limiter: limits::Limiter::off(),
            modal: None,
            notice: None,
            notice_sticky: false,
            panes: Vec::new(),
            next_panes: Vec::new(),
            scroll_drag: None,
            mouse_capture: None,
            selection: None,
            tab_hits: Vec::new(),
            worktree_pane: None,
            worktree_hits: Vec::new(),
            worktree_area: None,
            interaction_visible: true,
            should_quit: false,
            default_agent: "shell".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
            last_saved: None,
            metadata_pending: false,
            metadata_checked: Instant::now() - App::METADATA_INTERVAL,
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

        // The bar renders both projects; clicking another project's tab switches to it. Each
        // worktree tab is 24 columns now, so the five cells need a wide window.
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 24)).unwrap();
        let mut tick = None;
        terminal
            .draw(|frame| {
                tick = ui::draw(frame, &mut app);
            })
            .unwrap();
        let row: String = terminal.backend().buffer().content[..160]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        for expected in ["alpha", "1 main", "2 dev", "beta"] {
            assert!(row.contains(expected), "{expected:?} missing from {row:?}");
        }
        // Every worktree tab is the same width (number, an 18-column label slot, mark) and
        // shows the ready mark; nothing on the bar moves, so no frame is asked for.
        let widths: Vec<u16> = app
            .tab_hits
            .iter()
            .filter(|hit| hit.workspace.is_some())
            .map(|hit| hit.area.width)
            .collect();
        assert_eq!(widths, [24, 24, 24]);
        assert_eq!(row.matches('●').count(), 3, "{row:?}");
        assert_eq!(tick, None);
        // A long label scrolls and a working tab spins, in a background project too: both
        // ask for a next frame. beta is current here; alpha is `saved_projects[0]`.
        app.saved_projects[0].workspaces[1].tree.branch = "fix-juror-ranker-silent-fallback".into();
        terminal
            .draw(|frame| {
                tick = ui::draw(frame, &mut app);
            })
            .unwrap();
        assert!(
            tick.is_some_and(|tick| tick <= Duration::from_millis(2500)),
            "{tick:?}"
        );
        app.saved_projects[0].workspaces[1].tree.branch = "dev".into();
        let directory = tempfile::tempdir().unwrap();
        let session = sleeping_session(&app, directory.path(), 1);
        app.saved_projects[0].workspaces[0].sessions.push(session);
        app.saved_projects[0].workspaces[0].sessions[0].state = AgentState::Working;
        terminal
            .draw(|frame| {
                tick = ui::draw(frame, &mut app);
            })
            .unwrap();
        assert!(
            tick.is_some_and(|tick| tick <= Duration::from_millis(125)),
            "{tick:?}"
        );
        app.saved_projects[0].workspaces[0].sessions.clear();
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
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
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
            split_divider: None,
            split_drag: None,
            project: Project {
                secondary_session: None,
                trees: Vec::new(),
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
                            bare: false,
                            prunable: false,
                        })
                    })
                    .collect(),
                active: 0,
                secondary: None,
                focus_secondary: false,
                layout: LayoutMode::Full,
                order: 0,
                split_ratio: restore::default_split_ratio(),
            },
            saved_projects: Vec::new(),
            native_keys: false,
            limiter: limits::Limiter::off(),
            modal: None,
            notice: None,
            notice_sticky: false,
            panes: Vec::new(),
            next_panes: Vec::new(),
            scroll_drag: None,
            mouse_capture: None,
            selection: None,
            tab_hits: Vec::new(),
            worktree_pane: None,
            worktree_hits: Vec::new(),
            worktree_area: None,
            interaction_visible: true,
            should_quit: false,
            default_agent: "shell".into(),
            next_session: 1,
            next_request: 1,
            pending_creations: VecDeque::new(),
            state_dirty: false,
            last_saved: None,
            metadata_pending: false,
            metadata_checked: Instant::now() - App::METADATA_INTERVAL,
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

    #[test]
    fn intent_error_overflow_keeps_final_cause() {
        let (mut app, queued, _events) = interaction_app();
        app.open_intent().unwrap();
        queued.recv().unwrap();
        let screen = |app: &mut App, height: u16, error: &str| {
            let Some(Modal::Intent(dialog)) = &mut app.modal else {
                panic!("expected interpreter dialog");
            };
            dialog.loading = false;
            dialog.error = Some(error.into());
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(80, height)).unwrap();
            terminal
                .draw(|frame| {
                    ui::draw(frame, app);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..height)
                .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
        };
        let shown = |rows: &[String], text: &str| rows.iter().any(|row| row.contains(text));
        let controls = "Enter send · Esc cancel · Ctrl+U clear · F2 manual controls";
        let long = format!(
            "OMP intent interpreter failed (1): {}FINAL-CAUSE",
            "stderr ".repeat(130)
        );
        let short = "OMP intent interpreter failed (1): model unavailable";
        // 18 rows leave a one-row status area; 24 rows leave seven.
        for height in [18, 24] {
            let rows = screen(&mut app, height, &long);
            assert!(shown(&rows, "FINAL-CAUSE"), "80x{height}: {rows:#?}");
            assert!(shown(&rows, "↑"), "80x{height}: {rows:#?}");
            assert!(shown(&rows, controls), "80x{height}: {rows:#?}");

            let rows = screen(&mut app, height, short);
            assert!(shown(&rows, short), "80x{height}: {rows:#?}");
            assert!(!shown(&rows, "↑"), "80x{height}: {rows:#?}");
            assert!(shown(&rows, controls), "80x{height}: {rows:#?}");
        }
    }

    #[test]
    fn narrowest_tab_bar_keeps_the_focused_tab_clickable() {
        let (mut app, _queued, _events) = interaction_app();
        app.project.active = 1;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(30, 10)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let hit = app
            .tab_hits
            .iter()
            .find(|hit| hit.project.is_none() && hit.workspace == Some(app.focused_index()))
            .expect("the focused workspace has no click target in the 16-column tab bar");
        // The compact key-mode label takes the last 14 of 30 columns.
        assert!(hit.area.width > 0 && hit.area.right() <= 16);
    }

    /// `interaction_app()` listing four checkouts: its two tabs, an unopened `feature/login`
    /// whose folder exists, and a locked `frozen`.
    fn worktree_pane_app(directory: &Path) -> (App, mpsc::Receiver<Job>, mpsc::Receiver<Event>) {
        let (mut app, queued, events) = interaction_app();
        let mut trees: Vec<Worktree> = app
            .project
            .workspaces
            .iter()
            .map(|workspace| workspace.tree.clone())
            .collect();
        std::fs::create_dir(directory.join("feature")).unwrap();
        for (branch, path, locked) in [
            ("feature/login", directory.join("feature"), false),
            ("frozen", PathBuf::from("/interaction-fixture/frozen"), true),
        ] {
            trees.push(Worktree {
                id: branch.into(),
                path,
                branch: branch.into(),
                head: String::new(),
                is_main: false,
                locked,
                bare: false,
                prunable: false,
            });
        }
        app.project.trees = trees;
        (app, queued, events)
    }

    fn key_event(code: KeyCode) -> Event {
        Event::Input(crossterm::event::Event::Key(KeyEvent::new(
            code,
            KeyModifiers::NONE,
        )))
    }

    /// One frame the way the main loop makes it: sessions sized first, then the screen.
    fn draw_frame(app: &mut App, terminal: &mut Terminal<ratatui::backend::TestBackend>) {
        ui::resize_sessions(app, terminal.size().unwrap()).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, app);
            })
            .unwrap();
    }

    fn screen_row(terminal: &Terminal<ratatui::backend::TestBackend>, row: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    fn screen_rows(terminal: &Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
        (0..terminal.backend().buffer().area.height)
            .map(|row| screen_row(terminal, row))
            .collect()
    }

    /// What the worktree pane of a 100-column window shows on `row`: the cells right of its
    /// border, which sits at column 67.
    fn pane_row(terminal: &Terminal<ratatui::backend::TestBackend>, row: u16) -> String {
        screen_row(terminal, row).chars().skip(68).collect()
    }

    /// A left press on the first cell of the worktree pane cell that does `action`.
    fn press_worktree_cell(app: &mut App, action: WorktreeHitAction) -> bool {
        let area = app
            .worktree_hits
            .iter()
            .find(|hit| hit.action == action)
            .unwrap_or_else(|| panic!("the frame has no {action:?} cell"))
            .area;
        app.handle(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 1,
            area.y,
        ))
        .unwrap()
    }

    #[test]
    fn worktree_pane_toggles_and_lists_the_project_trees() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = worktree_pane_app(directory.path());
        let session = sleeping_session(&app, directory.path(), 1);
        let parser = session.parser.clone();
        app.project.workspaces[0].sessions.push(session);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();

        draw_frame(&mut app, &mut terminal);
        assert!(app.worktree_area.is_none() && app.worktree_hits.is_empty());
        assert_eq!(parser.lock().screen().size(), (27, 98));
        assert!(screen_row(&terminal, 0).ends_with(" F1 · F2 · F3 CPU OFF · F12 "));

        app.state_dirty = false;
        assert!(app.handle(key_event(KeyCode::F(2))).unwrap());
        assert!(app.worktree_pane.is_some());
        assert!(
            !app.state_dirty,
            "the pane is not part of the saved workspace"
        );
        draw_frame(&mut app, &mut terminal);
        // A third of 100 columns, with the terminal keeping the other 67.
        assert_eq!(app.worktree_area, Some(Rect::new(67, 1, 33, 29)));
        assert_eq!(parser.lock().screen().size(), (27, 65));
        let buffer = terminal.backend().buffer();
        assert!((1..30).all(|row| buffer[(67, row)].symbol() == "│"));
        // The header divider ends where the terminal does.
        assert_eq!(buffer[(66, 2)].symbol(), "─");
        assert!((2..6).all(|row| buffer[(98, row)].symbol() == "🗑"));
        // Main and locked rows dim the cell; the other rows leave it as it is.
        assert_eq!(buffer[(98, 2)].fg, buffer[(98, 5)].fg);
        assert_eq!(buffer[(98, 3)].fg, buffer[(98, 4)].fg);
        assert_ne!(buffer[(98, 2)].fg, buffer[(98, 3)].fg);
        // The focused tab's row stands out from the others.
        assert_ne!(buffer[(69, 2)].fg, buffer[(69, 3)].fg);
        assert_eq!(buffer[(69, 3)].fg, buffer[(69, 4)].fg);
        let header = pane_row(&terminal, 1);
        assert!(header.contains("Worktrees fixture"), "{header:?}");
        assert!(header.trim_end().ends_with('×'), "{header:?}");
        assert!(pane_row(&terminal, 2).contains("main ● main"));
        assert!(pane_row(&terminal, 3).contains("other ●"));
        assert!(!pane_row(&terminal, 3).contains("main"));
        assert!(pane_row(&terminal, 4).contains("feature/login"));
        assert!(!pane_row(&terminal, 4).contains('●'));
        assert!(pane_row(&terminal, 5).contains("frozen locked"));

        assert_eq!(app.worktree_hits.len(), 9);
        for index in 0..4 {
            let row = 2 + index as u16;
            for (action, area) in [
                (WorktreeHitAction::Open(index), Rect::new(68, row, 29, 1)),
                (WorktreeHitAction::Remove(index), Rect::new(97, row, 3, 1)),
            ] {
                let hit = app.worktree_hits.iter().find(|hit| hit.action == action);
                assert_eq!(hit.map(|hit| hit.area), Some(area), "{action:?}");
            }
        }
        let close: Vec<_> = app
            .worktree_hits
            .iter()
            .filter(|hit| hit.action == WorktreeHitAction::Close)
            .collect();
        assert_eq!(close.len(), 1);
        assert_eq!(close[0].area, Rect::new(97, 1, 3, 1));

        // F2 again closes it and gives the terminal its width back.
        assert!(app.handle(key_event(KeyCode::F(2))).unwrap());
        assert!(app.worktree_pane.is_none());
        assert!(app.worktree_area.is_none() && app.worktree_hits.is_empty());
        draw_frame(&mut app, &mut terminal);
        assert!(app.worktree_hits.is_empty());
        assert_eq!(parser.lock().screen().size(), (27, 98));
        assert_ne!(terminal.backend().buffer()[(67, 5)].symbol(), "│");

        // A resize drops the cells of the frame before it; the × cell closes the pane.
        app.handle(key_event(KeyCode::F(2))).unwrap();
        draw_frame(&mut app, &mut terminal);
        assert!(!app.worktree_hits.is_empty());
        app.handle(Event::Input(crossterm::event::Event::Resize(100, 30)))
            .unwrap();
        assert!(app.worktree_hits.is_empty());
        draw_frame(&mut app, &mut terminal);
        assert!(press_worktree_cell(&mut app, WorktreeHitAction::Close));
        assert!(app.worktree_pane.is_none());
    }

    #[test]
    fn worktree_pane_is_hidden_on_narrow_terminals() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = worktree_pane_app(directory.path());
        let session = sleeping_session(&app, directory.path(), 1);
        let parser = session.parser.clone();
        app.project.workspaces[0].sessions.push(session);
        app.worktree_pane = Some(WorktreePane::default());
        for (width, columns) in [(50, 48), (59, 57)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, 20)).unwrap();
            draw_frame(&mut app, &mut terminal);
            // The flag stays set, but nothing is drawn and the terminal keeps its width.
            assert!(app.worktree_pane.is_some());
            assert!(app.worktree_area.is_none());
            assert!(app.worktree_hits.is_empty());
            assert_eq!(parser.lock().screen().size(), (17, columns));
            let hint = screen_row(&terminal, 0);
            assert!(hint.ends_with(" F1 · F2 · F3 CPU OFF · F12 "), "{hint:?}");
        }
        // 60 columns is the first width that shows it, leaving 34 for the terminal.
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
        draw_frame(&mut app, &mut terminal);
        assert_eq!(app.worktree_area, Some(Rect::new(34, 1, 26, 19)));
        assert_eq!(parser.lock().screen().size(), (17, 32));
    }

    #[test]
    fn worktree_pane_shares_the_width_with_split_panes() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = worktree_pane_app(directory.path());
        app.project.layout = LayoutMode::Vertical;
        app.project.secondary = Some(1);
        app.worktree_pane = Some(WorktreePane::default());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        draw_frame(&mut app, &mut terminal);
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.worktree_area, Some(Rect::new(67, 1, 33, 23)));
        assert!(app.panes[1].body.right() < 67);
        // The divider's grip is a pair of box-drawing tees, which Kitty draws aligned with the
        // rails beside them.
        let handle = app.split_divider.expect("two panes share a divider").handle;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(handle.x, handle.y)].symbol(), "┤");
        assert_eq!(buffer[(handle.right() - 1, handle.y)].symbol(), "├");
        // At 85 columns the 57 left beside the pane cannot hold two panes; unlike a
        // notice, the pane may collapse the split.
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(85, 24)).unwrap();
        draw_frame(&mut app, &mut terminal);
        assert_eq!(app.panes.len(), 1);
        app.worktree_pane = None;
        draw_frame(&mut app, &mut terminal);
        assert_eq!(app.panes.len(), 2);
    }

    #[test]
    fn clicking_a_row_opens_or_focuses_its_tab() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = worktree_pane_app(directory.path());
        // Both tabs already have a session, so selecting one starts nothing.
        for (id, workspace) in (1..).zip(0..2) {
            let session = sleeping_session(&app, directory.path(), id);
            app.project.workspaces[workspace].sessions.push(session);
        }
        app.next_session = 3;
        app.worktree_pane = Some(WorktreePane::default());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        draw_frame(&mut app, &mut terminal);

        // An unopened checkout gets a tab, with the default agent, and the focus.
        assert!(press_worktree_cell(&mut app, WorktreeHitAction::Open(2)));
        assert_eq!(app.project.workspaces.len(), 3);
        assert_eq!(app.focused().tree.id, "feature/login");
        assert_eq!(app.focused().sessions.len(), 1);
        assert!(
            app.worktree_hits.is_empty(),
            "the old frame's cells are gone"
        );

        // An open tab only takes the focus: no second tab or session.
        draw_frame(&mut app, &mut terminal);
        press_worktree_cell(&mut app, WorktreeHitAction::Open(0));
        assert_eq!(app.project.workspaces.len(), 3);
        assert_eq!(app.focused().tree.id, "main");
        assert_eq!(app.project.workspaces[0].sessions.len(), 1);

        // A checkout missing on disk cannot open; it says so.
        app.project.trees[3].prunable = true;
        draw_frame(&mut app, &mut terminal);
        press_worktree_cell(&mut app, WorktreeHitAction::Open(3));
        assert_eq!(app.project.workspaces.len(), 3);
        assert_eq!(
            app.notice.as_deref(),
            Some("This checkout is missing on disk; delete it or recreate the branch.")
        );

        // The listing can change between frames: a click on a row that is gone does nothing.
        draw_frame(&mut app, &mut terminal);
        let stale = app
            .worktree_hits
            .iter()
            .find(|hit| hit.action == WorktreeHitAction::Open(3))
            .unwrap()
            .area;
        app.project.trees.truncate(2);
        app.handle(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            stale.x + 1,
            stale.y,
        ))
        .unwrap();
        assert_eq!(app.project.workspaces.len(), 3);
        assert_eq!(app.focused().tree.id, "main");
        assert!(app.modal.is_none());
    }

    #[test]
    fn trash_asks_for_confirmation_then_queues_removal_for_that_tree() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, queued, _events) = worktree_pane_app(directory.path());
        app.worktree_pane = Some(WorktreePane::default());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        draw_frame(&mut app, &mut terminal);

        // A checkout with no tab: the dialog names its branch, and nothing runs before `y`.
        assert!(press_worktree_cell(&mut app, WorktreeHitAction::Remove(2)));
        assert!(matches!(
            &app.modal,
            Some(Modal::Confirm(Confirm::RemoveWorktree { tree: Some(tree) })) if tree.id == "feature/login"
        ));
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let rows = screen_rows(&terminal);
        assert!(
            rows.iter()
                .any(|row| row.contains("Delete worktree feature/login?")),
            "{rows:#?}"
        );
        assert!(rows.iter().any(|row| row.contains("Branch remains.")));
        assert!(queued.try_recv().is_err());
        app.handle(key_event(KeyCode::Char('y'))).unwrap();
        let Job::Remove { tree, .. } = queued.recv().unwrap() else {
            panic!("expected a removal");
        };
        assert_eq!(tree.id, "feature/login");
        assert!(matches!(
            &app.modal,
            Some(Modal::Removal { tree, error: None, .. }) if tree == "feature/login"
        ));
        // No tab will close, so the progress dialog does not promise one.
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let rows = screen_rows(&terminal);
        assert!(
            rows.iter()
                .any(|row| row.contains("Checking and removing worktree"))
        );
        assert!(
            !rows.iter().any(|row| row.contains("tab will close")),
            "{rows:#?}"
        );
        assert!(
            app.project
                .workspaces
                .iter()
                .all(|workspace| !workspace.pending)
        );
        // Success dismisses the progress dialog and the row, though no tab was involved.
        app.handle(Event::Removed {
            tree: "feature/login".into(),
            result: Ok(None),
        })
        .unwrap();
        assert!(app.modal.is_none());
        assert!(
            app.project
                .trees
                .iter()
                .all(|tree| tree.id != "feature/login")
        );
        assert_eq!(app.project.workspaces.len(), 2);

        // An open tab that is not focused: the row picks the checkout, not the focus.
        draw_frame(&mut app, &mut terminal);
        press_worktree_cell(&mut app, WorktreeHitAction::Remove(1));
        app.handle(key_event(KeyCode::Char('y'))).unwrap();
        let Job::Remove { tree, .. } = queued.recv().unwrap() else {
            panic!("expected a removal");
        };
        assert_eq!(tree.id, "other");
        assert!(app.project.workspaces[1].pending);
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app);
            })
            .unwrap();
        let rows = screen_rows(&terminal);
        assert!(
            rows.iter()
                .any(|row| row.contains("The tab will close on success.")),
            "{rows:#?}"
        );
        assert_eq!(app.focused().tree.id, "main");
        app.handle(Event::Removed {
            tree: "other".into(),
            result: Ok(None),
        })
        .unwrap();
        assert!(app.modal.is_none(), "the dialog was this removal's");
        assert_eq!(app.project.workspaces.len(), 1);
        assert_eq!(app.focused().tree.id, "main");

        // The main and locked rows only explain why they stay.
        draw_frame(&mut app, &mut terminal);
        press_worktree_cell(&mut app, WorktreeHitAction::Remove(0));
        assert!(app.modal.is_none());
        assert_eq!(
            app.notice.as_deref(),
            Some("The main checkout cannot be removed.")
        );
        draw_frame(&mut app, &mut terminal);
        press_worktree_cell(&mut app, WorktreeHitAction::Remove(1));
        assert!(app.modal.is_none());
        assert_eq!(app.notice.as_deref(), Some("This worktree is locked."));
        assert!(queued.try_recv().is_err());
    }

    #[test]
    fn removal_commands_still_target_the_focused_checkout() {
        let (mut app, queued, _events) = interaction_app();
        app.project.active = 1;
        app.run_command(IntentCommand::RemoveWorktree).unwrap();
        assert!(matches!(
            &app.modal,
            Some(Modal::Confirm(Confirm::RemoveWorktree { tree: None }))
        ));
        app.modal_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(queued.recv().unwrap(), Job::Remove { tree, .. } if tree.id == "other"));
        assert!(app.project.workspaces[1].pending);
    }

    #[test]
    fn worktree_pane_scrolls_long_lists_with_the_wheel() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = worktree_pane_app(directory.path());
        for number in 0..12 {
            let mut tree = app.project.trees[2].clone();
            tree.id = format!("extra-{number}");
            tree.branch = format!("extra-{number}");
            app.project.trees.push(tree);
        }
        app.worktree_pane = Some(WorktreePane::default());
        // A header and ten list lines for sixteen worktrees.
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
        draw_frame(&mut app, &mut terminal);
        assert!(pane_row(&terminal, 2).contains("main ● main"));
        assert!(pane_row(&terminal, 10).contains("extra-4"));
        assert!(pane_row(&terminal, 11).contains("↓ 7 more"));
        let opens = |app: &App| {
            app.worktree_hits
                .iter()
                .filter(|hit| matches!(hit.action, WorktreeHitAction::Open(_)))
                .count()
        };
        assert_eq!(opens(&app), 9);

        let wheel = |app: &mut App, kind| app.handle(mouse_event(kind, 80, 5)).unwrap();
        for _ in 0..3 {
            assert!(wheel(&mut app, MouseEventKind::ScrollDown));
        }
        draw_frame(&mut app, &mut terminal);
        assert!(pane_row(&terminal, 2).contains("↑ 3 more"));
        assert!(pane_row(&terminal, 3).contains("frozen"));
        assert!(pane_row(&terminal, 11).contains("↓ 5 more"));
        assert_eq!(opens(&app), 8);

        // Scrolling past the end stops at the last row, and one notch back moves at once.
        for _ in 0..20 {
            wheel(&mut app, MouseEventKind::ScrollDown);
        }
        draw_frame(&mut app, &mut terminal);
        assert!(pane_row(&terminal, 2).contains("↑ 7 more"));
        assert!(pane_row(&terminal, 11).contains("extra-11"));
        assert_eq!(app.worktree_pane.as_ref().unwrap().scroll, 7);
        wheel(&mut app, MouseEventKind::ScrollUp);
        draw_frame(&mut app, &mut terminal);
        assert!(pane_row(&terminal, 2).contains("↑ 6 more"));
        assert!(pane_row(&terminal, 11).contains("↓ 2 more"));
    }

    #[test]
    fn worktree_pane_mouse_events_never_reach_the_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = worktree_pane_app(directory.path());
        let output = directory.path().join("received");
        let session = recording_session(1, directory.path(), app.tx.clone(), &output);
        app.project.workspaces[0].sessions.push(session);
        app.worktree_pane = Some(WorktreePane::default());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        draw_frame(&mut app, &mut terminal);
        let pane = app.worktree_area.unwrap();
        let body = app.panes[0].body;
        assert!(body.right() <= pane.x);
        // On the border, on a list row and on blank space, whatever the event is the pane's.
        for (column, row) in [
            (pane.x, pane.y + 4),
            (pane.x + 3, pane.y + 3),
            (pane.x + 5, pane.bottom() - 2),
        ] {
            for kind in [
                MouseEventKind::Down(MouseButton::Right),
                MouseEventKind::Down(MouseButton::Middle),
                MouseEventKind::ScrollUp,
                MouseEventKind::ScrollDown,
                MouseEventKind::Moved,
                MouseEventKind::Drag(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                app.handle(mouse_event(kind, column, row)).unwrap();
            }
        }
        assert!(app.mouse_capture.is_none());
        assert!(
            !app.handle(mouse_event(MouseEventKind::Moved, pane.x + 3, pane.y + 3))
                .unwrap()
        );
        // Anything the program had received would come before this click, the one thing
        // that reaches it.
        app.handle(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            body.x + 2,
            body.y + 2,
        ))
        .unwrap();
        wait_for_file(&output, b"\x1b[<0;3;3M");
    }

    #[test]
    fn worktree_pane_explains_projects_outside_git() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = interaction_app();
        app.project = Project::terminal(directory.path()).unwrap();
        app.worktree_pane = Some(WorktreePane::default());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        draw_frame(&mut app, &mut terminal);
        let rows: Vec<String> = (1..6).map(|row| pane_row(&terminal, row)).collect();
        assert!(rows[0].contains("Worktrees"), "{rows:?}");
        for expected in ["Not a Git project.", "Open a Git folder", "worktrees."] {
            assert!(rows.iter().any(|row| row.contains(expected)), "{rows:?}");
        }
        let actions: Vec<_> = app.worktree_hits.iter().map(|hit| hit.action).collect();
        assert_eq!(actions, [WorktreeHitAction::Close]);
    }

    #[test]
    fn worktree_list_follows_the_git_poll_and_new_checkouts() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _queued, _events) = worktree_pane_app(directory.path());
        let common_dir = PathBuf::from("/interaction-fixture/.git");
        let poll = |app: &mut App, trees: Vec<Worktree>| {
            app.handle(Event::Metadata(vec![(common_dir.clone(), trees)]))
                .unwrap()
        };
        // The same listing changes nothing, so it asks for no frame.
        let listed = app.project.trees.clone();
        assert!(!poll(&mut app, listed.clone()));
        // A worktree gone or added elsewhere is kept at the next poll, with no frame to draw
        // while the pane is closed...
        let mut next = listed;
        next.remove(3);
        let mut fresh = next[2].clone();
        fresh.id = "fresh".into();
        fresh.branch = "fresh".into();
        next.push(fresh);
        assert!(!poll(&mut app, next.clone()));
        assert_eq!(app.project.trees, next);
        // ...and with a frame for the pane that lists it.
        app.worktree_pane = Some(WorktreePane::default());
        let mut later = next.clone();
        later.pop();
        assert!(poll(&mut app, later.clone()));
        assert_eq!(app.project.trees, later);
        assert!(!poll(&mut app, later));

        // A checkout made here is listed at once, and once, ahead of the next poll.
        let created = Worktree {
            id: "created".into(),
            path: directory.path().join("feature"),
            branch: "created".into(),
            head: String::new(),
            is_main: false,
            locked: false,
            bare: false,
            prunable: false,
        };
        for _ in 0..2 {
            app.handle(Event::Created {
                project: common_dir.clone(),
                source: PathBuf::from("/interaction-fixture"),
                preparation_error: None,
                agent: None,
                split: None,
                result: Ok(created.clone()),
            })
            .unwrap();
            let listed = app.project.trees.iter().filter(|tree| tree.id == "created");
            assert_eq!(listed.count(), 1);
        }
    }

    #[test]
    fn f2_belongs_to_the_terminal_in_native_mode_and_to_the_launcher_in_ask_crit() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, queued, _events) = interaction_app();
        let output = directory.path().join("received");
        let session = recording_session(1, directory.path(), app.tx.clone(), &output);
        app.project.workspaces[0].sessions.push(session);
        // Native mode forwards every key but F12, F2 included.
        app.native_keys = true;
        app.handle(key_event(KeyCode::F(2))).unwrap();
        assert!(app.worktree_pane.is_none());
        wait_for_file(&output, b"\x1bOQ");
        app.native_keys = false;

        // Ask Crit keeps F2 for its manual controls, whose entry toggles the pane.
        app.open_intent().unwrap();
        queued.recv().unwrap();
        app.handle(key_event(KeyCode::F(2))).unwrap();
        assert!(matches!(app.modal, Some(Modal::Commands { .. })));
        assert!(app.worktree_pane.is_none());
        let selected = COMMANDS
            .iter()
            .position(|(_, command)| *command == IntentCommand::Worktrees)
            .unwrap();
        app.modal = Some(Modal::Commands { selected });
        app.handle(key_event(KeyCode::Enter)).unwrap();
        assert!(app.modal.is_none());
        assert!(app.worktree_pane.is_some());
        app.run_command(IntentCommand::Worktrees).unwrap();
        assert!(app.worktree_pane.is_none());

        // Only a plain F2 is the pane's.
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::CONTROL] {
            app.handle(Event::Input(crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::F(2),
                modifiers,
            ))))
            .unwrap();
            assert!(app.worktree_pane.is_none());
        }
    }

    #[test]
    fn f3_retunes_the_agent_scope_through_the_worker_and_is_saved() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, queued, _events) = interaction_app();
        app.limiter =
            limits::Limiter::scope(directory.path(), "crit-test.scope", 50, 8, 32 << 30, true);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        draw_frame(&mut app, &mut terminal);
        assert!(screen_row(&terminal, 0).ends_with(" F3 CPU 50% · F12 CRIT KEYS "));

        app.handle(key_event(KeyCode::F(3))).unwrap();
        assert!(!app.limiter.limited());
        assert_eq!(
            app.notice.as_deref(),
            Some("Agent CPU and memory limit off.")
        );
        // systemd is never waited for here: the worker retunes the running agents.
        assert!(matches!(queued.try_recv(), Ok(Job::RetuneAgentCpu(_))));
        let snapshot = app.snapshot();
        assert_eq!(
            (snapshot.agent_cpu_limited, snapshot.agent_cpu_percent),
            (false, 50)
        );
        assert!(app.state_dirty);
        draw_frame(&mut app, &mut terminal);
        assert!(screen_row(&terminal, 0).ends_with(" F3 CPU OFF · F12 CRIT KEYS "));

        app.handle(key_event(KeyCode::F(3))).unwrap();
        assert!(app.limiter.limited());
        assert_eq!(
            app.notice.as_deref(),
            Some("Agents limited to 50% of the CPU (4 of 8 cores) and memory (16.0 GiB).")
        );
        assert!(matches!(queued.try_recv(), Ok(Job::RetuneAgentCpu(_))));

        // A retune that failed joins the notice row.
        let report = Event::CpuLimit("could not retune".into());
        app.handle(report).unwrap();
        let notice = app.notice.as_deref().unwrap_or_default();
        assert!(notice.ends_with("\ncould not retune"), "{notice:?}");

        // With no room for the retune, nothing changes and F3 can be pressed again.
        app.jobs.try_send(Job::RefreshMetadata(Vec::new())).unwrap();
        let error = format!("{:#}", app.handle(key_event(KeyCode::F(3))).unwrap_err());
        assert!(error.contains("command queue is busy"), "{error}");
        assert!(app.limiter.limited());
        assert!(matches!(queued.try_recv(), Ok(Job::RefreshMetadata(_))));

        // Native mode forwards F3 to the terminal, like every key but F12.
        let output = directory.path().join("received");
        let session = recording_session(1, directory.path(), app.tx.clone(), &output);
        app.project.workspaces[0].sessions.push(session);
        app.native_keys = true;
        app.handle(key_event(KeyCode::F(3))).unwrap();
        wait_for_file(&output, b"\x1bOR");
        assert!(app.limiter.limited());
        assert!(queued.try_recv().is_err());
    }

    #[test]
    fn f3_without_a_scope_lowers_running_agents_priority_and_says_what_stays() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, queued, _events) = interaction_app();
        assert!(!app.limiter.limited() && !app.limiter.uses_scope());
        let nice = |pid: u32| -> i64 {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let fields = stat.rsplit_once(") ").unwrap().1;
            fields.split_whitespace().nth(16).unwrap().parse().unwrap()
        };
        let own = nice(std::process::id());
        let agent = Session::spawn(
            1,
            "omp",
            None,
            directory.path(),
            &app.socket,
            app.tx.clone(),
            10,
            40,
            Some(("/bin/sleep".into(), vec!["30".into()])),
            None,
        )
        .unwrap();
        let shell = sleeping_session(&app, directory.path(), 2);
        let (agent_pid, shell_pid) = (agent.pid().unwrap(), shell.pid().unwrap());
        app.project.workspaces[0].sessions.extend([agent, shell]);

        app.handle(key_event(KeyCode::F(3))).unwrap();
        assert!(app.limiter.limited());
        assert!(queued.try_recv().is_err());
        let notice = |app: &App| app.notice.clone().unwrap_or_default();
        assert!(notice(&app).contains("lower CPU priority"));
        // Only the agent yields; no process can lower its niceness below its parent's.
        assert_eq!(nice(agent_pid), own.max(10));
        assert_eq!(nice(shell_pid), own);

        app.handle(key_event(KeyCode::F(3))).unwrap();
        assert!(!app.limiter.limited());
        assert!(notice(&app).contains("started from now on"));
        assert_eq!(nice(agent_pid), own.max(10));
    }
}
