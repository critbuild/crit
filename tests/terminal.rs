use portable_pty::{CommandBuilder, PtySize};
use std::{
    io::{Read, Write},
    process::Command,
    sync::mpsc,
    time::{Duration, Instant},
};

struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn drive_until(
    writer: &mut impl Write,
    rx: &mpsc::Receiver<Vec<u8>>,
    screen: &mut vt100::Parser,
    input: &[u8],
    ready: impl Fn(&str) -> bool,
) {
    writer.write_all(input).unwrap();
    writer.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if ready(&screen.screen().contents()) {
            return;
        }
        let bytes = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!(
                    "terminal interaction did not complete: {error}\n{}",
                    screen.screen().contents()
                )
            });
        screen.process(&bytes);
    }
}

fn agent_picker(contents: &str) -> bool {
    ["OMP", "OpenCode", "Terminal"]
        .iter()
        .all(|program| contents.contains(program))
}

fn git_output(cwd: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn interactive_startup_offers_terminal_and_preserves_explicit_shell_project_selection() {
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("project with spaces");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("tracked.txt"), "committed contents\n").unwrap();
    for args in [
        vec!["init", "--initial-branch=main"],
        vec!["add", "tracked.txt"],
        vec![
            "-c",
            "user.name=Crit Test",
            "-c",
            "user.email=test@localhost",
            "commit",
            "--allow-empty",
            "-m",
            "test",
        ],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let linked = home.path().join("linked checkout");
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "add", "-b", "feature/other"])
        .arg(&linked)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let subfolder = linked.join("nested");
    std::fs::create_dir(&subfolder).unwrap();
    std::fs::write(linked.join("tracked.txt"), "dirty linked contents\n").unwrap();
    std::fs::write(linked.join("untracked.txt"), "untracked linked contents\n").unwrap();
    let original_head = git_output(&repo, &["rev-parse", "HEAD"]);
    let sandbox = home.path().join("sandbox");

    // Never run the user's real agent updaters from a test.
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    std::os::unix::fs::symlink(git, bin.join("git")).unwrap();
    for (cwd, checkout, branch, unopened_branch, direct_start) in [
        (
            repo.as_path(),
            repo.as_path(),
            "main",
            "feature/other",
            true,
        ),
        (
            subfolder.as_path(),
            linked.as_path(),
            "feature/other",
            "main",
            true,
        ),
        (home.path(), home.path(), "terminal", "main", true),
        (repo.as_path(), repo.as_path(), "terminal", "main", false),
        (
            subfolder.as_path(),
            subfolder.as_path(),
            "terminal",
            "feature/other",
            false,
        ),
        (home.path(), home.path(), "terminal", "main", false),
    ] {
        let had_git_metadata = cwd.join(".git").exists();
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_crit"));
        command.arg("--no-kitty");
        command.arg("--fresh");
        if direct_start {
            command.args(["--agent", "shell"]);
        }
        command.cwd(cwd);
        command.env("SHELL", "/bin/sh");
        command.env("TERM", "xterm-256color");
        command.env("HOME", home.path());
        command.env("PATH", &bin);
        command.env("PS1", "CRIT_TEST> ");
        command.env("XDG_RUNTIME_DIR", home.path());
        // Scratch must land in the throwaway home, never in the user's sandbox.
        command.env("CRIT_SANDBOX", &sandbox);
        let mut child = ChildGuard(pair.slave.spawn_command(command).unwrap());
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = [0; 8192];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 || tx.send(bytes[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut screen = vt100::Parser::new(24, 100, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        // Wait for raw-mode startup, so the outer PTY does not translate CR to LF.
        while !screen.screen().alternate_screen() {
            let bytes = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("Crit did not enter the terminal UI");
            screen.process(&bytes);
        }
        if !direct_start {
            while !screen.screen().contents().contains("New terminal") {
                let bytes = rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("default startup did not offer New terminal");
                screen.process(&bytes);
            }
            writer.write_all(b"\r").unwrap();
        }
        let expected = format!(
            "runtime-ready:{}",
            checkout.canonicalize().unwrap().display()
        );
        // Every pane's temp folder is its workspace's directory in the sandbox.
        let scratch = format!(
            "case \"$TMPDIR\" in '{}'/*) [ -d \"$TMPDIR\" ] && [ \"$TMP\" = \"$TMPDIR\" ] && [ \"$TEMP\" = \"$TMPDIR\" ] && printf 'scratch-%s\\n' sandboxed;; esac\r",
            sandbox.display()
        );
        for (input, marker) in [
            ("", "CRIT_TEST>"),
            (
                "printf 'runtime-%s:%s\\n' ready \"$PWD\"\r",
                expected.as_str(),
            ),
            (scratch.as_str(), "scratch-sandboxed"),
        ] {
            writer.write_all(input.as_bytes()).unwrap();
            writer.flush().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            // The actual cwd sentinel is not present in the echoed command.
            while !screen.screen().contents().contains(marker) {
                let bytes = rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(|error| {
                        panic!(
                            "Crit startup in {} did not reach {marker:?}: {error}\n{}",
                            cwd.display(),
                            screen.screen().contents(),
                        )
                    });
                screen.process(&bytes);
            }
        }
        let contents = screen.screen().contents();
        let tabs = contents.lines().next().unwrap();
        assert!(
            tabs.contains(branch),
            "selected checkout is missing: {tabs}"
        );
        assert!(
            !tabs.contains(unopened_branch),
            "unopened worktree became a tab: {tabs}"
        );
        assert_eq!(
            cwd.join(".git").exists(),
            had_git_metadata,
            "opening a terminal must not create Git metadata",
        );
        let trees_before = git_output(&repo, &["worktree", "list", "--porcelain"]);
        for (split_key, direction) in [
            (b"\x1b[44;6u".as_slice(), "vertical"),
            (b"\x1b[46;6u".as_slice(), "horizontal"),
        ] {
            let command =
                format!("SHARED_FIRST=retained; printf 'split-%s:%s\\n' start '{direction}'\r");
            let ready = format!("split-start:{direction}");
            drive_until(&mut writer, &rx, &mut screen, command.as_bytes(), |s| {
                s.contains(&ready)
            });
            drive_until(&mut writer, &rx, &mut screen, split_key, |s| {
                s.contains("Split terminal")
            });
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[B\r", |s| {
                !s.contains("Split terminal") && s.contains("[shell 2")
            });
            let destination = home.path().join("shared-split-cwd");
            let command = format!(
                "SHARED_SECOND=independent; printf '%s' \"$PWD\" > '{}'; printf 'right-%s:%s\\n' '{direction}' \"$SHARED_SECOND\"\r",
                destination.display()
            );
            let ready = format!("right-{direction}:independent");
            drive_until(&mut writer, &rx, &mut screen, command.as_bytes(), |s| {
                s.contains(&ready)
            });
            assert_eq!(
                std::fs::read_to_string(&destination).unwrap(),
                checkout.canonicalize().unwrap().to_str().unwrap()
            );
            let command = format!(
                "\x1b[93;6uprintf 'left-%s:%s:%s\\n' '{direction}' \"$SHARED_FIRST\" \"${{SHARED_SECOND-unset}}\"\r"
            );
            let ready = format!("left-{direction}:retained:unset");
            drive_until(&mut writer, &rx, &mut screen, command.as_bytes(), |s| {
                s.contains(&ready)
            });
            // Close only the second session; its peer and checkout must survive.
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"\x1b[93;6u\x1b[119;6uy",
                |s| {
                    !s.contains("shell 2")
                        && s.lines().nth(1).is_some_and(|h| h.contains("[shell 1"))
                },
            );
            let command = format!("printf 'kept-%s:%s\\n' '{direction}' \"$SHARED_FIRST\"\r");
            let ready = format!("kept-{direction}:retained");
            drive_until(&mut writer, &rx, &mut screen, command.as_bytes(), |s| {
                s.contains(&ready)
            });
            assert_eq!(
                git_output(&repo, &["worktree", "list", "--porcelain"]),
                trees_before
            );
            assert_eq!(cwd.join(".git").exists(), had_git_metadata);
        }
        if direct_start && cwd == repo {
            // New tab chooses Home, then a program, without needing a path prompt.
            let in_home = |contents: &str| {
                contents
                    .lines()
                    .nth(1)
                    .is_some_and(|header| header.trim_start().starts_with("› terminal"))
            };
            let in_repo = |contents: &str| {
                contents
                    .lines()
                    .nth(1)
                    .is_some_and(|header| header.trim_start().starts_with("› main"))
            };
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[116;6u", |contents| {
                contents.contains("Home (~)")
            });
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[H\r", agent_picker);
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[F\r", in_home);
            let home_marker = format!(
                "home-ready:{}",
                home.path().canonicalize().unwrap().display()
            );
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"HOME_TAB_TOKEN=kept; printf 'home-%s:%s\\n' ready \"$PWD\"\r",
                |contents| contents.contains(&home_marker),
            );
            assert!(!home.path().join(".git").exists());

            // Choosing home again opens another tab with a fresh session; the first tab
            // keeps its state.
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[9;6u", in_repo);
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"\x1b[116;6u\x1b[H\r\x1b[F\r",
                in_home,
            );
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"printf 'home-fresh:%s\\n' \"${HOME_TAB_TOKEN-unset}\"\r",
                |contents| contents.contains("home-fresh:unset"),
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[113;6u", |contents| {
                contents.contains("Yes, delete worktree")
            });
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[H\r", |contents| {
                in_home(contents) && !contents.contains("Yes, delete worktree")
            });
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"printf 'home-resumed:%s\\n' \"$HOME_TAB_TOKEN\"\r",
                |contents| contents.contains("home-resumed:kept"),
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[9;6u", in_repo);

            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"\x1bOP\x1bOQ\x1b[H\x1b[B\x1b[B\x1b[B\rmain\r\x1b[F\r",
                |contents| contents.contains("Enter / Esc dismiss"),
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1b", |contents| {
                !contents.contains("Enter / Esc dismiss")
            });
            drive_until(&mut writer, &rx, &mut screen, b"\x1b", |contents| {
                !contents.contains("new worktree tab")
            });
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"FIRST_PANE_TOKEN=kept; printf 'first-%s\\n' prepared\r",
                |contents| contents.contains("first-prepared"),
            );
            let original_state = home.path().join("original-pane-state");
            let target_state = home.path().join("existing-checkout-state");
            let linked_status = git_output(&linked, &["status", "--porcelain"]);
            let trees_before_existing = git_output(&repo, &["worktree", "list", "--porcelain"]);
            let record_original = format!(
                "printf '%s\\n%s\\n%s\\n' \"$FIRST_PANE_TOKEN\" \"$(git branch --show-current)\" \"$(git rev-parse HEAD)\" > '{}'; printf 'reuse-%s\\n' original\r",
                original_state.display()
            );
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                record_original.as_bytes(),
                |contents| contents.contains("reuse-original"),
            );
            assert_eq!(
                std::fs::read_to_string(&original_state).unwrap(),
                format!("kept\nmain\n{original_head}\n"),
            );

            // F2 opens registered checkouts through real mouse clicks. Clicking an
            // already-open checkout focuses its existing session instead of replacing it.
            assert!(
                !screen
                    .screen()
                    .contents()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .contains("feature/other"),
                "linked checkout must be unopened before F2 is used",
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1bOQ", |contents| {
                contents.contains("feature/other")
            });
            for (opening, (branch, marker, command)) in [
                (
                    "feature/other",
                    "reuse-target",
                    format!(
                        "LINKED_TOKEN=retained; printf '%s\\n%s\\n%s\\n' \"$PWD\" \"$(git branch --show-current)\" \"$(git rev-parse HEAD)\" > '{}'; printf 'reuse-%s\\n' target\r",
                        target_state.display()
                    ),
                ),
                (
                    "main",
                    "original-session:kept",
                    "printf 'original-%s:%s\\n' session \"$FIRST_PANE_TOKEN\"\r".into(),
                ),
                (
                    "feature/other",
                    "linked-session:retained",
                    "printf 'linked-%s:%s\\n' session \"$LINKED_TOKEN\"\r".into(),
                ),
            ]
            .into_iter()
            .enumerate()
            {
                let row = (2..24)
                    .find(|&row| {
                        let label: String = (68..97)
                            .filter_map(|column| screen.screen().cell(row, column))
                            .map(|cell| cell.contents())
                            .collect();
                        label.contains(branch)
                    })
                    .map(|row| row + 1)
                    .expect("registered checkout must have a mouse row in F2");
                let click = format!("\x1b[<0;75;{row}M\x1b[<0;75;{row}m");
                let focused = |contents: &str| {
                    contents.lines().nth(1).is_some_and(|header| {
                        header.trim_start().starts_with(&format!("› {branch}"))
                    })
                };
                if opening == 0 {
                    drive_until(
                        &mut writer,
                        &rx,
                        &mut screen,
                        click.as_bytes(),
                        agent_picker,
                    );
                    drive_until(&mut writer, &rx, &mut screen, b"\x1b[F\r", focused);
                } else {
                    drive_until(
                        &mut writer,
                        &rx,
                        &mut screen,
                        click.as_bytes(),
                        focused,
                    );
                }
                drive_until(
                    &mut writer,
                    &rx,
                    &mut screen,
                    command.as_bytes(),
                    |contents| contents.contains(marker),
                );
            }
            assert_eq!(
                std::fs::read_to_string(&target_state).unwrap(),
                format!("{}\nfeature/other\n{original_head}\n", linked.display()),
            );
            assert_eq!(
                git_output(&repo, &["worktree", "list", "--porcelain"]),
                trees_before_existing,
                "opening or focusing a registered checkout must not create another checkout",
            );
            assert_eq!(
                git_output(&linked, &["status", "--porcelain"]),
                linked_status
            );
            assert_eq!(
                std::fs::read_to_string(linked.join("tracked.txt")).unwrap(),
                "dirty linked contents\n",
            );
            assert_eq!(
                std::fs::read_to_string(linked.join("untracked.txt")).unwrap(),
                "untracked linked contents\n",
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1bOQ", |contents| {
                !contents.contains("Worktrees")
            });
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[113;6u", |contents| {
                contents.contains("Yes, delete worktree")
            });
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[H\r", |contents| {
                in_repo(contents)
                    && !contents
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .contains("feature/other")
                    && !contents.contains("Yes, delete worktree")
            });
            assert!(
                linked.is_dir(),
                "closing the tab must retain the dirty checkout"
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[44;6u", |contents| {
                contents.contains("Split terminal")
            });
            drive_until(&mut writer, &rx, &mut screen, b"\r", |contents| {
                contents.contains("new worktree in split") && !agent_picker(contents)
            });
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"integration-split\r\x1b[F\r",
                |contents| {
                    contents.lines().nth(1).is_some_and(|header| {
                        header.contains("main") && header.contains("integration-split")
                    })
                },
            );
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"\x1bOP\x1bOQ\x1b[H\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\r",
                |contents| {
                    contents.lines().nth(1).is_some_and(|header| {
                        header.trim_start().starts_with("› integration-split")
                    })
                },
            );
            for number in 2..=10 {
                drive_until(&mut writer, &rx, &mut screen, b"\x1bOP\x1bOQ", |contents| {
                    contents.contains("manual controls")
                });
                drive_until(
                    &mut writer,
                    &rx,
                    &mut screen,
                    b"\x1b[H\x1b[B\x1b[B\x1b[B\x1b[B\r",
                    agent_picker,
                );
                let selected = format!("[shell {number}");
                drive_until(&mut writer, &rx, &mut screen, b"\x1b[F\r", |contents| {
                    contents
                        .lines()
                        .nth(1)
                        .is_some_and(|row| row.contains(&selected))
                });
            }
            pair.master
                .resize(PtySize {
                    rows: 9,
                    cols: 29,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            screen.screen_mut().set_size(9, 29);
            drive_until(&mut writer, &rx, &mut screen, b"", |contents| {
                contents.contains("enlarge")
            });
            writer.write_all(b"\x1b[21~y").unwrap();
            writer.flush().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "invisible confirmation terminated the application"
            );
            pair.master
                .resize(PtySize {
                    rows: 24,
                    cols: 100,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            screen.screen_mut().set_size(24, 100);
            drive_until(&mut writer, &rx, &mut screen, b"", |contents| {
                contents
                    .lines()
                    .nth(1)
                    .is_some_and(|header| header.trim_start().starts_with("› integration-split"))
            });
            // Close-only and cancellation must never take the deletion path.
            let path_file = home.path().join("focused-checkout");
            let record_path = format!(
                "printf '%s' \"$PWD\" > '{}'; printf 'close-%s\\n' ready\r",
                path_file.display()
            );
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                record_path.as_bytes(),
                |contents| contents.contains("close-ready"),
            );
            let split = std::path::PathBuf::from(std::fs::read_to_string(path_file).unwrap());
            let close_menu = |contents: &str| contents.contains("Yes, delete worktree");
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[113;6u", close_menu);
            drive_until(&mut writer, &rx, &mut screen, b"\r", |contents| {
                !close_menu(contents)
            });
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"printf 'cancel-%s\\n' retained\r",
                |contents| contents.contains("cancel-retained"),
            );

            // A refused deletion leaves both the tab and its data intact.
            let local_file = split.join("untracked.txt");
            std::fs::write(&local_file, "keep this work").unwrap();
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[113;6u", close_menu);
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[A\r", |contents| {
                contents.contains("refusing to remove dirty worktree")
            });
            assert_eq!(
                std::fs::read_to_string(&local_file).unwrap(),
                "keep this work"
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1b", |contents| {
                !contents.contains("refusing to remove dirty worktree")
            });
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"printf 'refused-%s\\n' alive\r",
                |contents| contents.contains("refused-alive"),
            );
            std::fs::remove_file(local_file).unwrap();

            drive_until(&mut writer, &rx, &mut screen, b"\x1b[113;6u", close_menu);
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[H\r", |contents| {
                !close_menu(contents)
                    && !contents
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .contains("integration-split")
            });
            assert!(split.is_dir(), "close-only deleted the checkout");

            // Reopen the retained checkout, then explicitly delete it.
            let reopen = format!("\x1b[116;6u\x1b[H\x1b[B\r{}\r\x1b[F\r", split.display());
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                reopen.as_bytes(),
                |contents| {
                    contents.lines().nth(1).is_some_and(|header| {
                        header.trim_start().starts_with("› integration-split")
                    })
                },
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[113;6u", close_menu);
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[A\r", |contents| {
                !close_menu(contents)
                    && !contents
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .contains("integration-split")
            });
            assert!(
                !split.exists(),
                "explicit deletion left the checkout on disk"
            );
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args([
                        "show-ref",
                        "--verify",
                        "--quiet",
                        "refs/heads/integration-split"
                    ])
                    .status()
                    .unwrap()
                    .success(),
                "checkout removal also deleted its branch"
            );
        }
        writer.write_all(b"\x1b[21~y").unwrap();
        writer.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "confirmation did not exit Crit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Crit on its own PTY with a throwaway `$HOME`. Only the `(name, script)` programs
