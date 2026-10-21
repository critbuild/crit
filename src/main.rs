mod app;
mod checks;
mod clipboard;
mod compose;
mod event;
mod integration;
mod intent;
mod keys;
mod kitty;
mod limits;
mod restore;
mod sandbox;
mod selection;
mod terminal;
mod ui;
mod updates;
mod worktree;

use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const HELP: &str = "crit — worktree-first terminals for OMP and OpenCode

USAGE
  crit [--fresh] [--repo PATH] [--agent omp|opencode|shell] [--no-kitty]
       [--agent-cpu PERCENT]
  crit worktree list [--repo PATH]
  crit worktree create BRANCH [--base REF] [--repo PATH]
  crit worktree remove PATH [--repo PATH]
  crit check acquire|request|release|cancel|status|restart|logs [--repo PATH]
  crit check release --automatic [--repo PATH]
  crit check exec [--repo PATH] -- PROGRAM [ARGS...]
  crit setup kitty|agents|all
  crit keys
  crit sandbox [status|clean [--all]]

Crit restores your saved workspace on startup. With no saved workspace, choose
New terminal or any folder. F1 opens Ask Crit. Ctrl+D goes to the focused pane:
OMP and OpenCode quit on it, a shell reads end of input. When a shell or agent
ends, or Ctrl+Shift+W closes a pane, the pane closes like a Kitty window: a
split's other pane goes full width, an emptied tab closes, and the last tab
closes Crit. An agent that fails, or quits within a second of starting, stays
open with its exit status (F1 to restart).
Ctrl+Shift+T chooses Home, open folder, new worktree, or existing worktree.
Choose the folder, branch name, or registered checkout first, then OMP /
OpenCode / Terminal. Nothing launches before the program choice. Esc cancels.
Switch projects from the tab bar, F1, or F1 then F2.
F2 opens global resources on the right: live sessions and checking runtimes
grouped by project, independent of the selected tab. Click a session to focus
it without restarting it; registered checkout rows retain open/delete controls.
Ctrl+Shift+, or . opens a split picker:
New worktree creates a separate checkout; Split terminal opens a shell in the
same folder. Open or manage existing checkouts with F2.
F12 persistently toggles Crit/native Kitty shortcuts. Run `crit setup kitty` once
to enable the window-scoped mappings.

ASK CRIT
  F1 opens a natural-language launcher using anthropic/claude-haiku-5-5,
  through your authenticated OMP installation. It is separate from the
  coding agents: no tools, extensions, skills, rules, or persistent session.
  Examples: \"Start a branch for login fixes in a separate worktree\",
  \"Open portfolio with OpenCode\", or \"Show this workspace's logs\".
  Vague requests ask a follow-up; answer in the same box. New branches use
  separate worktrees, based on the focused checkout's HEAD unless specified.
  An explicit base applies only to new branches.
  Enter sends; Esc cancels and stops the interpreter; Ctrl+U clears input.
  No model runs while idle. Requests have a 50-second local deadline.
  Crit sends your request history and project/worktree names and paths,
  not source files. Context plus conversation is limited to 64 KiB.
  Discovered projects beyond about 48 KiB are left out.
  Only supported Crit actions and known destinations can execute, never
  model-generated shell commands. Destructive actions still need confirmation.
  Errors remain visible without executing a guessed action or fallback model.
  F2 inside the launcher opens manual controls: Up/Down and Enter, no AI needed.

