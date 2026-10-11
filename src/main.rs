mod app;
mod compose;
mod event;
mod integration;
mod intent;
mod keys;
mod kitty;
mod terminal;
mod ui;
mod updates;
mod worktree;

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const HELP: &str = "crit — worktree-first terminals for OMP and OpenCode

USAGE
  crit [--repo PATH] [--agent omp|opencode|shell] [--no-kitty]
  crit worktree list [--repo PATH]
  crit worktree create BRANCH [--base REF] [--repo PATH]
  crit worktree remove PATH [--repo PATH]
  crit env up|down|restart|status|logs [--repo PATH]
  crit setup kitty|agents|all
  crit keys

Start with New terminal or choose a Git project. Press F1 and describe what
you want. Ctrl+D in an agent opens its workspace shell without stopping it;
Ctrl+D in a shell keeps normal EOF behavior, and native mode passes it through.
Ctrl+Shift+T creates a worktree; Ctrl+Shift+Enter opens a session. F12
persistently toggles Crit/native Kitty shortcuts. Run `crit setup kitty` once
to enable the window-scoped mappings.

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
  Crit uses the terminal's default background, including its tabs, dialogs,
  and status strip, so leftover pixels at the window edges blend into the UI.
  Embedded agents querying the background receive this Kitty window's current
  color when its remote-control socket is available.
  Crit removes Kitty padding/margins only in its own terminal and restores
  configured spacing on exit. Other terminals are unaffected.
  Window shape, rounded corners, borders, shadows, and desktop gaps stay under
  your normal compositor configuration; Crit installs no desktop overrides.

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
  Explicit --repo or --agent opens that Git project directly when possible;
  --agent shell retains direct shell startup in the selected checkout.
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
  Absolute, ~/, ./ and ../ paths open directly. Other relative folders open
  when no detected project matches; paths use the displayed base folder.
  Opening a folder opens only its checkout, not every branch/worktree.
  Ask to open an existing worktree or switch to a named project.
  Already open projects and shell/agent sessions stay alive when switching.
  A terminal-only workspace can open Git projects later through F1 / F2.
  Worktree and Docker actions require opening a Git project first.
  New worktrees use the focused checkout's current HEAD, not another project.
  Project/session state lasts until Crit exits; this is not a persistent daemon.

DOCKER
  New worktrees automatically start their Compose stack when configured.
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
Crit owns its agent processes; exiting closes all open projects' sessions,
not Docker stacks. Agent state hooks are launch-scoped, not global plugins.
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
            "--no-kitty" => use_kitty = false,
            _ if arg.starts_with('-') => bail!("unknown option {arg}; use --help"),
            _ => positionals.push(arg),
        }
    }
    if !matches!(agent.as_str(), "omp" | "opencode" | "shell") {
        bail!("unsupported agent {agent:?}; choose omp, opencode, or shell");
    }
    match positionals.first().map(String::as_str) {
        None => app::run(&repo, &agent, use_kitty, direct_start),
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