/// given can act as agents or updaters; normal paths never inject update failures.
struct Crit {
    _child: ChildGuard,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    screen: vt100::Parser,
}

impl Crit {
    fn start(
        home: &std::path::Path,
        cwd: &std::path::Path,
        args: &[&str],
        programs: &[(&str, &str)],
    ) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let bin = home.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("git"))
            .find(|path| path.is_file())
            .unwrap()
            .canonicalize()
            .unwrap();
        std::os::unix::fs::symlink(git, bin.join("git")).unwrap();
        for (name, script) in programs {
            std::fs::write(bin.join(name), script).unwrap();
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 160,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_crit"));
        command.arg("--no-kitty");
        command.args(args);
        command.cwd(cwd);
        command.env("SHELL", "/bin/sh");
        command.env("TERM", "xterm-256color");
        command.env("HOME", home);
        // Workspace state must land in the throwaway home, never in the user's.
        command.env_remove("XDG_STATE_HOME");
        command.env_remove("XDG_DATA_HOME");
        command.env("PATH", &bin);
        command.env("PS1", "CRIT_TEST> ");
        command.env("XDG_RUNTIME_DIR", home);
        // Scratch must land in the throwaway home, never in the user's sandbox.
        command.env("CRIT_SANDBOX", home.join("sandbox"));
        let child = ChildGuard(pair.slave.spawn_command(command).unwrap());
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = [0; 8192];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 || tx.send(bytes[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            _child: child,
            _master: pair.master,
            writer,
            rx,
            screen: vt100::Parser::new(24, 160, 0),
        }
    }

    fn drive(&mut self, input: &[u8], ready: impl Fn(&str) -> bool) {
        drive_until(&mut self.writer, &self.rx, &mut self.screen, input, ready);
    }

    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(250);
        while let Ok(bytes) = self
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            self.screen.process(&bytes);
        }
    }
}