APPEARANCE
  Crit uses the terminal's default background, including its tabs and
  dialogs, so leftover pixels at the window edges blend into the UI.
  Embedded agents querying the background receive this Kitty window's current
  color when its remote-control socket is available.
  Crit removes Kitty padding/margins only in its own terminal and restores
  configured spacing on exit. Other terminals are unaffected.
  A muted, low-contrast divider spans the full terminal width below workspace/
  session headers. Stroke thickness is terminal-rendered; its row is excluded
  from the embedded terminal's size.
  The bottom row always belongs to the embedded terminal.
  Window shape, rounded corners, borders, shadows, and desktop gaps stay under
  your normal compositor configuration; Crit installs no desktop overrides.
  The minimum interactive size is 30x10. Below it, typing, paste, mouse input,
  and confirmations pause; Esc and F12 remain available. Enlarge to resume.
  Real errors and startup recovery warnings appear in centered dialogs.
  Enter or Esc dismisses them; ordinary input is blocked until then, while
  F12 remains available. Dialogs never resize the embedded terminal.
  Routine closes, limit toggles, cleanup and creation successes are silent.
  Selected sessions stay visible in the workspace header even when other
  session labels overflow.
  Worktree tabs fit their labels without padding short names. Longer labels
  scroll through an 18-column slot. A compact status column follows the name:
  a baseline-aligned braille spinner while working, a blinking ◆ for a
  decision, ○ while starting, and × when exited. A green ✓ means an unread completed
  turn, not merely idle. Viewing the tab in the focused window clears it
  automatically; a turn finishing while already visible is read immediately.
  Hidden tabs keep their check until viewed. Idle shells do not notify.
  Shortcut labels stay muted; F2 highlights while the worktree pane is open.
  Amber marks CPU limits turned off or native Kitty key routing.
  A thin scroll thumb appears in the right gutter when normal-screen history
  exists; its whole column is clickable and draggable to review history or
  return to live output at the bottom.
  Mouse-aware apps receive the wheel normally; Shift+wheel scrolls Crit's
  history instead. Typing or pasting returns to live output.
  Side-by-side panes share a thin rail with a centered highlighted segment.
  Drag the rail or the column beside it without changing focus.
  Each pane keeps a usable minimum width; the proportion follows window resizing
  and is saved per project. Narrow windows show one pane until both fit again.
  After a pane changes size, Crit waits for the app's repaint before drawing
  it, so a resize never shows cut-off lines; apps that do not repaint are
  drawn after a short pause.
  While the Crit window does not have focus (you are in a game or another
  window), its tab bar holds still: spinners, the blinking ◆ and scrolling
  labels show one still frame. Pane output is drawn at most 10 times a second,
  and Git branches and worktrees are checked every 10 seconds instead of every
  second. Focusing the window brings all of it back at once, with a fresh Git
  check. Terminals that do not report focus always count as focused.

COPY, PASTE, AND LINKS
  Drag terminal text to select it and copy to the clipboard and primary selection.
  Ctrl+Shift+C copies again. Ctrl+drag overrides a child application's mouse capture.
  Ctrl+Shift+V uses Kitty's clipboard paste; Shift+Insert uses primary selection.
  Middle-click and forwarded Ctrl+Shift+V use wl-paste, xclip, or xsel locally.
  Over SSH, native terminal paste works without those local clipboard tools.
  Click plain URLs or underlined OSC 8 links to open the exact target via xdg-open.
  Agent OSC 52 clipboard writes are forwarded; unsolicited clipboard reads are not.
  Run crit setup kitty after upgrading to install the updated clipboard mappings;
  reload Kitty configuration with Ctrl+Shift+F5. Other Kitty windows are unaffected.

