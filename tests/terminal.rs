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
    let distinct_head = git_output(
        &repo,
        &[
            "-c",
            "user.name=Crit Test",
            "-c",
            "user.email=test@localhost",
            "commit-tree",
            "HEAD^{tree}",
            "-p",
            &original_head,
            "-m",
            "existing branch at a distinct commit",
        ],
    );
    assert_ne!(distinct_head, original_head);
    git_output(&repo, &["branch", "topic-existing", &distinct_head]);
    for branch in ["picker-00", "picker-01"] {
        git_output(&repo, &["branch", branch]);
    }

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
        for (input, marker) in [
            ("", "CRIT_TEST>"),
            (
                "printf 'runtime-%s:%s\\n' ready \"$PWD\"\r",
                expected.as_str(),
            ),
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
            // The default new-tab action opens home without a path prompt.
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
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[116;6u\r", in_home);
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

            // Reopening home selects the existing session instead of losing its state.
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[9;6u", in_repo);
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[116;6u\r", in_home);
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
                b"\x1bOP\x1bOQ\x1b[H\x1b[B\x1b[B\x1b[B\rmain\r",
                |contents| contents.contains("│› main") && contents.contains("already"),
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1b", |contents| {
                !contents.contains("│› main")
            });
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"FIRST_PANE_TOKEN=kept; printf 'first-%s\\n' prepared\r",
                |contents| contents.contains("first-prepared"),
            );
            let original_state = home.path().join("original-pane-state");
            let target_state = home.path().join("existing-pane-state");
            let linked_status = git_output(&linked, &["status", "--porcelain"]);
            let trees_before_existing = git_output(&repo, &["worktree", "list", "--porcelain"]);
            assert!(
                !screen
                    .screen()
                    .contents()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .contains("feature/other"),
                "linked checkout must be unopened before the picker is used",
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[44;6u", |contents| {
                contents.contains("Existing branch/worktree")
            });
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"\x1b[B\x1b[B\r",
                |contents| {
                    contents.contains("› feature/other")
                        && contents.contains(linked.to_str().unwrap())
                },
            );

            // A short viewport must scroll the nested list, not lose its controls.
            pair.master
                .resize(PtySize {
                    rows: 10,
                    cols: 30,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            screen.screen_mut().set_size(10, 30);
            drive_until(&mut writer, &rx, &mut screen, b"", |contents| {
                contents.contains("› feature/other")
                    && contents.contains("Enter")
                    && contents.contains("Esc")
            });
            for (key, selected) in [
                (b"\x1b[B".as_slice(), "picker-00"),
                (b"\x1b[B".as_slice(), "picker-01"),
                (b"\x1b[A".as_slice(), "picker-00"),
            ] {
                let selected = format!("› {selected}");
                drive_until(&mut writer, &rx, &mut screen, key, |contents| {
                    contents.contains(&selected)
                        && contents.contains("Enter")
                        && contents.contains("Esc")
                });
            }
            drive_until(&mut writer, &rx, &mut screen, b"\x1b", |contents| {
                contents.contains("› Existing branch")
            });
            drive_until(&mut writer, &rx, &mut screen, b"\r", |contents| {
                contents.contains("› feature/other")
            });
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
                contents.contains("› feature/other") && contents.contains(linked.to_str().unwrap())
            });
            drive_until(&mut writer, &rx, &mut screen, b"\r", |contents| {
                contents.lines().nth(1).is_some_and(|header| {
                    header.trim_start().starts_with("› main")
                        && header.contains("│")
                        && header.contains("feature/other")
                }) && !contents.contains("↑↓ choose")
            });
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
            let record_target = format!(
                "\x1b[93;6uprintf '%s\\n%s\\n%s\\n' \"$PWD\" \"$(git branch --show-current)\" \"$(git rev-parse HEAD)\" > '{}'; printf 'reuse-%s\\n' target\r",
                target_state.display()
            );
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                record_target.as_bytes(),
                |contents| contents.contains("reuse-target"),
            );
            assert_eq!(
                std::fs::read_to_string(&target_state).unwrap(),
                format!("{}\nfeature/other\n{original_head}\n", linked.display()),
            );
            assert_eq!(
                git_output(&repo, &["worktree", "list", "--porcelain"]),
                trees_before_existing,
                "selecting a linked checkout must not create another checkout",
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

            // Checkouts precede local unchecked-out branches, sorted by branch name.
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[44;6u", |contents| {
                contents.contains("Existing branch/worktree")
            });
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"\x1b[B\x1b[B\r",
                |contents| contents.contains("› feature/other"),
            );
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"\x1b[B\x1b[B\x1b[B",
                |contents| {
                    contents.contains("› topic-existing") && contents.contains("create worktree")
                },
            );
            drive_until(&mut writer, &rx, &mut screen, b"\r", |contents| {
                contents.lines().nth(1).is_some_and(|header| {
                    header.trim_start().starts_with("› main")
                        && header.contains("│")
                        && header.contains("topic-existing")
                }) && !contents.contains("↑↓ choose")
            });
            let record_original = record_original.replace("reuse-%s", "create-%s");
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                record_original.as_bytes(),
                |contents| contents.contains("create-original"),
            );
            assert_eq!(
                std::fs::read_to_string(&original_state).unwrap(),
                format!("kept\nmain\n{original_head}\n"),
            );
            let record_target = record_target.replace("reuse-%s", "create-%s");
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                record_target.as_bytes(),
                |contents| contents.contains("create-target"),
            );
            let state = std::fs::read_to_string(&target_state).unwrap();
            let fields: Vec<_> = state.lines().collect();
            assert_eq!(fields.len(), 3);
            let created = std::path::Path::new(fields[0]);
            assert!(created.is_dir());
            assert_ne!(created, repo);
            assert_ne!(created, linked);
            assert_eq!(fields[1], "topic-existing");
            assert_eq!(fields[2], distinct_head);
            assert_eq!(git_output(&repo, &["rev-parse", "HEAD"]), original_head);
            assert_eq!(git_output(&repo, &["branch", "--show-current"]), "main");
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[113;6u", |contents| {
                contents.contains("Yes, delete worktree")
            });
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[H\r", |contents| {
                in_repo(contents)
                    && !contents
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .contains("topic-existing")
                    && !contents.contains("Yes, delete worktree")
            });
            assert!(
                created.is_dir(),
                "closing the tab must retain the branch checkout"
            );
            drive_until(&mut writer, &rx, &mut screen, b"\x1b[44;6u", |contents| {
                contents.contains("Split terminal")
            });
            drive_until(
                &mut writer,
                &rx,
                &mut screen,
                b"\rintegration-split\r",
                |contents| {
                    contents.lines().nth(1).is_some_and(|header| {
                        header.contains("│") && header.contains("› integration-split")
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
                            && !header.contains("│")
                    })
                },
            );
            for _ in 0..9 {
                writer.write_all(b"\x1b[13;6u\x1b[B\x1b[B\r").unwrap();
            }
            drive_until(&mut writer, &rx, &mut screen, b"", |contents| {
                contents
                    .lines()
                    .nth(1)
                    .is_some_and(|header| header.contains("[shell 10"))
            });
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
                !contents.contains("Tab and checkout kept.")
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
            let reopen = format!("\x1b[116;6u\x1b[B\r{}\r", split.display());
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
                contents.contains("Checkout removed")
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