fn seed_snapshot(home: &std::path::Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    let directory = home.join(".local/state/crit");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = directory.join("workspace.json");
    std::fs::write(&file, bytes).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn open_folder_selects_location_before_program_and_launches_there() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path().canonicalize().unwrap();
    let original = home.join("original-project");
    let absolute = home.join("chosen folder");
    let relative = home.join("relative folder");
    for path in [&original, &absolute, &relative] {
        std::fs::create_dir(path).unwrap();
    }
    seed_snapshot(
        &home,
        &serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "projects": [{
                "workspaces": [{
                    "path": original,
                    "source": null,
                    "sessions": [{"kind": "shell", "resume": null}],
                    "selected": 0
                }],
                "active": 0,
                "secondary": null,
                "focus_secondary": false,
                "layout": "full",
                "split_ratio": 5000
            }],
            "active_project": 0,
            "native_keys": false,
            "agent_cpu_limited": false,
            "agent_cpu_percent": 50
        }))
        .unwrap(),
    );
    // Restore a shell, but keep the default agent OMP: choosing Terminal must not start it.
    let mut crit = Crit::start(&home, &original, &[], &[]);
    let folder_picker = |screen: &str| screen.contains("enter any folder path:");
    let terminal = |screen: &str| {
        screen.contains("CRIT_TEST> ")
            && !folder_picker(screen)
            && !agent_picker(screen)
            && !screen.contains("new tab")
            && !screen.contains("opening")
    };
    let open_folder = |crit: &mut Crit, new_tab: bool| {
        if new_tab {
            crit.drive(b"\x1b[116;6u", |screen| screen.contains("new tab"));
            crit.drive(b"\x1b[B\r", folder_picker);
        } else {
            crit.drive(b"\x1bOP\x1bOQ\r", folder_picker);
        }
    };
    crit.drive(b"", terminal);
    crit.drive(
        b"ORDER_KEEP=still-here; printf 'order-%s\\n' initialized\r",
        |screen| screen.contains("order-initialized"),
    );
    open_folder(&mut crit, true);
    crit.drive(b"\x1b", terminal);
    open_folder(&mut crit, true);
    crit.drive(format!("{}\r", absolute.display()).as_bytes(), agent_picker);
    crit.drive(b"\x1b", terminal);
    let kept = format!("folder-kept:still-here:{}", original.display());
    crit.drive(
        b"printf 'folder-kept:%s:%s\\n' \"$ORDER_KEEP\" \"$PWD\"\r",
        |screen| screen.contains(&kept),
    );

    for (new_tab, input, destination) in [
        (true, absolute.to_str().unwrap(), &absolute),
        (false, "../relative folder", &relative),
    ] {
        open_folder(&mut crit, new_tab);
        crit.drive(format!("{input}\r").as_bytes(), agent_picker);
        crit.drive(b"\x1b[F\r", terminal);
        let expected = format!("folder-opened:{}", destination.display());
        crit.drive(b"printf 'folder-opened:%s\\n' \"$PWD\"\r", |screen| {
            screen.contains(&expected)
        });
    }
}