AGENT CPU
  Crit limits how much CPU and memory OMP and OpenCode panes use, together
  with everything they run (builds, tests, tools), so that a game or other
  apps stay responsive while agents work. Shell panes are never limited. By
  default the agent panes of a window share 50% of the machine's CPU (half of
  its cores) and 50% of its memory; --agent-cpu PERCENT (1-100) sets both
  shares and turns the limit on. F3 turns the limit off and on again, for
  running agents too; the key hint shows the share, or Off. The share and
  F3's choice are saved with the workspace, like F12; --fresh starts over
  with the default. In native Kitty mode F3 goes to the terminal like every
  key except F12.
  The agents run in a transient systemd user scope named after the window
  (crit-<pid>-<random>.scope) that holds the CPU quota and the memory share
  (MemoryHigh). While limited they also get a low CPU weight, so a game and
  other apps take precedence whenever the CPUs are contended, even while the
  agents use less than their share. A scope past its memory share is not
  killed: it is throttled and reclaimed, its pages swapped out first, though
  systemd-oomd may still kill it when swap runs out. F3 toggles silently;
  the lower priority below never limits memory.
  While limited, the scope also keeps the agents off the machine's best
  cores (AllowedCPUs): they get its least-preferred physical cores, both
  threads of each, as few as cover the share; at 50% of an 8-core, 16-thread
  CPU, the 4 cores it ranks lowest. The ranking is AMD's preferred-core
  ranking, else ACPI CPPC's highest performance, else the CPU capacity the
  kernel reports; Intel's E-cores rank below all others, and of equal cores
  the highest-numbered go first, so that CPU 0 stays free. The agents'
  threads also run as SCHED_BATCH, so they never preempt a game's threads
  when they wake up, and agents started while limited get a 1 ms timer slack,
  which lets the kernel batch their timer wake-ups. F3 moves running agents'
  cores and class live; their timer slack stays as it was when they started,
  since changing another process's takes CAP_SYS_NICE. A user manager that
  cannot hand out the cpuset controller leaves the agents on every core.
  Threads that another tool put in SCHED_IDLE or a real-time class stay there.
  An agent that cannot join the scope still starts, unlimited, and an error
  dialog says so. Docker containers run under the Docker daemon, outside the limit.
  Without cgroup v2, a systemd user session, busctl, or a user
  manager that can limit CPU, agents run at a lower priority (nice 10) while
  the limit is on instead, those started while it is on as SCHED_BATCH with
  the 1 ms timer slack too; the key hint shows Low, and a startup warning
  says why. No process can raise that priority again, so turning the limit
  off then applies only to agents started afterwards.

SCRATCH SPACE
  Agents leave scratch behind: clones, virtualenvs, builds, downloads. In a
  RAM-backed /tmp those files hold memory, then swap, until they are deleted,
  and ending the agents frees none of it. So every pane's TMPDIR, TMP and TEMP
  point to its workspace's folder in a disk-backed sandbox, shells included:
  the agents' own temp files, worktrees and shell snapshots go there. OMP and
  OpenCode also get a system-prompt note telling the agent to keep clones,
  virtualenvs, builds and downloads there, never under /tmp. Crit's own
  helpers (the updaters, Ask Crit, Git, Docker) use the sandbox too.
  The sandbox is the first of these that works: CRIT_SANDBOX=DIR, on any
  filesystem; $TMPDIR/crit-USER when TMPDIR is on a disk (inside a Crit pane,
  the sandbox TMPDIR is in); /var/tmp/crit-USER; $XDG_CACHE_HOME/crit/sandbox
  (default ~/.cache/crit/sandbox). It must be a directory you own, not a
  symlink, and Crit makes it private. Each workspace has SANDBOX/FOLDER-ID
  there: its folder's name, then an ID of its path.
  Crit cleans the sandbox by itself. A checkout deleted from Crit, or with
  crit worktree remove, takes its scratch with it at once. A pass at startup
  and then daily, in the background at idle CPU and I/O priority, removes the
  scratch of folders that no longer exist, anything inside a workspace's
  scratch or left by Crit's own helpers untouched for 30 days, and the empty
  folders of workspaces unused for 30 days; successful cleanup is silent. A
  sandbox under /var/tmp is also aged by the system (systemd-tmpfiles).
  crit sandbox status (or just crit sandbox) shows the sandbox, the space each
  workspace's scratch takes, and your largest entries in a RAM-backed /tmp.
  crit sandbox clean runs a pass now; with --all it deletes all scratch, but
  only while no window owns the saved workspace.
  A startup warning says when a RAM-backed /tmp holds over 1 GiB. Files an
  agent writes to a literal /tmp/... path despite the note still land in RAM,
  and Crit never deletes them: it did not make them. Without a usable
  sandbox, scratch stays in the default temp folder, and a warning says why
  when that is in RAM.

