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
        (home.path(), repo.as_path(), "main", "feature/other", true),
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
        } else if cwd == home.path() {
            // Queued input must work even while the initial project scan is running.
            writer
                .write_all(format!("{}\r", repo.display()).as_bytes())
                .unwrap();
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