#[test]
fn restored_workspace_still_opens_explicit_repo() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let restored = home_path.join("alpha-restored");
    let requested = home_path.join("bravo-requested");
    std::fs::create_dir(&restored).unwrap();
    std::fs::create_dir(&requested).unwrap();
    seed_snapshot(
        &home_path,
        &serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "projects": [{
                "workspaces": [{
                    "path": restored,
                    "source": null,
                    "sessions": [{"kind": "shell", "resume": null}],
                    "selected": 0
                }],
                "active": 0,
                "secondary": null,
                "focus_secondary": false,
                "layout": "full",
                "split_ratio": 5000
            }],
            "active_project": 0,
            "native_keys": false
        }))
        .unwrap(),
    );
    let mut crit = Crit::start(
        &home_path,
        &home_path,
        &["--repo", requested.to_str().unwrap(), "--agent", "shell"],
        &[],
    );
    crit.drive(b"", |screen| screen.contains("CRIT_TEST>"));
    let contents = crit.screen.screen().contents();
    let tabs = contents.lines().next().unwrap();
    assert!(
        tabs.contains("alpha-restored") && tabs.contains("bravo-requested"),
        "the restored project and the requested folder must both be tabs: {tabs}"
    );
    // The requested folder is the one the visible shell runs in.
    let expected = format!("opened-in:{}", requested.display());
    crit.drive(b"printf 'opened-%s:%s\\n' in \"$PWD\"\r", |screen| {
        screen.contains(&expected)
    });
}