AGENT RUNTIME
  OMP and OpenCode run on Bun, whose garbage collector keeps waking the CPU
  while an agent works, and even while it waits on a command. So their panes
  get BUN_GC_TIMER_INTERVAL=10000: the collections Bun forces while memory
  changes come every 10 seconds instead of every second, and JavaScriptCore
  still collects as memory fills. They also get BUN_JSC_numberOfGCMarkers=2:
  collections mark on two threads instead of up to one per core. Bun refuses
  to start with a BUN_JSC_ option its JavaScriptCore does not know, and those
  options change between releases, so Crit passes this one only to an agent
  whose executable, found in Crit's PATH, ships it. Everything an agent runs,
  such as bun test, inherits both.
  OMP panes also get PI_CONFIG_FILES naming Crit's settings overlay,
  omp-overlay.yml in $XDG_DATA_HOME/crit/integrations (default
  ~/.local/share/crit/integrations). It turns off tui.titleState,
  terminal.showProgress and terminal.programStatus: the spinner OMP animates
  in the terminal title while it works, its OSC 9;4 progress and its OSC 7501
  status, none of which Crit shows. Overlays of your own in PI_CONFIG_FILES
  come after Crit's, so yours win.
  A BUN_GC_TIMER_INTERVAL or BUN_JSC_numberOfGCMarkers already set in Crit's
  environment wins: the panes inherit it as it is. Shell panes get none of
  these.

UPDATES
  Every startup, the window owning the saved workspace (see RESTORE) checks
  OMP and OpenCode in parallel on the animated blue-scan startup screen.
  Separate rows show each tool checking, ready, not installed, or using its
  installed version after a failure. Other windows never run the checks.
  Crit waits for the checks and any necessary installs before restoring or
  launching agents, so those panes use the updated executables immediately.
  There is no daily throttle. OMP's native updater checks release metadata
  and installs only a newer build, respecting its stable/canary setting.
  For OpenCode 1, Crit checks the official npm release metadata first and
  runs opencode upgrade VERSION only for a newer version of the same major.
  An npm-installed OpenCode's version is read from its package manifest,
  without starting its runtime; other installs use --version. Timestamped
  preview builds follow their own npm tag. Other majors and custom builds
  keep their native upgrade check rather than guessing their release channel.
  OpenCode metadata requests have 15 seconds. Updater banners stay out of the
  UI. A failed check or update shows an error dialog and startup continues
  with the installed version. A missing tool is not a failure.
  Esc quits the startup screen without replacing the saved workspace; an
  installer already running is left to finish. Every updater has 10 minutes,
  even after Crit quits: one still running then is stopped with
  everything it started (and killed 30 seconds later if it keeps running).
  A running Crit reports that it timed out. The process deadlines need
  coreutils timeout; without it, child processes are not limited.

PROJECTS
  Run crit from any folder, including a Git checkout, to choose New terminal
  or a project. New terminal starts your shell in the launch folder; it needs
  no Git repository and does not create one. It is the first, selected row.
  Explicit --repo or --agent can open any existing folder directly, with the
  requested agent or shell. With a saved workspace, that folder opens as an
  extra tab after the restore. No Git initialization is required, including
  for research.
  Interactive new tabs choose a destination option, then OMP, OpenCode or
  Terminal; --agent may preselect that choice but does not skip it.
  Opening an already-running tab keeps its sessions; an unopened checkout
  selected from F2 asks which agent or shell to launch.
  Crit passes --allow-home to OMP so a home-folder workspace stays in home,
  rather than being automatically relocated to /tmp.
  The picker finds Git projects near the launch folder and under your home.
  Use Up/Down and Enter; no manual cd needed.
  Typing or pasting filters project names and paths, ignoring case.
  Up/Down keeps the filter; Enter opens the highlighted match. Ctrl+U clears
  the filter and restores New terminal on the startup screen.
  Discovery is a bounded, one-shot search up to four directory levels deep,
  skipping hidden, dependency, and build directories. You can always type
  or paste a project path instead. Escape exits without opening a project.
  Stale Git markers do not hide valid projects beneath them.
  Ask \"open a project\" to browse Git folders, subfolders, or worktrees.
  Absolute paths, ~, and paths containing / open directly. Other relative folders
  open when no detected project matches; paths use the displayed base folder.
  A folder outside Git opens as a workspace named after the folder and can run
  OMP, OpenCode, or shells. Home (~) is first and preselected in the new-tab menu.
  Each Enter opens a new ~ tab with a fresh session (a fresh session in the
  existing tab if ~ is a Git checkout).
  Opening a Git folder opens only its checkout, not every branch/worktree.
  Ask to open an existing worktree or switch to a named project.
  Already open projects and shell/agent sessions stay alive when switching.
  Branch labels refresh automatically after git switch, checkout, or branch rename,
  including background tabs. Detached checkouts are labeled detached HEAD.
  An ordinary folder workspace can open Git projects later through F1 / F2.
  Worktree and Docker actions require opening a Git project first.
  New worktrees use the focused checkout's current HEAD, not another project.
  Esc cancels pending project opens and worktree listings; late results
  cannot reopen canceled dialogs or replace newer ones. Failed worktree creation
  restores its input only while its original workspace is focused and no newer
  dialog is open.
  The split picker offers New worktree and Split terminal.
  New worktree asks for a branch, then a program, and leaves the first pane alone.
  Split terminal starts a shell in the same folder, whatever the focused pane
  runs, without creating a branch, worktree, or Docker environment: a terminal
  beside the agent. Ctrl+D in the agent then leaves the terminal at full
  width. It also works in non-Git folders. Mouse clicks or Ctrl+Shift+[ / ]
  switch panes. Ctrl+Shift+W closes the focused session; its peer stays running.
  F2 lists registered checkouts, not local branches without checkouts. Click a
  checkout to open its tab or focus it if already open; use 🗑 to delete it.
  Returning a split to full width keeps the focused session. Saved terminal
  splits reopen both sessions in the same workspace, with their focus and ratio.
  Saved projects, worktrees, splits, focused panes and selected sessions reopen.

