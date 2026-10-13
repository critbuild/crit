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

use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsString;
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
Ctrl+Shift+T then Enter opens a new home tab. The menu also offers other
folders and worktrees; switch projects from the tab bar, F1, or F2.
Ctrl+Shift+, or . opens a split picker:
New worktree creates a separate checkout; Split terminal opens another session
in the same folder. Existing branch/worktree opens a destination picker.
Ctrl+Shift+Enter opens a session.
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
  No bottom row is reserved when there is no notice.
  Window shape, rounded corners, borders, shadows, and desktop gaps stay under
  your normal compositor configuration; Crit installs no desktop overrides.
  The minimum interactive size is 30x10. Below it, typing, paste, mouse input,
  and confirmations pause; Esc and F12 remain available. Enlarge to resume.
  Notices occupy reserved rows, never terminal output. Long notices show
  their final lines with an overflow marker. Notices clear on the next
  keystroke, paste, or click. Warnings that saving is off, or that saved
  items were left out at startup, stay until Esc or a Crit shortcut.
  Selected sessions stay visible in the workspace header even when other
  session labels overflow.
  A thin scrollbar uses the right gutter when normal-screen history exists.
  Click or drag it to review history or return to live output at the bottom.
  Mouse-aware apps receive the wheel normally; Shift+wheel scrolls Crit's
  history instead. Typing or pasting returns to live output.
  Side-by-side panes use adjacent edge rails, with ⟨ on the left and ⟩ on
  the right, and no blank column between them. Drag either arrow without changing focus.
  Each pane keeps a usable minimum width; the proportion follows window resizing
  and is saved per project. Narrow windows show one pane until both fit again.

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
  Each interactive startup runs omp update and opencode upgrade together,
  before launching agents. A blue scan animation shows each tool checking,
  ready, skipped, or not installed; updater banners stay out of the UI.
  Checks finish without an artificial delay. Esc skips updates still running;
  Ctrl+C cancels startup. Either stops the updater process groups.
  Input typed while checking is kept for the picker or terminal.
  If an update fails, Crit shows its diagnostic and continues with the
  installed version. A tool that is not installed is not a failure.

PROJECTS
  Run crit from any folder, including a Git checkout, to choose New terminal
  or a project. New terminal starts your shell in the launch folder; it needs
  no Git repository and does not create one. It is the first, selected row.
  Explicit --repo or --agent can open any existing folder directly, with the
  requested agent or shell. With a saved workspace, that folder opens as an
  extra tab after the restore. No Git initialization is required, including
  for research.
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
  The split picker offers New worktree, Split terminal, and Existing branch/worktree.
  New worktree asks for a branch name and leaves the first pane's branch alone.
  Split terminal starts an independent session of the current agent/shell kind
  in the same folder, without creating a branch, worktree, or Docker environment.
  It also works in non-Git folders. Mouse clicks or Ctrl+Shift+[ / ] switch panes.
  Ctrl+Shift+W closes the focused session; its peer stays running.
  Existing branch/worktree lists other registered checkouts and local branches
  without checkouts. Up/Down selects; Enter opens; Esc returns to the split menu.
  Existing checkouts keep their files and sessions. An unchecked-out branch gets
  its own worktree at that branch's commit. The original pane keeps its branch,
  session, and focus. Esc hides the opening popup; the checkout then opens as a tab.
  Returning a split to full width keeps the focused session. Saved terminal
  splits reopen both sessions in the same workspace, with their focus and ratio.
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
  Saved folders, worktrees, chats or agents that cannot be reopened are left
  out and listed in a notice; everything else reopens and saving continues.
  The file as it was is kept as workspace.json.bak next to workspace.json.
  An unreadable or invalid snapshot is left untouched and saving is disabled
  in that window; fix it, or use --fresh to replace it.
  Projects whose folder paths are not valid UTF-8 are not saved.
  Only one Crit window owns the snapshot. Additional windows do not restore or
  overwrite it while the owner is running, and display that limitation.