#[test]
fn startup_recovery_warning_blocks_input_until_dismissed_and_preserves_snapshot() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    seed_snapshot(&home_path, b"{broken");
    let mut crit = Crit::start(&home_path, &home_path, &[], &[]);
    crit.drive(b"", |screen| {
        screen.contains("Saved workspace could not be loaded")
            && screen.contains("Enter")
            && screen.contains("Esc")
    });
    crit.writer.write_all(b"ordinary input").unwrap();
    crit.writer.flush().unwrap();
    crit.settle();
    crit.drive(b"\x1b", |screen| {
        !screen.contains("Saved workspace could not be loaded") && screen.contains("New terminal")
    });
    crit.drive(b"\r", |screen| screen.contains("CRIT_TEST>"));
    crit.drive(b"printf 'recovery-%s\\n' dismissed\r", |screen| {
        screen.contains("recovery-dismissed")
    });
    assert_eq!(
        std::fs::read(home_path.join(".local/state/crit/workspace.json")).unwrap(),
        b"{broken"
    );
}

#[test]
fn updater_error_requires_enter_before_shell_input_resumes() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let mut crit = Crit::start(
        &home_path,
        &home_path,
        &["--fresh", "--agent", "shell"],
        &[("omp", "#!/bin/sh\necho refused >&2\nexit 1\n")],
    );
    crit.drive(b"", |screen| {
        screen.contains("OMP update failed") && screen.contains("Enter") && screen.contains("Esc")
    });
    let blocked = home_path.join("blocked-input");
    let command = format!("printf blocked > '{}'\r", blocked.display());
    crit.writer.write_all(command.as_bytes()).unwrap();
    crit.writer.flush().unwrap();
    crit.settle();
    assert!(
        !blocked.exists(),
        "error dialog forwarded input to the shell"
    );
    crit.drive(b"\r", |screen| !screen.contains("OMP update failed"));
    crit.drive(b"printf 'update-%s\\n' dismissed\r", |screen| {
        screen.contains("update-dismissed")
    });
    assert!(!blocked.exists(), "dismissal replayed blocked input");
}