RESOURCES AND WORKTREES (F2)
  F2 toggles a global pane on the right, independent of the selected project
  or tab. All live agent and shell sessions in this window are grouped by
  project and worktree, in project opening order. Idle processes still count;
  exited panes do not. Terminal-only folders are included.
  Click a session to focus its existing process, a project to return to it,
  or a registered checkout to open/focus its tab. Click × or press F2 to close
  the pane. Keyboard focus stays in the terminal while it is open.
  In native Kitty mode F2 goes to the terminal like every key except F12;
  Ask Crit keeps F2 for manual controls.
  Windows narrower than 60 columns hide the pane and give the terminal its full
  width; it returns when the window is wide enough. While it shows, the
  terminal (and any split) lays out in the remaining width. It belongs to the
  window and is not saved.
  The list is live: session changes appear with the next frame. Git metadata
  for every open project is checked each second (every 10 seconds unfocused)
  and refreshed when changed, with a 30-second safety refresh.
  Each row shows the branch (detached HEAD when there is none) and markers:
  ● a tab is open, main, locked, missing (checkout deleted outside Git; it
  cannot be opened). The focused tab's row is highlighted. Long lists scroll
  with the wheel over the pane, with ↑ n more / ↓ n more marking hidden rows.
  Each project's check summary names its phase, owner branch and queue count,
  including live/uncertain check projects without a local open tab. Owner and
  Q1/Q2 markers follow registered worktrees even without open tabs.
  Starting/stopping are transitions; blocked means cleanup or startup needs
  attention. Check environment status shows its error. Not requested describes
  Crit's ownership, not the state of any external Docker stack.
  This is a live process/runtime inventory, not sampled CPU or RAM telemetry.
  🗑 at the end of a row asks for confirmation, then deletes the checkout under
  the same rules as Close tab, Yes, delete worktree: it must be clean, unlocked
  and not the main checkout, with no active/uncertain checking use. The branch
  stays, ignored files go with the checkout, and an open tab closes. Dimmed 🗑
  cells (main, locked) only explain why that checkout cannot be deleted.

