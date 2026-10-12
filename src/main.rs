mod app;
mod clipboard;
mod compose;
mod event;
mod integration;
mod intent;
mod keys;
mod kitty;
mod restore;
mod selection;
mod terminal;
mod ui;
mod updates;
mod worktree;

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const HELP: &str = "crit — worktree-first terminals for OMP and OpenCode

USAGE
  crit [--fresh] [--repo PATH] [--agent omp|opencode|shell] [--no-kitty]
  crit worktree list [--repo PATH]
  crit worktree create BRANCH [--base REF] [--repo PATH]
  crit worktree remove PATH [--repo PATH]
  crit env up|down|restart|status|logs [--repo PATH]
  crit env up|restart --source PATH [--repo PATH]
  crit setup kitty|agents|all
  crit keys

Crit restores your saved workspace on startup. With no saved workspace, choose
New terminal or any folder. F1 opens Ask Crit. Ctrl+D in an agent opens its
workspace shell without stopping it;
Ctrl+D in a shell keeps normal EOF behavior, and native mode passes it through.
Ctrl+Shift+T opens a new tab: any folder (no Git required), a worktree, or an
open project. Ctrl+Shift+, or . splits: name a branch and its worktree opens in
the second pane. Ctrl+Shift+Enter opens a session. F12 persistently toggles
Crit/native Kitty shortcuts. Run `crit setup kitty` once to enable the
window-scoped mappings.

ASK CRIT
  F1 opens a natural-language launcher using anthropic/claude-haiku-5-5,
  through your authenticated OMP installation. It is separate from the
  coding agents: no tools, extensions, skills, rules, or persistent session.
  Examples: \"Start a branch for login fixes in a separate worktree\",
  \"Open portfolio with OpenCode\", or \"Show this workspace's logs\".
  Vague requests ask a follow-up; answer in the same box. New branches use
  separate worktrees, based on the focused checkout's HEAD unless specified.
  Enter sends; Esc cancels and stops the interpreter; Ctrl+U clears input.
  No model runs while idle. Requests have a 50-second local deadline.
  Crit sends your request history and project/worktree names and paths,
  not source files. Context plus conversation is limited to 64 KiB.
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
  No bottom row is reserved when there is no notice.
  Window shape, rounded corners, borders, shadows, and desktop gaps stay under
  your normal compositor configuration; Crit installs no desktop overrides.
  The minimum interactive size is 30x10. Below it, typing, paste, mouse input,
  and confirmations pause; Esc and F12 remain available. Enlarge to resume.
  Notices occupy reserved rows, never terminal output. Long notices show
  their final lines with an overflow marker. Selected sessions stay visible
  in the workspace header even when other session labels overflow.
  A thin scrollbar uses the right gutter when normal-screen history exists.
  Click or drag it to review history or return to live output at the bottom.
  Mouse-aware apps receive the wheel normally; Shift+wheel scrolls Crit's
  history instead. Typing or pasting returns to live output.

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

UPDATES
  Each interactive startup runs omp update and opencode upgrade before
  launching agents. A blue scan animation shows each tool waiting, checking,
  or ready; updater banners stay out of the UI. Checks finish without an
  artificial delay. Ctrl+C cancels startup and stops the updater process group.
  Input typed while checking is kept for the picker or terminal.
  If an update fails, Crit shows its diagnostic and continues with the
  installed version.

PROJECTS
  Run crit from any folder, including a Git checkout, to choose New terminal
  or a project. New terminal starts your shell in the launch folder; it needs
  no Git repository and does not create one. It is the first, selected row.
  Explicit --repo or --agent can open any existing folder directly, with the
  requested agent or shell. No Git initialization is required, including for research.
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
  OMP, OpenCode, or shells. New tabs default to Open folder; enter ~ for home.
  Opening a Git folder opens only its checkout, not every branch/worktree.
  Ask to open an existing worktree or switch to a named project.
  Already open projects and shell/agent sessions stay alive when switching.
  An ordinary folder workspace can open Git projects later through F1 / F2.
  Worktree and Docker actions require opening a Git project first.
  New worktrees use the focused checkout's current HEAD, not another project.
  Esc cancels pending project opens and worktree listings; late results
  cannot reopen canceled dialogs or replace newer ones. Failed worktree creation
  restores its input only while its original workspace is focused and no newer
  dialog is open.
  Returning a split to full width keeps the focused worktree.
  Saved projects, worktrees, splits, focused panes and selected sessions reopen.

RESTORE
  Crit checkpoints workspace changes and exact OMP/OpenCode chat references in
  $XDG_STATE_HOME/crit/workspace.json (default ~/.local/state/crit/workspace.json).
  Metadata is owner-private; chat contents stay in the agents' own stores.
  All saved agent sessions reopen, including those in background tabs/projects.
  OMP resumes its exact session file; OpenCode resumes its exact session ID.
  Switching chats inside an agent updates its saved reference. Closing a session
  or tab removes it from the saved workspace, without deleting chat history.
  Shells reopen fresh in their worktrees; running processes, unsent input and
  terminal scrollback are not restored. Docker log followers are not reopened.
  --fresh skips restoration and starts a new saved workspace; use it with
  --repo / --agent to choose a different starting point.
  Missing worktrees/chats or invalid state show an error and leave the original
  snapshot untouched; saving is disabled in that window. Fix the cause and
  restart to retry, or explicitly use --fresh to replace the saved workspace.
  Only one Crit window owns the snapshot. Additional windows do not restore or
  overwrite it while the owner is running, and display that limitation.

