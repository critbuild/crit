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

#[test]
fn interactive_startup_offers_terminal_and_preserves_explicit_shell_project_selection() {
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("project with spaces");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        vec!["init", "--initial-branch=main"],
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
        if direct_start && cwd == repo {
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
                b"\x1b[44;6uintegration-split\r",
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