RESTORE
  Crit checkpoints workspace changes and exact OMP/OpenCode chat references in
  $XDG_STATE_HOME/crit/workspace.json (default ~/.local/state/crit/workspace.json).
  Metadata is owner-private; chat contents stay in the agents' own stores.
  All saved agent sessions reopen, including those in background tabs/projects.
  OMP resumes its exact session file; OpenCode resumes its exact session ID.
  An agent with no remembered chat continues its folder's newest one: OMP with
  its own --continue, OpenCode with the newest chat started in that exact folder
  that no other saved pane names (a new chat if there is none).
  A pane whose agent has not drawn yet says what it is starting; a large saved
  OMP chat can take seconds to load before OMP draws, and OMP then repaints its
  last page until the whole chat is in.
  Switching chats inside an agent updates its saved reference. Closing a session
  or tab removes it from the saved workspace, without deleting chat history.
  A shell running an OMP or OpenCode TUI in the foreground at exit reopens as
  that agent, continuing that folder's newest chat; the shell is not restored.
  Shells reopen fresh in their worktrees; running processes, unsent input and
  terminal scrollback are not restored. Docker log followers are not reopened.
  --fresh skips restoration and starts a new saved workspace; use it with
  --repo / --agent to choose a different starting point.
  Saved folders, worktrees, chats or agents that cannot be reopened are left
  out and listed in a recovery dialog; everything else reopens and saving continues.
  The file as it was is kept as workspace.json.bak next to workspace.json.
  An unreadable or invalid snapshot is left untouched and saving is disabled
  in that window; fix it, or use --fresh to replace it.
  Projects whose folder paths are not valid UTF-8 are not saved.
  Only one Crit window owns the snapshot. Additional windows do not restore or
  overwrite it while the owner is running, and display that limitation.