#[test]
fn startup_updates_finish_before_fresh_and_restored_agents_open_their_executable() {
    use std::os::unix::fs::PermissionsExt;
    let old = r#"#!/bin/sh
case "$1" in
  --version) echo 2.0.0; exit 0;;
  db) printf '[]'; exit 0;;
  update|upgrade)
    : > "$HOME/updater-started"
    remaining=500
    while [ ! -e "$HOME/install-go" ]; do
      [ "$remaining" -gt 0 ] || exit 9
      remaining=$((remaining - 1))
      /bin/sleep 0.01
    done
    /bin/mv "$HOME/new-agent" "$0"
    exit 0;;
  *)
    : > "$HOME/old-agent-started"
    printf 'AGENT-VERSION=old\n'
    exec /bin/sleep 30;;
esac
"#;
    let new = r#"#!/bin/sh
case "$1" in
  --version) echo 2.0.1; exit 0;;
  db) printf '[]'; exit 0;;
esac
: > "$HOME/new-agent-started"
printf 'AGENT-VERSION=updated\n'
exec /bin/sleep 30
"#;
    for kind in ["omp", "opencode"] {
        for restore in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let home_path = home.path().canonicalize().unwrap();
            std::fs::write(home_path.join("new-agent"), new).unwrap();
            std::fs::set_permissions(
                home_path.join("new-agent"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            if restore {
                seed_snapshot(
                    &home_path,
                    &serde_json::to_vec(&serde_json::json!({
                        "version": 1,
                        "projects": [{
                            "workspaces": [{
                                "path": home_path,
                                "source": null,
                                "sessions": [{"kind": kind, "resume": null}],
                                "selected": 0
                            }],
                            "active": 0,
                            "secondary": null,
                            "focus_secondary": false,
                            "layout": "full",
                            "split_ratio": 5000
                        }],
                        "active_project": 0,
                        "native_keys": false
                    }))
                    .unwrap(),
                );
            }
            let direct = ["--fresh", "--agent", kind];
            let args = if restore { &[][..] } else { &direct[..] };
            let mut crit = Crit::start(&home_path, &home_path, args, &[(kind, old)]);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !home_path.join("updater-started").exists() {
                assert!(Instant::now() < deadline, "{kind}, restore={restore}");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                !home_path.join("old-agent-started").exists(),
                "{kind} started before its update, restore={restore}"
            );
            assert!(!home_path.join("new-agent-started").exists());
            std::fs::write(home_path.join("install-go"), "").unwrap();
            crit.drive(b"", |screen| screen.contains("AGENT-VERSION=updated"));
            assert!(home_path.join("new-agent-started").exists());
            assert!(
                !home_path.join("old-agent-started").exists(),
                "{kind} used the old executable, restore={restore}"
            );
        }
    }
}