DOCKER
  New worktrees automatically start their Compose stack when configured.
  Required Git-ignored local Compose configuration, .env and service env_file
  inputs are copied from the source checkout as private, independent files.
  Existing target files are preserved; shared links and path escapes are refused.
  A preparation failure leaves the new checkout usable. Fix its local files and
  start its environment again to retry from the remembered source checkout.
  For a checkout created before this support, explicitly supply its source:
    crit env up --repo /path/to/worktree --source /path/to/source-checkout
  Each worktree gets its own project, networks, named volumes, build image,
  and dynamically allocated loopback host ports. Ask \"open the preview\".
  External networks/volumes and host networking are refused. Bind mounts
  retain their configured paths: use worktree-relative paths or named volumes,
  not a shared absolute writable directory, for independent data.

  Optional crit.toml in the repository:
    [compose]
    files = [\"compose.yaml\"]
    [compose.links.web]
    API_URL = \"api:8000\"
  This injects that worktree's http://127.0.0.1:PORT into web's runtime
  environment after api starts. Use your app's actual variable name.
  Build-time frontend URLs must be configured in the app/build separately.

LIFECYCLE

Closing a tab never deletes its Git checkout. Docker down retains volumes.
Crit owns its agent processes; exiting stops them, not Docker stacks. The next
launch restores the saved workspace and supported chats, not the old processes.
Agent hooks are launch-scoped, not global plugins. Crit is not a background daemon.
";

fn main() {
    if let Err(error) = run() {
        eprintln!("crit: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut positionals = Vec::new();
    let mut repo = std::env::current_dir()?;
    let mut agent = String::from("omp");
    let mut base = None;
    let mut use_kitty = true;
    let mut direct_start = false;
    let mut fresh = false;
    let mut source = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("crit {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--repo" => {
                repo = PathBuf::from(args.next().context("--repo requires a path")?);
                direct_start = true;
            }
            "--agent" => {
                direct_start = true;
                agent = args
                    .next()
                    .context("--agent requires omp, opencode, or shell")?
            }
            "--base" => base = Some(args.next().context("--base requires a Git reference")?),
            "--fresh" => fresh = true,
            "--source" => {
                source = Some(PathBuf::from(
                    args.next().context("--source requires a checkout path")?,
                ))
            }
            "--no-kitty" => use_kitty = false,
            _ if arg.starts_with('-') => bail!("unknown option {arg}; use --help"),
            _ => positionals.push(arg),
        }
    }
    if !matches!(agent.as_str(), "omp" | "opencode" | "shell") {
        bail!("unsupported agent {agent:?}; choose omp, opencode, or shell");
    }
    if fresh && !positionals.is_empty() {
        bail!("--fresh is only available when opening Crit");
    }
    if source.is_some()
        && !(positionals.first().is_some_and(|command| command == "env")
            && positionals
                .get(1)
                .is_some_and(|operation| matches!(operation.as_str(), "up" | "restart")))
    {
        bail!("--source is only available with crit env up|restart");
    }
    match positionals.first().map(String::as_str) {
        None => app::run(&repo, &agent, use_kitty, direct_start, fresh),
        Some("keys") if positionals.len() == 1 => {
            print!("{}", keys::HELP);
            Ok(())
        }
        Some("setup") if positionals.len() == 2 => {
            match positionals[1].as_str() {
                "kitty" => println!(
                    "Installed {}. Reload Kitty config with Ctrl+Shift+F5.",
                    kitty::install()?.display()
                ),
                "agents" => {
                    integration::install()?;
                    println!("Installed launch-scoped OMP/OpenCode hooks.");
                }
                "all" => {
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
            let repository = worktree::discover(&repo)?;
            match positionals.get(1).map(String::as_str) {
                Some("list") if positionals.len() == 2 => {
                    for tree in worktree::list(&repository)? {
                        println!("{}\t{}\t{}", tree.id, tree.branch, tree.path.display());
                    }
                }
                Some("create") if positionals.len() == 3 => {
                    let tree = worktree::create(&repository, &positionals[2], base.as_deref())?;
                    println!("{}", tree.path.display());
                    let source = selected_tree(&repository, &repo)?;
                    compose::prepare_local_files(&source, &tree).with_context(|| {
                        format!("worktree was created at {}; local environment preparation failed (retry with crit env up --repo '{}' --source '{}')",
                            tree.path.display(), tree.path.display(), source.path.display())
                    })?;
                }
                Some("remove") if positionals.len() == 3 => {
                    let path = Path::new(&positionals[2]).canonicalize()?;
                    let tree = worktree::list(&repository)?
                        .into_iter()
                        .find(|t| t.path == path)
                        .context("path is not a worktree in this repository")?;
                    worktree::remove(&repository, &tree)?;
                    println!("Removed checkout {}; branch retained.", path.display());
                }
                _ => bail!("use crit worktree list|create BRANCH|remove PATH [--repo PATH]"),
            }
            Ok(())
        }
        Some("env") if positionals.len() == 2 => {
            let repository = worktree::discover(&repo)?;
            let tree = selected_tree(&repository, &repo)?;
            if let Some(source) = source {
                let source_repository = worktree::discover(&source)?;
                let source = selected_tree(&source_repository, &source)?;
                compose::prepare_local_files(&source, &tree)?;
            }
            let environment = match positionals[1].as_str() {
                "up" => compose::up(&tree)?,
                "down" => compose::down(&tree)?,
                "restart" => compose::restart(&tree)?,
                "status" => compose::status(&tree)?,
                "logs" => {
                    let status = std::process::Command::new("docker")
                        .args(compose::logs_args(&tree)?)
                        .status()?;
                    if !status.success() {
                        bail!("docker logs exited with {status}");
                    }
                    return Ok(());
                }
                _ => bail!("use crit env up|down|restart|status|logs [--repo PATH]"),
            };
            println!("{}", serde_json::to_string_pretty(&environment)?);
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