CHECKING RUNTIME
  Opening, creating or restoring a worktree never starts containers.
  Required Git-ignored local Compose configuration, .env and service env_file
  inputs are copied from the source as private files; existing targets remain.
  One project-owned Compose runtime serves exclusive FIFO checking turns.
  `crit check acquire` waits for healthy services; `request` queues a manual turn.
  Preview follows the active owner, not the focused tab. `status` shows owner,
  queue, endpoints and errors without starting a coordinator or containers.
  `crit check exec -- PROGRAM ARGS` streams an owned host command in that checkout.
  Release in finally: `crit check release`. `cancel` also removes queued requests.
  `restart` rebuilds the current owner's turn without yielding its position.
  Agent turn completion releases automatic turns; manual requests stay pinned
  until release or pane/window closure. Owner death and cancellation clean up
  before the next grant. Drained queues stop containers and coordinator workers;
  named data volumes remain. Restoration never revives checking turns.
  Native OMP/OpenCode tools expose crit_check actions acquire/release/status.
  A turn observes the checkout's HEAD; it does not freeze mutable files, mark
  tests passed, commit, merge, reset main, or create another validation checkout.
  Existing unmanaged/legacy stacks block startup; Crit never silently stops them.
  External networks/volumes, host networking and shared writable bind mounts
  are refused. Dynamic host ports bind loopback only.
  If all coordinators are externally killed, status reports blocked, not stopped.
  The next acquire/request reconciles owned resources under the lifecycle lock
  before granting another turn; uncertain cleanup never permits overlap.

  Optional crit.toml:
    [checks]
    cpus = 2.0
    memory_mib = 2048
    [compose]
    files = [\"compose.yaml\"]
    [compose.links.web]
    API_URL = \"api:8000\"
  Default aggregate running-service budget: half available CPUs, clamped to
  1–4 CPUs, and half physical RAM, clamped to 1024–8192 MiB. Overrides accept
  cpus 0.1–64 and memory_mib 128–262144. Stricter service limits remain.
  Docker build-daemon resource use is not capped by this budget.
  Links inject the dependency's http://127.0.0.1:PORT into runtime environment;
  build-time frontend URLs must be configured in the app/build separately.

LIFECYCLE

Ctrl+Shift+Q offers Yes (close only), Yes, delete worktree, and No.
Use Up/Down and Enter; No is selected by default. Esc cancels.
Close-only retains the checkout and releases that pane's checking requests.
Explicit deletion removes only a clean, unlocked, non-main checkout; its branch
stays. Active or uncertain runtime ownership blocks deletion. Git-ignored files
inside the checkout are deleted with it; named Docker volumes are kept.
Ordinary folders are never deleted. Deletion shows progress; Esc hides it.
A refusal leaves the tab and checkout intact and shows an error dialog.
Crit owns its pane and checking command descendants and stops them on close.
The next launch restores saved workspace and supported chats, not old processes
or checking turns. Agent hooks are launch-scoped, not global plugins.
";

fn main() {
    let result = run();
    // Dropped sessions are killed by threads that must finish before Crit exits.
    terminal::join_reapers();
    if let Err(error) = result {
        eprintln!("crit: {error:#}");
        std::process::exit(1);
    }
}

fn utf8(option: &str, value: OsString) -> Result<String> {
    value
        .into_string()
        .map_err(|value| anyhow!("{option} must be UTF-8: {}", value.to_string_lossy()))
}

/// The agents' share of the CPU that `--agent-cpu` takes: a whole percentage from 1 to 100,
/// with or without its percent sign.
fn agent_cpu_percent(value: OsString) -> Result<u8> {
    let value = utf8("--agent-cpu", value)?;
    match value.strip_suffix('%').unwrap_or(&value).parse::<u8>() {
        Ok(percent @ 1..=100) => Ok(percent),
        _ => bail!("--agent-cpu takes a whole percentage from 1 to 100, not {value:?}"),
    }
}

fn run() -> Result<()> {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    match raw.first().and_then(|arg| arg.to_str()) {
        Some("__checks") => return checks::internal(&raw[1..]),
        Some("__pane") => return terminal::supervise(&raw[1..]),
        Some("check") => {
            return checks::cli(&raw[1..], &std::env::current_dir().unwrap_or_default());
        }
        _ => {}
    }
    let mut args = std::env::args_os().skip(1);
    let mut positionals: Vec<OsString> = Vec::new();
    let mut repo = None;
    let mut agent = String::from("omp");
    let mut base = None;
    let mut use_kitty = true;
    let mut direct_start = false;
    let mut fresh = false;
    let mut agent_cpu = None;
    let mut all = false;
    while let Some(arg) = args.next() {
        // A non-UTF-8 argument cannot be an option, so it is a positional (a path).
        match arg.to_str() {
            Some("--help" | "-h") => {
                print!("{HELP}");
                return Ok(());
            }
            Some("--version" | "-V") => {
                println!("crit {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some("--repo") => {
                repo = Some(PathBuf::from(
                    args.next().context("--repo requires a path")?,
                ));
                direct_start = true;
            }
            Some("--agent") => {
                direct_start = true;
                agent = utf8(
                    "--agent",
                    args.next()
                        .context("--agent requires omp, opencode, or shell")?,
                )?
            }
            Some("--base") => {
                base = Some(utf8(
                    "--base",
                    args.next().context("--base requires a Git reference")?,
                )?)
            }
            Some("--agent-cpu") => {
                agent_cpu = Some(agent_cpu_percent(
                    args.next()
                        .context("--agent-cpu requires a percentage from 1 to 100")?,
                )?)
            }
            Some("--fresh") => fresh = true,
            Some("--all") => all = true,
            Some("--no-kitty") => use_kitty = false,
            Some(option) if option.starts_with('-') => {
                bail!("unknown option {option}; use --help")
            }
            _ => positionals.push(arg),
        }
    }
    // The launch folder is only needed by commands that use it, and not at all when --repo
    // names one: a deleted current folder breaks neither `crit --help` nor `crit keys`.
    let repo = || match &repo {
        Some(repo) => Ok(repo.clone()),
        None => std::env::current_dir().context(
            "the current folder no longer exists; cd to an existing folder or pass --repo PATH",
        ),
    };
    if !matches!(agent.as_str(), "omp" | "opencode" | "shell") {
        bail!("unsupported agent {agent:?}; choose omp, opencode, or shell");
    }
    if fresh && !positionals.is_empty() {
        bail!("--fresh is only available when opening Crit");
    }
    if agent_cpu.is_some() && !positionals.is_empty() {
        bail!("--agent-cpu is only available when opening Crit");
    }
    if all
        && !(positionals
            .first()
            .is_some_and(|command| command.to_str() == Some("sandbox"))
            && positionals
                .get(1)
                .is_some_and(|operation| operation.to_str() == Some("clean")))
    {
        bail!("--all is only available with crit sandbox clean");
    }
    match positionals
        .first()
        .map(|command| command.to_str().unwrap_or_default())
    {
        None => app::run(&repo()?, &agent, use_kitty, direct_start, fresh, agent_cpu),
        Some("keys") if positionals.len() == 1 => {
            print!("{}", keys::HELP);
            Ok(())
        }
        Some("setup") if positionals.len() == 2 => {
            match positionals[1].to_str() {
                Some("kitty") => println!(
                    "Installed {}. Reload Kitty config with Ctrl+Shift+F5.",
                    kitty::install()?.display()
                ),
                Some("agents") => {
                    integration::install()?;
                    println!("Installed launch-scoped OMP/OpenCode hooks.");
                }
                Some("all") => {
                    integration::install()?;
                    println!(
                        "Installed agent hooks and {}. Reload Kitty config with Ctrl+Shift+F5.",
                        kitty::install()?.display()
                    );
                }
                _ => bail!("use crit setup kitty|agents|all"),
            }
            Ok(())
        }
        Some("worktree") => {
            let repo = repo()?;
            let repository = worktree::discover(&repo)?;
            match positionals.get(1).and_then(|word| word.to_str()) {
                Some("list") if positionals.len() == 2 => {
                    for tree in worktree::list(&repository)? {
                        println!("{}\t{}\t{}", tree.id, tree.branch, tree.path.display());
                    }
                }
                Some("create") if positionals.len() == 3 => {
                    let branch = positionals[2]
                        .to_str()
                        .context("branch name must be UTF-8")?;
                    let tree = worktree::create(&repository, branch, base.as_deref())?;
                    println!("{}", tree.path.display());
                    let source = selected_tree(&repository, &repo)?;
                    compose::prepare_local_files(&source, &tree).with_context(|| {
                        format!(
                            "worktree was created at {}; local file preparation failed: {}",
                            tree.path.display(),
                            source.path.display()
                        )
                    })?;
                }
                Some("remove") if positionals.len() == 3 => {
                    // Resolved as the listing resolves it, so a checkout deleted outside Git
                    // (which the UI's 🗑 can remove) is still found by its registration.
                    let path = worktree::canonical_path(Path::new(&positionals[2]))?;
                    let tree = worktree::list(&repository)?
                        .into_iter()
                        .find(|t| t.path == path)
                        .context("path is not a worktree in this repository")?;
                    let _checks_guard = checks::removal_guard(&tree.path)?;
                    worktree::remove(&repository, &tree)?;
                    println!("Removed checkout {}; branch retained.", path.display());
                    // Its scratch has nothing left to serve. One that stays only warns: the
                    // checkout itself is gone.
                    if let Ok(sandbox) = sandbox::Sandbox::locate()
                        && let Err(error) = sandbox.remove_workspace(&path)
                    {
                        eprintln!(
                            "crit: its scratch in {} could not be removed: {error}",
                            sandbox.root().display()
                        );
                    }
                }
                _ => bail!("use crit worktree list|create BRANCH|remove PATH [--repo PATH]"),
            }
            Ok(())
        }
        Some("sandbox") => {
            match positionals.get(1).map(|operation| operation.to_str()) {
                None => print!("{}", sandbox::status()),
                Some(Some("status")) if positionals.len() == 2 => print!("{}", sandbox::status()),
                Some(Some("clean")) if positionals.len() == 2 => sandbox::clean_command(all)?,
                _ => bail!("use crit sandbox [status|clean [--all]]"),
            }
            Ok(())
        }
        _ => bail!("unknown command; use crit --help"),
    }
}

fn selected_tree(repository: &worktree::Repository, path: &Path) -> Result<worktree::Worktree> {
    let path = path.canonicalize()?;
    worktree::list(repository)?
        .into_iter()
        .filter(|tree| path.starts_with(&tree.path))
        .max_by_key(|tree| tree.path.components().count())
        .context("directory is not inside a checked-out worktree")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_cpu_takes_a_whole_percentage_from_1_to_100() {
        for (value, percent) in [("50", 50), ("1", 1), ("100", 100), ("25%", 25)] {
            assert_eq!(agent_cpu_percent(value.into()).unwrap(), percent);
        }
        for value in ["0", "101", "256", "-5", "50.5", "half", "", "%", "50%%"] {
            let error = agent_cpu_percent(value.into()).unwrap_err().to_string();
            assert!(error.contains("1 to 100"), "{value:?}: {error}");
        }
        use std::os::unix::ffi::OsStringExt;
        let error = agent_cpu_percent(OsString::from_vec(b"5\xff".to_vec())).unwrap_err();
        assert!(error.to_string().contains("must be UTF-8"), "{error}");
    }
}