#[test]
fn bottom_row_stays_usable_without_resize_after_f3_and_tab_close() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let project = home_path.join("original-project");
    std::fs::create_dir(&project).unwrap();
    let mut crit = Crit::start(&home_path, &project, &["--fresh", "--agent", "shell"], &[]);
    crit.drive(b"", |screen| screen.contains("CRIT_TEST>"));
    let sizes = home_path.join("pane-sizes");
    let resized = home_path.join("pane-resized");
    let prepare = format!(
        "trap 'printf resized >> \"{}\"' WINCH; /bin/stty size > '{}'; printf 'pane-%s\\n' prepared\r",
        resized.display(),
        sizes.display()
    );
    crit.drive(prepare.as_bytes(), |screen| {
        screen.contains("pane-prepared")
    });
    assert_eq!(std::fs::read_to_string(&sizes).unwrap().trim(), "21 158");
    for (input, marker) in [
        (b"".as_slice(), "initial"),
        (b"\x1bOR".as_slice(), "f3-off"),
        (b"\x1bOR".as_slice(), "f3-on"),
    ] {
        let command = format!(
            "/bin/stty size >> '{}'; printf '\\033[999;1Hbottom-%s\\033[1;1H' '{marker}'\r",
            sizes.display()
        );
        let mut bytes = input.to_vec();
        bytes.extend_from_slice(command.as_bytes());
        crit.drive(&bytes, |screen| {
            screen
                .lines()
                .nth(23)
                .is_some_and(|row| row.contains(&format!("bottom-{marker}")))
        });
    }
    crit.drive(b"\x1b[116;6u\x1b[H\r\x1b[F\r", |screen| {
        screen
            .lines()
            .nth(1)
            .is_some_and(|row| row.contains("[shell 1"))
            && !screen.contains("bottom-f3-on")
    });
    crit.drive(b"\x1b[113;6u", |screen| {
        screen.contains("Yes, delete worktree")
    });
    crit.drive(b"\x1b[H\r", |screen| screen.contains("bottom-f3-on"));
    let command = format!(
        "/bin/stty size >> '{}'; printf '\\033[999;1Hbottom-%s\\033[1;1H' after-close\r",
        sizes.display()
    );
    crit.drive(command.as_bytes(), |screen| {
        screen
            .lines()
            .nth(23)
            .is_some_and(|row| row.contains("bottom-after-close"))
    });
    crit.settle();
    assert!(
        std::fs::read_to_string(&sizes)
            .unwrap()
            .lines()
            .all(|size| size == "21 158"),
        "a routine operation changed the full-height pane size"
    );
    assert!(
        !resized.exists(),
        "a routine operation sent SIGWINCH to the surviving pane"
    );
}