DOCKER
  New worktrees automatically start their Compose stack when configured.
  Required Git-ignored local Compose configuration, .env and service env_file
  inputs are copied from the source checkout as private, independent files.
  Existing target files are preserved; shared links and path escapes are refused.
  A preparation failure leaves the new checkout usable. Fix its local files and
  start its environment again to retry from the remembered source checkout.
  If that checkout no longer exists, Crit forgets it and uses the files already
  in place.
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

Ctrl+Shift+Q offers Yes (close only), Yes, delete worktree, and No.
Use Up/Down and Enter; No is selected by default. Esc cancels.
Close-only retains the checkout and Docker stack. Explicit deletion closes the
tab only after removing a clean, unlocked, non-main worktree; its branch stays.
Git-ignored files inside it (build output, .env, bind-mounted data) are deleted
with it; named Docker volumes are kept. Directories you cannot write, often
created by containers, block deletion instead of leaving a half-deleted checkout.
Stop its Docker environment first. If Docker cannot be reached, the checkout is
still removed and the notice names the stack to clean up. Ordinary folders are
never deleted.
Deletion shows a progress popup; Esc hides it and the result arrives as a notice.
A refusal stays visible with its reason and leaves the tab and checkout intact;
Enter returns to the confirmation and Esc dismisses the error. Docker state is
checked again at deletion, so stopping the stack outside Crit does not require
reopening the tab. A checkout whose environment Crit never started needs no Docker.
Docker down retains volumes.
Crit owns its agent processes; exiting stops them, not Docker stacks. The next
launch restores the saved workspace and supported chats, not the old processes.
Agent hooks are launch-scoped, not global plugins. Crit is not a background daemon.
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

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mut positionals: Vec<OsString> = Vec::new();
    let mut repo = None;
    let mut agent = String::from("omp");
    let mut base = None;
    let mut use_kitty = true;
    let mut direct_start = false;
    let mut fresh = false;
    let mut source = None;
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
            Some("--fresh") => fresh = true,
            Some("--source") => {
                source = Some(PathBuf::from(
                    args.next().context("--source requires a checkout path")?,
                ))
            }
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
    if source.is_some()
        && !(positionals
            .first()
            .is_some_and(|command| command.to_str() == Some("env"))
            && positionals
                .get(1)
                .is_some_and(|operation| matches!(operation.to_str(), Some("up" | "restart"))))
    {
        bail!("--source is only available with crit env up|restart");
    }
    match positionals
        .first()
        .map(|command| command.to_str().unwrap_or_default())
    {
        None => app::run(&repo()?, &agent, use_kitty, direct_start, fresh),
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
                    // The same guard as deleting from the UI: a running stack blocks it, and
                    // Docker being unreachable only warns.
                    match compose::existing_status(&tree) {
                        Ok(Some(environment)) if !environment.services.is_empty() => bail!(
                            "stop this worktree's Docker environment first: crit env down --repo {}",
                            tree.path.display()
                        ),
                        Ok(_) => {}
                        Err(error) => {
                            let reason = format!("{error:#}");
                            eprintln!(
                                "crit: Docker could not be checked ({}); if this worktree's stack still exists, run: docker compose -p crit-{} down",
                                reason.trim_end(),
                                tree.id
                            );
                        }
                    }
                    worktree::remove(&repository, &tree)?;
                    println!("Removed checkout {}; branch retained.", path.display());
                }
                _ => bail!("use crit worktree list|create BRANCH|remove PATH [--repo PATH]"),
            }
            Ok(())
        }
        Some("env") if positionals.len() == 2 => {
            let repo = repo()?;
            let repository = worktree::discover(&repo)?;
            let tree = selected_tree(&repository, &repo)?;
            if let Some(source) = source {
                let source_repository = worktree::discover(&source)?;
                let source = selected_tree(&source_repository, &source)?;
                compose::prepare_local_files(&source, &tree)?;
            }
            let environment = match positionals[1].to_str() {
                Some("up") => compose::up(&tree)?,
                Some("down") => compose::down(&tree)?,
                Some("restart") => compose::restart(&tree)?,
                Some("status") => compose::status(&tree)?,
                Some("logs") => {
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