#[test]
fn restore_checks_every_opencode_chat_with_one_query() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let project = home_path.join("restored-chats");
    std::fs::create_dir(&project).unwrap();
    let log = home_path.join("opencode-db.log");
    let chats: Vec<_> = (1..=5).map(|number| format!("ses_{number}")).collect();
    seed_snapshot(
        &home_path,
        &serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "projects": [{
                "workspaces": [{
                    "path": project,
                    "source": null,
                    "sessions": chats
                        .iter()
                        .map(|chat| serde_json::json!({"kind": "opencode", "resume": chat}))
                        .collect::<Vec<_>>(),
                    "selected": 0
                }],
                "active": 0,
                "secondary": null,
                "focus_secondary": false,
                "layout": "full",
                "split_ratio": 5000
            }],
            "active_project": 0,
            "native_keys": false
        }))
        .unwrap(),
    );
    // `opencode db` lists every chat as existing and each call is logged; the upgrade check
    // returns at once and the agent itself just stays alive.
    let rows = serde_json::to_string(
        &chats
            .iter()
            .map(|chat| serde_json::json!({"id": chat}))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let opencode = format!(
        "#!/bin/sh\ncase \"$1\" in\n  db) echo \"$*\" >> '{}'; printf '%s' '{rows}';;\n  --version) echo 999.0.0;;\n  upgrade) exit 0;;\n  *) exec /bin/sleep 30;;\nesac\n",
        log.display()
    );
    let mut crit = Crit::start(&home_path, &home_path, &[], &[("opencode", &opencode)]);
    crit.drive(b"", |screen| screen.contains("restored-chats"));
    let queries = std::fs::read_to_string(&log).unwrap();
    assert_eq!(queries.lines().count(), 1, "{queries}");
    assert!(
        queries.contains("WHERE id IN ('ses_1','ses_2','ses_3','ses_4','ses_5')"),
        "{queries}"
    );
}
