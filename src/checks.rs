//! Demand-started, project-exclusive checking turns. Persistent views are observations,
//! never authority: the inherited project flock is held until cleanup has completed.
use crate::{compose, worktree};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Owner {
    pub window: u32,
    pub session: u64,
    pub pid: u32,
    pub start: u64,
    pub window_start: u64,
}
fn start(pid: u32) -> Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields = stat.rsplit_once(')').context("invalid process stat")?.1;
    ensure!(
        fields.split_whitespace().next() != Some("Z"),
        "process has exited"
    );
    fields
        .split_whitespace()
        .nth(19)
        .context("missing process start")?
        .parse()
        .context("invalid process start")
}
impl Owner {
    pub fn session(pid: u32, session: u64) -> Result<Self> {
        let window = std::process::id();
        Ok(Self {
            window,
            session,
            pid,
            start: start(pid)?,
            window_start: start(window)?,
        })
    }
    pub fn from_env() -> Result<Self> {
        let env = |key: &str| std::env::var(key).ok();
        let window = env("CRIT_WINDOW_PID")
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or_else(|| unsafe { libc::getppid() as u32 });
        let pid = env("CRIT_PANE_PID")
            .or_else(|| env("CRIT_AGENT_PID"))
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or(window);
        Ok(Self {
            window,
            pid,
            session: env("CRIT_SESSION")
                .map(|v| v.parse())
                .transpose()?
                .unwrap_or(0),
            start: start(pid)?,
            window_start: start(window)?,
        })
    }
    fn same(&self, other: &Self) -> bool {
        self.window == other.window
            && self.window_start == other.window_start
            && self.session == other.session
    }
    #[cfg(test)]
    fn alive(&self) -> bool {
        start(self.pid).ok() == Some(self.start)
            && start(self.window).ok() == Some(self.window_start)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub owner: Owner,
    pub worktree: PathBuf,
    pub branch: String,
    pub revision: String,
    pub automatic: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Stopped,
    Queued,
    Starting,
    Active,
    Stopping,
    Blocked,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct View {
    pub project: PathBuf,
    pub phase: Phase,
    pub active: Option<Entry>,
    pub queue: Vec<Entry>,
    pub environment: Option<compose::Environment>,
    pub error: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Request {
    action: String,
    entry: Entry,
    #[serde(default)]
    exec: Option<(u32, u64)>,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    view: View,
    error: Option<String>,
    worker: u32,
}
fn root() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(p) => PathBuf::from(p),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is unset")?
            .join(".local/state"),
    };
    let path = base.join("crit/checks");
    fs::create_dir_all(&path)?;
    let m = fs::symlink_metadata(&path)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink() && m.uid() == unsafe { libc::geteuid() },
        "unsafe checks state directory"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}
fn tree(path: &Path) -> Result<(worktree::Repository, worktree::Worktree)> {
    let repo = worktree::discover(path)?;
    let t = crate::selected_tree(&repo, path)?;
    Ok((repo, t))
}
fn paths(path: &Path) -> Result<(PathBuf, PathBuf, worktree::Worktree)> {
    let (r, t) = tree(path)?;
    let dir = root()?.join(compose::project_id(&t)?);
    fs::create_dir_all(&dir)?;
    Ok((r.common_dir.canonicalize()?, dir, t))
}
fn empty(project: PathBuf) -> View {
    View {
        project,
        phase: Phase::Stopped,
        active: None,
        queue: vec![],
        environment: None,
        error: None,
    }
}
fn load(dir: &Path) -> Result<Option<View>> {
    match fs::read(dir.join("view.json")) {
        Ok(v) => Ok(Some(serde_json::from_slice(&v)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn save(dir: &Path, v: &View) -> Result<()> {
    let temp = dir.join("view.new");
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    f.write_all(&serde_json::to_vec(v)?)?;
    f.sync_all()?;
    fs::rename(temp, dir.join("view.json"))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}
fn lock(path: &Path, nonblock: bool) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    ensure!(
        unsafe {
            libc::flock(
                f.as_raw_fd(),
                libc::LOCK_EX | if nonblock { libc::LOCK_NB } else { 0 },
            )
        } == 0,
        "project lifecycle is busy or cleanup is uncertain"
    );
    Ok(f)
}
fn frame<T: Serialize>(s: &mut UnixStream, v: &T) -> Result<()> {
    let bytes = serde_json::to_vec(v)?;
    ensure!(bytes.len() <= 65536, "check frame too large");
    s.write_all(&(bytes.len() as u32).to_be_bytes())?;
    s.write_all(&bytes)?;
    Ok(())
}
fn receive<T: for<'a> Deserialize<'a>>(s: &mut UnixStream) -> Result<T> {
    let mut n = [0; 4];
    s.read_exact(&mut n)?;
    let n = u32::from_be_bytes(n) as usize;
    ensure!(n <= 65536, "check frame too large");
    let mut b = vec![0; n];
    s.read_exact(&mut b)?;
    Ok(serde_json::from_slice(&b)?)
}
fn peer(s: &UnixStream) -> Result<()> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        } == 0
            && cred.uid == unsafe { libc::geteuid() },
        "foreign checks client"
    );
    Ok(())
}
fn send_fd(socket: &UnixStream, fd: i32) -> Result<()> {
    let mut byte = 1u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        *(libc::CMSG_DATA(header).cast::<i32>()) = fd;
    }
    ensure!(
        unsafe { libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL) } == 1,
        "failed to transfer lifecycle lock"
    );
    Ok(())
}
// Allocation-free so this is also safe in the forked pre-exec child.
fn receive_fd(socket: i32) -> i32 {
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let mut control = [0usize; 8];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control);
    if unsafe { libc::recvmsg(socket, &mut message, libc::MSG_CMSG_CLOEXEC) } != 1 {
        return -1;
    }
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
            || (*header).cmsg_len != libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize
        {
            return -1;
        }
        let fd = *(libc::CMSG_DATA(header).cast::<i32>());
        if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            libc::close(fd);
            return -1;
        }
        fd
    }
}
fn socket_address(dir: &Path) -> std::io::Result<std::os::unix::net::SocketAddr> {
    use sha2::{Digest, Sha256};
    use std::os::{linux::net::SocketAddrExt, unix::ffi::OsStrExt};
    // Linux abstract sockets avoid sun_path truncation in deeply nested state
    // directories. Both ends still authenticate the kernel-reported peer UID.
    let name = format!(
        "crit-check-{}-{:x}",
        unsafe { libc::geteuid() },
        Sha256::digest(dir.as_os_str().as_bytes())
    );
    std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
}
fn connect_socket(dir: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect_addr(&socket_address(dir)?)
}
#[derive(Serialize, Deserialize)]
struct Launch {
    owner: Owner,
    path: PathBuf,
}
fn window_address(window: u32, ticks: u64) -> std::io::Result<std::os::unix::net::SocketAddr> {
    use std::os::linux::net::SocketAddrExt;
    std::os::unix::net::SocketAddr::from_abstract_name(
        format!("crit-check-window-{}-{window}-{ticks}", unsafe {
            libc::geteuid()
        })
        .as_bytes(),
    )
}
fn spawn_guard(dir: &Path, path: &Path) -> Result<Child> {
    Ok(Command::new("/proc/self/exe")
        .arg("__checks")
        .arg("guard")
        .arg(dir)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?)
}
fn connect(dir: &Path, t: &worktree::Worktree, owner: &Owner) -> Result<UnixStream> {
    if let Ok(s) = connect_socket(dir) {
        return Ok(s);
    }
    let _boot = lock(&dir.join("boot.lock"), false)?;
    if let Ok(s) = connect_socket(dir) {
        return Ok(s);
    }
    // A guardian launched underneath a pane would be killed by that pane's
    // subreaper on ordinary close. The live window launches it as a sibling.
    let mut local = None;
    let (pid, ticks) = match UnixStream::connect_addr(&window_address(
        owner.window,
        owner.window_start,
    )?) {
        Ok(mut socket) => {
            peer(&socket)?;
            socket.set_read_timeout(Some(Duration::from_secs(5)))?;
            socket.set_write_timeout(Some(Duration::from_secs(2)))?;
            frame(
                &mut socket,
                &Launch {
                    owner: owner.clone(),
                    path: t.path.clone(),
                },
            )?;
            let reply: std::result::Result<(u32, u64), String> = receive(&mut socket)?;
            reply.map_err(anyhow::Error::msg)?
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            ensure!(
                std::env::var_os("CRIT_PANE_PID").is_none(),
                "Crit window checking launcher is unavailable; refusing to attach runtime cleanup to a closing pane"
            );
            let child = spawn_guard(dir, &t.path)?;
            let identity = (child.id(), start(child.id())?);
            local = Some(child);
            identity
        }
        Err(e) => return Err(e).context("contact checking window launcher"),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(s) = connect_socket(dir) {
            return Ok(s);
        }
        if let Some(child) = local.as_mut() {
            if let Some(exit) = child.try_wait()? {
                bail!("check guardian failed: {exit}; inspect crit check status")
            }
        }
        ensure!(
            start(pid).ok() == Some(ticks),
            "check guardian exited; inspect crit check status"
        );
        ensure!(
            Instant::now() < deadline,
            "check coordinator startup timed out"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
fn call(
    path: &Path,
    owner: Owner,
    action: &str,
    automatic: bool,
    exec: Option<(u32, u64)>,
) -> Result<Reply> {
    let (_, dir, t) = paths(path)?;
    let mut s = if matches!(action, "enqueue" | "acquire") {
        connect(&dir, &t, &owner)?
    } else {
        match connect_socket(&dir) {
            Ok(s) => s,
            Err(e) => {
                if matches!(action, "release" | "cancel") {
                    if let Some(v) = load(&dir)? {
                        if v.active.is_none() && v.queue.is_empty() && v.phase == Phase::Stopped {
                            return Ok(Reply {
                                view: v,
                                error: None,
                                worker: 0,
                            });
                        }
                    }
                }
                return Err(e).context("no live check coordinator; inspect crit check status");
            }
        }
    };
    peer(&s)?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    if matches!(action, "release" | "cancel") {
        s.set_read_timeout(Some(Duration::from_secs(180)))?;
    }
    frame(
        &mut s,
        &Request {
            action: action.into(),
            entry: Entry {
                owner,
                worktree: t.path,
                branch: t.branch,
                revision: t.head,
                automatic,
            },
            exec,
        },
    )?;
    let r: Reply = receive(&mut s)?;
    if let Some(e) = r.error {
        bail!("{e}")
    }
    Ok(r)
}
pub fn enqueue(path: &Path, owner: Owner) -> Result<View> {
    Ok(call(path, owner, "enqueue", false, None)?.view)
}
pub fn acquire(path: &Path, owner: Owner) -> Result<View> {
    Ok(call(path, owner, "acquire", true, None)?.view)
}
pub fn release(path: &Path, owner: &Owner) -> Result<View> {
    Ok(call(path, owner.clone(), "release", false, None)?.view)
}
pub fn cancel(path: &Path, owner: &Owner) -> Result<View> {
    Ok(call(path, owner.clone(), "cancel", false, None)?.view)
}
pub fn restart(path: &Path, owner: &Owner) -> Result<View> {
    Ok(call(path, owner.clone(), "restart", false, None)?.view)
}
pub fn status(path: &Path) -> Result<Option<View>> {
    let (_, dir, _) = paths(path)?;
    observed(&dir)
}
pub fn statuses() -> Result<Vec<View>> {
    let mut result = vec![];
    for entry in fs::read_dir(root()?)? {
        let p = entry?.path();
        if p.is_dir() {
            if let Some(v) = observed(&p)? {
                result.push(v)
            }
        }
    }
    Ok(result)
}
fn observed(dir: &Path) -> Result<Option<View>> {
    let mut view = load(dir)?;
    if let Some(v) = view.as_mut() {
        let live = fs::read(dir.join("worker.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<(u32, u64)>(&b).ok())
            .is_some_and(|(pid, ticks)| start(pid).ok() == Some(ticks));
        if v.phase != Phase::Stopped && !live {
            v.phase = Phase::Blocked;
            v.error = Some(format!(
                "{}coordinator is absent; managed runtime requires reconciliation on next request",
                v.error.as_ref().map_or(String::new(), |e| format!("{e}; "))
            ));
        }
    }
    Ok(view)
}
fn close(window: u32, session: Option<u64>, automatic: bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut errors = vec![];
    let mut pending = vec![];
    // Initiate independent teardowns together, then await one absolute deadline.
    for dir in fs::read_dir(root()?)? {
        let dir = dir?.path();
        let Some(v) = load(&dir)? else { continue };
        for e in v.active.iter().chain(v.queue.iter()) {
            if e.owner.window == window
                && session.is_none_or(|s| s == e.owner.session)
                && (!automatic || e.automatic)
            {
                let result = (|| -> Result<UnixStream> {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    ensure!(!remaining.is_zero(), "shutdown cleanup deadline exceeded");
                    let mut s = connect_socket(&dir)
                        .context("check coordinator absent; runtime cleanup is uncertain")?;
                    peer(&s)?;
                    s.set_write_timeout(Some(remaining.min(Duration::from_secs(2))))?;
                    frame(
                        &mut s,
                        &Request {
                            action: "cancel".into(),
                            entry: e.clone(),
                            exec: None,
                        },
                    )?;
                    Ok(s)
                })();
                match result {
                    Ok(s) => pending.push(s),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
        }
    }
    for mut s in pending {
        let result = (|| -> Result<()> {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "shutdown cleanup deadline exceeded; worker retains lifecycle lock"
            );
            s.set_read_timeout(Some(remaining))?;
            let r: Reply = receive(&mut s).context(
                "cleanup acknowledgment timed out/closed; worker must finish before successor",
            )?;
            if let Some(e) = r.error {
                bail!("{e}")
            }
            Ok(())
        })();
        if let Err(e) = result {
            errors.push(format!("{e:#}"));
        }
    }
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}
pub fn close_session(window: u32, session: u64) -> Result<()> {
    close(window, Some(session), false)
}
pub fn finish_turn(window: u32, session: u64) -> Result<()> {
    close(window, Some(session), true)
}
pub fn close_window(window: u32) -> Result<()> {
    close(window, None, false)
}
pub fn removal_guard(path: &Path) -> Result<File> {
    let (_, dir, t) = paths(path)?;
    let lock_path = compose::project_lock_path(&t)?;
    let guard = lock(&lock_path, true)?;
    if let Some(v) = load(&dir)? {
        ensure!(
            !matches!(v.phase, Phase::Blocked | Phase::Starting | Phase::Stopping)
                && !v
                    .active
                    .iter()
                    .chain(v.queue.iter())
                    .any(|e| e.worktree == t.path),
            "release/cancel the project checking turn before deleting this worktree; cleanup must be certain"
        );
    }
    let legacy = root()?
        .parent()
        .context("checks state parent missing")?
        .join("environments");
    let trees = worktree::list(&worktree::discover(path)?)?;
    let manifest = lock_path.with_file_name("compose.json").is_file();
    if manifest
        || trees.iter().any(|tree| {
            compose::has_config(tree) || legacy.join(&tree.id).join("compose.json").is_file()
        })
    {
        compose::preflight(&t)?;
        if manifest {
            ensure!(
                compose::status(&t)?.services.is_empty(),
                "managed checking resources still exist; reconcile/release them before deleting this checkout"
            );
        }
    }
    Ok(guard)
}

pub struct Watcher {
    stop: Arc<AtomicBool>,
    wake: File,
    thread: Option<thread::JoinHandle<()>>,
}
impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let byte = 1u64;
        unsafe {
            libc::write(self.wake.as_raw_fd(), (&byte as *const u64).cast(), 8);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
pub fn watch(tx: SyncSender<crate::event::Event>) -> Result<Watcher> {
    let dir = root()?;
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    ensure!(fd >= 0, "inotify unavailable");
    let file = unsafe { File::from_raw_fd(fd) };
    fn add(fd: i32, path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(p) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
            unsafe {
                libc::inotify_add_watch(
                    fd,
                    p.as_ptr(),
                    libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_CLOSE_WRITE,
                );
            }
        }
    }
    add(fd, &dir);
    for e in fs::read_dir(&dir)? {
        add(fd, &e?.path());
    }
    let wake_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    ensure!(wake_fd >= 0, "watcher wake descriptor unavailable");
    let wake = unsafe { File::from_raw_fd(wake_fd) };
    let thread_wake = wake.try_clone()?;
    let window = std::process::id();
    let window_start = start(window)?;
    let launcher = UnixListener::bind_addr(&window_address(window, window_start)?)?;
    launcher.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let thread = thread::spawn(move || {
        let _file = file;
        let mut buf = [0u8; 8192];
        let mut pending = true;
        let mut guardians: Vec<(Child, File)> = vec![];
        while !stopped.load(Ordering::Relaxed) {
            guardians.retain_mut(|(child, _)| matches!(child.try_wait(), Ok(None)));
            let mut polls = vec![
                libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: thread_wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: launcher.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            for (_, pin) in &guardians {
                polls.push(libc::pollfd {
                    fd: pin.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            unsafe {
                libc::poll(
                    polls.as_mut_ptr(),
                    polls.len() as libc::nfds_t,
                    if pending { 100 } else { -1 },
                );
            }
            if polls[1].revents != 0 {
                break;
            }
            if polls[0].revents != 0 {
                while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
                if let Ok(entries) = fs::read_dir(&dir) {
                    for e in entries.flatten() {
                        add(fd, &e.path());
                    }
                }
                pending = true;
            }
            while let Ok((mut socket, _)) = launcher.accept() {
                let result = (|| -> Result<(u32, u64)> {
                    peer(&socket)?;
                    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
                    socket.set_write_timeout(Some(Duration::from_millis(200)))?;
                    let request: Launch = receive(&mut socket)?;
                    ensure!(
                        request.owner.window == window
                            && request.owner.window_start == window_start,
                        "checking launch belongs to another window"
                    );
                    ensure!(
                        start(request.owner.pid).ok() == Some(request.owner.start),
                        "checking launch owner exited"
                    );
                    let (_, project, t) = paths(&request.path)?;
                    let mut child = spawn_guard(&project, &t.path)?;
                    let pid = child.id();
                    let pin = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
                    if pin < 0 {
                        let _ = child.kill();
                        let _ = child.wait();
                        bail!("cannot pin checking guardian")
                    }
                    let pin = unsafe { File::from_raw_fd(pin) };
                    let ticks = start(pid);
                    guardians.push((child, pin));
                    Ok((pid, ticks?))
                })()
                .map_err(|e| format!("{e:#}"));
                let _ = frame(&mut socket, &result);
            }
            if pending {
                match statuses() {
                    Ok(views) => match tx.try_send(crate::event::Event::ChecksChanged(views)) {
                        Ok(()) => pending = false,
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                        Err(_) => {}
                    },
                    Err(_) => pending = false,
                }
            }
        }
    });
    Ok(Watcher {
        stop,
        wake,
        thread: Some(thread),
    })
}

fn entry_tree(e: &Entry) -> worktree::Worktree {
    worktree::Worktree {
        id: String::new(),
        path: e.worktree.clone(),
        branch: e.branch.clone(),
        head: e.revision.clone(),
        is_main: false,
        locked: false,
        bare: false,
        prunable: false,
    }
}
fn stop_exec(execs: &mut Vec<(u32, u64)>) -> Result<()> {
    for &(pid, ticks) in execs.iter() {
        if start(pid).ok() == Some(ticks) {
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            ensure!(
                fd >= 0 || start(pid).ok() != Some(ticks),
                "cannot pin check command for cleanup"
            );
            if fd >= 0 {
                let fd = unsafe { File::from_raw_fd(fd) };
                if start(pid).ok() == Some(ticks) {
                    for signal in [libc::SIGCONT, libc::SIGTERM] {
                        unsafe {
                            libc::syscall(
                                libc::SYS_pidfd_send_signal,
                                fd.as_raw_fd(),
                                signal,
                                std::ptr::null::<libc::siginfo_t>(),
                                0,
                            );
                        }
                    }
                }
            }
        }
    }
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        execs.retain(|&(pid, ticks)| start(pid).ok() == Some(ticks));
        if execs.is_empty() {
            return Ok(());
        }
        ensure!(
            Instant::now() < end,
            "owned check command did not finish cleanup; successor blocked"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
// Both coordinator processes are subreapers. A dead operation cannot leave a
// detached Docker CLI racing recovery, even when that CLI creates a new session.
fn reap_orphans() -> Result<()> {
    let pid = std::process::id();
    let children = format!("/proc/{pid}/task/{pid}/children");
    loop {
        while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
        let pids: Vec<i32> = fs::read_to_string(&children)?
            .split_whitespace()
            .filter_map(|v| v.parse().ok())
            .collect();
        if pids.is_empty() {
            return Ok(());
        }
        for child in pids {
            unsafe {
                libc::kill(child, libc::SIGKILL);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn operation(dir: &Path, entry: &Entry, action: &str) -> Result<Child> {
    let _ = fs::remove_file(dir.join("operation.json"));
    let parent = std::process::id();
    let exe = Path::new("/proc/self/exe");
    let mut c = Command::new(&exe);
    c.arg("__pane")
        .arg("--parent")
        .arg(parent.to_string())
        .arg("--parent-start")
        .arg(start(parent)?.to_string())
        .arg("--")
        .arg(&exe)
        .arg("__checks")
        .arg("operation")
        .arg(action)
        .arg(dir)
        .arg(serde_json::to_string(entry)?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The outer supervisor retains the inherited flock until its entire subtree
    // is gone, even if both coordinator processes are killed together.
    let fd: i32 = std::env::var("CRIT_CHECK_LOCK_FD")
        .context("missing lifecycle lock")?
        .parse()?;
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, 0);
    }
    let result = c.spawn();
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Ok(result?)
}
fn terminate_operation(child: &mut Child) -> Result<()> {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    child.wait()?;
    Ok(())
}
struct Waiting {
    stream: UnixStream,
    entry: Entry,
    action: String,
}
fn respond(s: &mut UnixStream, v: &View, error: Option<String>) {
    let _ = frame(
        s,
        &Reply {
            view: v.clone(),
            error,
            worker: std::process::id(),
        },
    );
}
fn queue_entry(v: &mut View, entry: &Entry) -> Result<()> {
    if let Some(existing) = v
        .active
        .iter_mut()
        .chain(v.queue.iter_mut())
        .find(|e| e.owner.same(&entry.owner))
    {
        ensure!(
            existing.worktree == entry.worktree,
            "this pane already owns/requests another worktree; release it first"
        );
        if !entry.automatic {
            existing.automatic = false;
        }
    } else {
        v.queue.push(entry.clone());
    }
    Ok(())
}
fn read_execs(dir: &Path) -> Result<Vec<(u32, u64)>> {
    match fs::read(dir.join("execs.json")) {
        Ok(b) => Ok(serde_json::from_slice(&b)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}
fn save_execs(dir: &Path, execs: &[(u32, u64)]) -> Result<()> {
    let mut f = File::create(dir.join("execs.new"))?;
    f.write_all(&serde_json::to_vec(execs)?)?;
    f.sync_all()?;
    fs::rename(dir.join("execs.new"), dir.join("execs.json"))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}
#[derive(Default)]
struct Processes {
    fds: std::collections::HashMap<(u32, u64), Option<File>>,
}
impl Processes {
    fn alive(&mut self, pid: u32, ticks: u64) -> bool {
        let fd = self.fds.entry((pid, ticks)).or_insert_with(|| {
            if start(pid).ok() != Some(ticks) {
                return None;
            }
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            if fd < 0 {
                return None;
            }
            let file = unsafe { File::from_raw_fd(fd) };
            if start(pid).ok() != Some(ticks) {
                return None;
            }
            Some(file)
        });
        fd.as_ref().is_some_and(|f| {
            let mut p = libc::pollfd {
                fd: f.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut p, 1, 0) == 0 }
        })
    }
    fn owner(&mut self, o: &Owner) -> bool {
        self.alive(o.pid, o.start) && self.alive(o.window, o.window_start)
    }
}
struct Monitor {
    child: Child,
    output: std::process::ChildStdout,
    pending: Vec<u8>,
}
impl Monitor {
    fn start(project: &Path) -> Result<Self> {
        let pid = std::process::id();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
        // Whole-second cursors replay the previous owner's teardown when FIFO
        // handoff/reacquisition happens within that second.
        let since = format!("{}.{:09}", now.as_secs(), now.subsec_nanos());
        let mut child = Command::new("/proc/self/exe")
            .arg("__pane")
            .arg("--parent")
            .arg(pid.to_string())
            .arg("--parent-start")
            .arg(start(pid)?.to_string())
            .arg("--")
            .arg("docker")
            .args([
                "events",
                "--since",
                &since,
                "--filter",
                "label=io.crit.managed=2",
                "--filter",
                &format!("label=io.crit.repository={}", project.display()),
                "--filter",
                "event=die",
                "--filter",
                "event=destroy",
                "--filter",
                "event=oom",
                "--filter",
                "event=health_status",
                "--format",
                "{{json .}}",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let output = child.stdout.take().context("missing Docker event stream")?;
        let fd = output.as_raw_fd();
        unsafe {
            libc::fcntl(
                fd,
                libc::F_SETFL,
                libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
            );
        }
        Ok(Self {
            child,
            output,
            pending: vec![],
        })
    }
    fn failed(&mut self, active: bool) -> Option<String> {
        let mut bytes = [0; 4096];
        loop {
            match self.output.read(&mut bytes) {
                Ok(0) => return Some("Docker event monitoring stream closed".into()),
                Ok(n) => {
                    self.pending.extend_from_slice(&bytes[..n]);
                    if self.pending.len() > 65536 {
                        return Some("Docker event monitoring frame exceeded 64 KiB".into());
                    }
                    while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                        let line: Vec<_> = self.pending.drain(..=end).collect();
                        let event = match serde_json::from_slice::<serde_json::Value>(&line) {
                            Ok(event) => event,
                            Err(e) => {
                                return Some(format!("invalid Docker event monitoring frame: {e}"));
                            }
                        };
                        let action = event
                            .get("Action")
                            .or_else(|| event.get("status"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        // During startup Compose --wait owns service health/error
                        // reporting. A service event must not erase its real error.
                        // Loss of the monitoring stream still fails closed in every phase.
                        if active
                            && matches!(
                                action,
                                "die" | "destroy" | "oom" | "health_status: unhealthy"
                            )
                        {
                            let name = event
                                .pointer("/Actor/Attributes/name")
                                .or_else(|| event.pointer("/Actor/ID"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown container");
                            return Some(format!("checking service {name}: {action}"));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Some(format!("Docker event monitoring read failed: {e}")),
            }
        }
    }
    fn stop(mut self) -> Result<()> {
        terminate_operation(&mut self.child)
    }
}
fn worker(dir: &Path, path: &Path, parent: u32, parent_start: u64) -> Result<()> {
    if let Ok(fd) = std::env::var("CRIT_CHECK_LOCK_FD") {
        let fd: i32 = fd.parse()?;
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
    }
    fs::write(
        dir.join("worker.json"),
        serde_json::to_vec(&(std::process::id(), start(std::process::id())?))?,
    )?;
    let (repo, t) = tree(path)?;
    let mut v = load(dir)?.unwrap_or_else(|| empty(repo.common_dir));
    v.active = None;
    v.queue.clear();
    v.environment = None;
    // Every incarnation reconciles managed resources; never restore a previous turn.
    v.phase = Phase::Stopping;
    v.error = Some("reconciling previous managed runtime".into());
    save(dir, &v)?;
    let mut old_execs = read_execs(dir)?;
    stop_exec(&mut old_execs)?;
    save_execs(dir, &[])?;
    if let Err(e) = compose::recover(&t) {
        v.phase = Phase::Blocked;
        v.error = Some(format!("recovery failed: {e:#}"));
        save(dir, &v)?;
        return Err(e);
    }
    v.phase = Phase::Stopped;
    v.error = None;
    save(dir, &v)?;
    let listener = UnixListener::bind_addr(&socket_address(dir)?)?;
    listener.set_nonblocking(true)?;
    let mut waiting: Vec<Waiting> = vec![];
    let mut execs = vec![];
    let mut op: Option<(Child, String)> = None;
    let mut rebuild = false;
    let mut cancelled_owner: Option<Owner> = None;
    let mut processes = Processes::default();
    let mut operation_identity = None;
    let mut monitor: Option<Monitor> = None;
    let startup_deadline = Instant::now() + Duration::from_secs(2);
    let mut accepted = false;
    loop {
        let parent_dead = !processes.alive(parent, parent_start);
        if parent_dead {
            v.queue.clear();
            if let Some(m) = monitor.take() {
                m.stop()?;
            }
            if let Some(e) = v.active.as_ref() {
                stop_exec(&mut execs)?;
                if let Some((mut child, _)) = op.take() {
                    terminate_operation(&mut child)?;
                }
                v.phase = Phase::Stopping;
                save(dir, &v)?;
                let mut cleanup = operation(dir, e, "down")?;
                cleanup.wait()?;
                let result: std::result::Result<compose::Environment, String> =
                    serde_json::from_slice(&fs::read(dir.join("operation.json"))?)?;
                match result {
                    Ok(_) => {
                        v.active = None;
                        v.environment = None;
                        v.phase = Phase::Stopped
                    }
                    Err(e) => {
                        v.phase = Phase::Blocked;
                        v.error = Some(format!("guardian died; cleanup failed: {e}"));
                    }
                }
                save(dir, &v)?;
            }
            break;
        }
        while let Ok((mut s, _)) = listener.accept() {
            accepted = true;
            s.set_read_timeout(Some(Duration::from_millis(200)))?;
            s.set_write_timeout(Some(Duration::from_millis(200)))?;
            let request = (|| {
                peer(&s)?;
                receive::<Request>(&mut s)
            })();
            let Ok(r) = request else { continue };
            let owner = &r.entry.owner;
            if !processes.owner(owner) && !matches!(r.action.as_str(), "cancel" | "release") {
                respond(&mut s, &v, Some("owner process/window is gone".into()));
                continue;
            }
            let is_active = v.active.as_ref().is_some_and(|e| e.owner.same(owner));
            match r.action.as_str() {
                "enqueue" | "acquire" => {
                    if v.phase == Phase::Blocked {
                        respond(&mut s, &v, v.error.clone());
                        continue;
                    }
                    if let Err(e) = queue_entry(&mut v, &r.entry) {
                        respond(&mut s, &v, Some(format!("{e:#}")));
                        continue;
                    }
                    if v.active.is_none() {
                        v.phase = Phase::Queued;
                    }
                    save(dir, &v)?;
                    if r.action == "enqueue" || (is_active && v.phase == Phase::Active) {
                        respond(&mut s, &v, None)
                    } else {
                        waiting.push(Waiting {
                            stream: s,
                            entry: r.entry,
                            action: r.action,
                        });
                    }
                }
                "release" | "cancel" => {
                    v.queue.retain(|e| !e.owner.same(owner));
                    if is_active {
                        cancelled_owner = Some(owner.clone());
                        rebuild = false;
                        if v.phase == Phase::Active {
                            v.phase = Phase::Stopping;
                        }
                        waiting.push(Waiting {
                            stream: s,
                            entry: r.entry,
                            action: r.action,
                        });
                    } else {
                        respond(&mut s, &v, None)
                    }
                    save(dir, &v)?;
                }
                "restart" => {
                    if !is_active || v.phase != Phase::Active {
                        respond(&mut s, &v, Some("only active owner can rebuild".into()));
                    } else {
                        rebuild = true;
                        v.phase = Phase::Stopping;
                        waiting.push(Waiting {
                            stream: s,
                            entry: r.entry,
                            action: r.action,
                        });
                        save(dir, &v)?;
                    }
                }
                "exec" => {
                    if is_active && v.phase == Phase::Active {
                        if let Some((pid, ticks)) = r.exec {
                            if start(pid).ok() == Some(ticks) {
                                execs.push((pid, ticks));
                                save_execs(dir, &execs)?;
                                respond(&mut s, &v, None);
                                let fd: i32 = std::env::var("CRIT_CHECK_LOCK_FD")?.parse()?;
                                let _ = send_fd(&s, fd);
                            } else {
                                respond(&mut s, &v, Some("check command process changed".into()));
                            }
                        } else {
                            respond(&mut s, &v, None)
                        }
                    } else {
                        respond(
                            &mut s,
                            &v,
                            Some("check exec requires the active healthy turn".into()),
                        );
                    }
                }
                _ => respond(&mut s, &v, Some("unknown coordinator request".into())),
            }
        }
        let mut disconnected = vec![];
        waiting.retain(|w| {
            let mut b = 0u8;
            let n = unsafe {
                libc::recv(
                    w.stream.as_raw_fd(),
                    (&mut b as *mut u8).cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            if n == 0 {
                if w.action == "acquire" {
                    disconnected.push(w.entry.owner.clone());
                }
                false
            } else {
                true
            }
        });
        disconnected.retain(|o| {
            v.active
                .iter()
                .chain(v.queue.iter())
                .any(|e| e.owner.same(o) && e.automatic)
        });
        if let Some(e) = v.active.as_ref() {
            if disconnected.iter().any(|o| e.owner.same(o)) {
                cancelled_owner = Some(e.owner.clone());
            }
        }
        let queued_before = v.queue.len();
        v.queue
            .retain(|e| processes.owner(&e.owner) && !disconnected.iter().any(|o| e.owner.same(o)));
        if v.queue.len() != queued_before {
            save(dir, &v)?;
        }
        if v.active.as_ref().is_some_and(|e| {
            !processes.owner(&e.owner) || disconnected.iter().any(|o| e.owner.same(o))
        }) {
            rebuild = false;
            if v.phase == Phase::Active {
                v.phase = Phase::Stopping;
                save(dir, &v)?;
            }
        }
        // Stop and reap startup subprocesses before tearing down partial resources.
        let runtime_error = monitor
            .as_mut()
            .and_then(|m| m.failed(v.phase == Phase::Active));
        let runtime_failed = runtime_error.is_some();
        if let Some(error) = runtime_error {
            v.error = Some(error);
            if v.phase == Phase::Active {
                v.phase = Phase::Stopping;
                save(dir, &v)?;
            }
        }
        let cancelled = runtime_failed
            || v.active.as_ref().is_some_and(|e| {
                !processes.owner(&e.owner)
                    || cancelled_owner.as_ref().is_some_and(|o| e.owner.same(o))
                    || waiting.iter().any(|w| {
                        matches!(w.action.as_str(), "release" | "cancel")
                            && w.entry.owner.same(&e.owner)
                    })
            });
        if cancelled && v.phase == Phase::Starting {
            if op.as_ref().is_some_and(|(_, action)| action == "up") {
                let (mut child, _) = op.take().expect("operation exists");
                operation_identity = None;
                terminate_operation(&mut child)?;
            }
            v.phase = Phase::Stopping;
            rebuild = false;
            save(dir, &v)?;
        }
        if let Some((child, action)) = op.as_mut() {
            if let Some(exit) = child.try_wait()? {
                let action = action.clone();
                op = None;
                operation_identity = None;
                let result = fs::read(dir.join("operation.json"))
                    .ok()
                    .and_then(|b| {
                        serde_json::from_slice::<std::result::Result<compose::Environment, String>>(
                            &b,
                        )
                        .ok()
                    })
                    .unwrap_or_else(|| {
                        Err(format!("runtime worker exited {exit} without a result"))
                    });
                match (action.as_str(), result) {
                    ("up", Ok(env)) => {
                        v.environment = Some(env);
                        v.phase = if cancelled {
                            Phase::Stopping
                        } else {
                            Phase::Active
                        };
                    }
                    ("down", Ok(_)) => {
                        v.environment = None;
                        save_execs(dir, &[])?;
                        if rebuild {
                            v.phase = Phase::Starting;
                        } else {
                            v.active = None;
                            cancelled_owner = None;
                            v.phase = Phase::Stopped;
                        }
                    }
                    (_, Err(e)) => {
                        v.phase = Phase::Blocked;
                        v.error = Some(e);
                    }
                    _ => {}
                }
                save(dir, &v)?;
            }
        }
        if op.is_none() {
            if v.phase == Phase::Stopping {
                if let Some(m) = monitor.take() {
                    m.stop()?;
                }
                if let Some(e) = &v.active {
                    let owner = &e.owner;
                    if !processes.alive(owner.window, owner.window_start)
                        && processes.alive(owner.pid, owner.start)
                        && !execs.contains(&(owner.pid, owner.start))
                    {
                        execs.push((owner.pid, owner.start));
                        save_execs(dir, &execs)?;
                    }
                }
                if let Err(e) = stop_exec(&mut execs) {
                    v.phase = Phase::Blocked;
                    v.error = Some(format!("{e:#}"));
                    save(dir, &v)?;
                } else if let Some(e) = &v.active {
                    op = Some((operation(dir, e, "down")?, "down".into()));
                }
            } else if v.phase == Phase::Starting {
                if let Some(e) = &v.active {
                    if monitor.is_none() {
                        monitor = Some(Monitor::start(&v.project)?);
                    }
                    op = Some((operation(dir, e, "up")?, "up".into()));
                }
            } else if v.active.is_none() && v.phase != Phase::Blocked && !v.queue.is_empty() {
                let e = v.queue.remove(0);
                v.active = Some(e);
                v.phase = Phase::Starting;
                v.error = None;
                save(dir, &v)?;
            }
        }
        waiting.retain_mut(|w| {
            let own = v
                .active
                .as_ref()
                .is_some_and(|e| e.owner.same(&w.entry.owner));
            let queued = v.queue.iter().any(|e| e.owner.same(&w.entry.owner));
            let ready = match w.action.as_str() {
                "acquire" | "restart" => own && v.phase == Phase::Active,
                "release" | "cancel" => !own && !queued,
                _ => false,
            };
            if v.phase == Phase::Blocked || ready || (!own && !queued) {
                let err = if v.phase == Phase::Blocked {
                    v.error.clone()
                } else if !ready {
                    Some("checking request cancelled".into())
                } else {
                    None
                };
                let sent = frame(
                    &mut w.stream,
                    &Reply {
                        view: v.clone(),
                        error: err,
                        worker: std::process::id(),
                    },
                );
                if sent.is_err() && ready && w.action == "acquire" {
                    cancelled_owner = Some(w.entry.owner.clone());
                }
                false
            } else {
                true
            }
        });
        if cancelled_owner
            .as_ref()
            .is_some_and(|o| v.active.as_ref().is_some_and(|e| e.owner.same(o)))
            && v.phase == Phase::Active
        {
            v.phase = Phase::Stopping;
            save(dir, &v)?;
        }
        if v.phase == Phase::Blocked {
            break;
        }
        if v.active.is_none() && v.queue.is_empty() && op.is_none() && v.phase != Phase::Stopped {
            v.phase = Phase::Stopped;
            save(dir, &v)?;
        }
        if v.active.is_none()
            && v.queue.is_empty()
            && op.is_none()
            && waiting.is_empty()
            && (accepted || Instant::now() >= startup_deadline)
        {
            break;
        }
        let mut keys = vec![(parent, parent_start)];
        for e in v.active.iter().chain(v.queue.iter()) {
            keys.push((e.owner.pid, e.owner.start));
            keys.push((e.owner.window, e.owner.window_start));
        }
        let mut immediate =
            op.is_none() && matches!(v.phase, Phase::Starting | Phase::Stopping | Phase::Queued);
        if let Some((child, _)) = &op {
            let identity = match operation_identity {
                Some((pid, ticks)) if pid == child.id() => (pid, ticks),
                _ => {
                    let ticks = start(child.id()).unwrap_or(0);
                    operation_identity = Some((child.id(), ticks));
                    (child.id(), ticks)
                }
            };
            if !processes.alive(identity.0, identity.1) {
                immediate = true
            }
            keys.push(identity);
        }
        processes.fds.retain(|key, _| keys.contains(key));
        let mut polls = vec![libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        for fd in processes.fds.values().flatten() {
            polls.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for w in &waiting {
            polls.push(libc::pollfd {
                fd: w.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if let Some(m) = &monitor {
            polls.push(libc::pollfd {
                fd: m.output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let timeout = if immediate {
            0
        } else if !accepted {
            startup_deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .max(1)
                .min(i32::MAX as u128) as i32
        } else {
            -1
        };
        let result =
            unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, timeout) };
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    if let Some(m) = monitor.take() {
        m.stop()?;
    }
    Ok(())
}

pub fn internal(args: &[OsString]) -> Result<()> {
    match args.first().and_then(|s| s.to_str()) {
        Some("guard") if args.len() == 3 => {
            let dir = Path::new(&args[1]);
            let path = Path::new(&args[2]);
            let (_, t) = tree(path)?;
            let lock = lock(&compose::project_lock_path(&t)?, false)?;
            unsafe {
                libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
                libc::fcntl(lock.as_raw_fd(), libc::F_SETFD, 0);
            }
            let parent = std::process::id();
            let ticks = start(parent)?;
            let mut c = Command::new("/proc/self/exe");
            c.env("CRIT_CHECK_LOCK_FD", lock.as_raw_fd().to_string())
                .arg("__checks")
                .arg("worker")
                .arg(dir)
                .arg(path)
                .arg(parent.to_string())
                .arg(ticks.to_string());
            unsafe {
                c.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = c.spawn()?;
            unsafe {
                libc::fcntl(lock.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            }
            let exit = child.wait()?;
            // Kill orphaned operation subprocesses before attempting recovery.
            reap_orphans()?;
            let mut execs = read_execs(dir)?;
            stop_exec(&mut execs)?;
            let mut v = load(dir)?.unwrap_or_else(|| {
                empty(
                    worktree::discover(path)
                        .map(|r| r.common_dir)
                        .unwrap_or_default(),
                )
            });
            if !exit.success() || v.active.is_some() {
                let reason = v
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("coordinator exited {exit}"));
                v.phase = Phase::Stopping;
                save(dir, &v)?;
                match compose::recover(&t) {
                    Ok(()) => {
                        v.phase = Phase::Stopped;
                        v.active = None;
                        v.queue.clear();
                        v.environment = None;
                        v.error = Some(format!("{reason}; managed runtime recovered"));
                    }
                    Err(e) => {
                        v.phase = Phase::Blocked;
                        v.error = Some(format!("{reason}; recovery failed: {e:#}"));
                    }
                }
                save(dir, &v)?;
            }
            drop(lock);
            Ok(())
        }
        Some("worker") if args.len() == 5 => worker(
            Path::new(&args[1]),
            Path::new(&args[2]),
            args[3].to_str().context("parent PID")?.parse()?,
            args[4].to_str().context("parent identity")?.parse()?,
        ),
        Some("operation") if args.len() == 4 => {
            let fd: i32 = std::env::var("CRIT_CHECK_LOCK_FD")
                .context("missing lifecycle lock")?
                .parse()?;
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            let action = args[1].to_str().context("operation")?;
            let dir = Path::new(&args[2]);
            let e: Entry = serde_json::from_str(args[3].to_str().context("entry")?)?;
            let t = entry_tree(&e);
            let result: std::result::Result<compose::Environment, String> = match action {
                "up" => (|| {
                    compose::preflight(&t)?;
                    compose::up(&t)
                })(),
                "down" => compose::down(&t),
                "status" => compose::status(&t),
                _ => bail!("invalid runtime operation"),
            }
            .map_err(|e| format!("{e:#}"));
            let mut f = File::create(dir.join("operation.new"))?;
            f.write_all(&serde_json::to_vec(&result)?)?;
            f.sync_all()?;
            fs::rename(dir.join("operation.new"), dir.join("operation.json"))?;
            Ok(())
        }
        _ => bail!("invalid private checks invocation"),
    }
}

pub fn cli(args: &[OsString], repo: &Path) -> Result<()> {
    let action = args
        .first()
        .and_then(|s| s.to_str())
        .context("use crit check acquire|request|release|cancel|status|restart|logs|exec")?;
    let mut path = repo.to_path_buf();
    let mut automatic = false;
    let mut command: Vec<OsString> = vec![];
    let mut i = 1;
    while i < args.len() {
        match args[i].to_str() {
            Some("--repo") => {
                i += 1;
                path = PathBuf::from(args.get(i).context("--repo requires PATH")?);
            }
            Some("--automatic") if action == "release" => automatic = true,
            Some("--") if action == "exec" => {
                command.extend_from_slice(&args[i + 1..]);
                break;
            }
            _ => bail!("unknown check argument {:?}", args[i]),
        }
        i += 1;
    }
    if action == "status" {
        println!("{}", serde_json::to_string_pretty(&status(&path)?)?);
        return Ok(());
    }
    if action == "logs" {
        let v = status(&path)?.context("no checking runtime requested")?;
        let e = v.active.context("no active checking runtime")?;
        let pid = std::process::id();
        let exit = Command::new("/proc/self/exe")
            .arg("__pane")
            .arg("--parent")
            .arg(pid.to_string())
            .arg("--parent-start")
            .arg(start(pid)?.to_string())
            .arg("--")
            .arg("docker")
            .args(compose::logs_args(&entry_tree(&e))?)
            .status()?;
        ensure!(exit.success(), "check logs exited {exit}");
        return Ok(());
    }
    let owner = Owner::from_env()?;
    if automatic {
        finish_turn(owner.window, owner.session)?;
        return Ok(());
    }
    let view = match action {
        "acquire" => acquire(&path, owner)?,
        "request" => enqueue(&path, owner)?,
        "release" => release(&path, &owner)?,
        "cancel" => cancel(&path, &owner)?,
        "restart" => restart(&path, &owner)?,
        "exec" => {
            ensure!(
                !command.is_empty(),
                "use crit check exec [--repo PATH] -- PROGRAM [ARGS...]"
            );
            let (_, dir, _) = paths(&path)?;
            let mut socket = connect_socket(&dir)?;
            peer(&socket)?;
            // Get the worker identity without creating a new turn.
            let (_, _, t) = paths(&path)?;
            let entry = Entry {
                owner: owner.clone(),
                worktree: t.path,
                branch: t.branch,
                revision: t.head,
                automatic: true,
            };
            frame(
                &mut socket,
                &Request {
                    action: "exec".into(),
                    entry: entry.clone(),
                    exec: None,
                },
            )?;
            // Authorization-only exec request returns the current worker identity.
            let r: Reply = receive(&mut socket)?;
            if let Some(e) = r.error {
                bail!("{e}")
            }
            let entry = r.view.active.context("no active checking owner")?;
            // Gate execution until the worker has registered this supervised command.
            let worker_start = start(r.worker)?;
            let argv: Vec<std::ffi::CString> = std::iter::once(OsString::from("/proc/self/exe"))
                .chain([
                    OsString::from("__pane"),
                    OsString::from("--parent"),
                    OsString::from(r.worker.to_string()),
                    OsString::from("--parent-start"),
                    OsString::from(worker_start.to_string()),
                    OsString::from("--"),
                ])
                .chain(command)
                .map(|s| {
                    use std::os::unix::ffi::OsStrExt;
                    std::ffi::CString::new(s.as_os_str().as_bytes()).context("NUL in command")
                })
                .collect::<Result<_>>()?;
            let cwd = std::ffi::CString::new({
                use std::os::unix::ffi::OsStrExt;
                entry.worktree.as_os_str().as_bytes()
            })?;
            let ptrs: Vec<*const libc::c_char> = argv
                .iter()
                .map(|a| a.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect();
            let launcher = std::process::id() as i32;
            let (gate, permit) = UnixStream::pair()?;
            let gate_fd = gate.as_raw_fd();
            let permit_fd = permit.as_raw_fd();
            let pid = unsafe { libc::fork() };
            ensure!(pid >= 0, "fork failed");
            if pid == 0 {
                unsafe {
                    libc::close(permit_fd);
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    if libc::getppid() != launcher {
                        libc::_exit(125);
                    }
                    if libc::chdir(cwd.as_ptr()) != 0 {
                        libc::_exit(126);
                    }
                    let lock = receive_fd(gate_fd);
                    if lock < 0 {
                        libc::_exit(125);
                    }
                    libc::fcntl(lock, libc::F_SETFD, 0);
                    libc::close(gate_fd);
                    libc::execv(argv[0].as_ptr(), ptrs.as_ptr());
                    libc::_exit(127);
                }
            }
            drop(gate);
            let mut status = 0;
            let registration = (|| -> Result<File> {
                let mut s = connect_socket(&dir)?;
                peer(&s)?;
                s.set_read_timeout(Some(Duration::from_secs(180)))?;
                frame(
                    &mut s,
                    &Request {
                        action: "exec".into(),
                        entry: entry.clone(),
                        exec: Some((pid as u32, start(pid as u32)?)),
                    },
                )?;
                let r: Reply = receive(&mut s)?;
                if let Some(e) = r.error {
                    bail!("{e}")
                }
                let fd = receive_fd(s.as_raw_fd());
                ensure!(fd >= 0, "worker did not transfer lifecycle lock");
                Ok(unsafe { File::from_raw_fd(fd) })
            })();
            let result = registration.and_then(|lock| send_fd(&permit, lock.as_raw_fd()));
            if let Err(e) = result {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
                return Err(e).context("check command cancelled before execution");
            }
            drop(permit);
            loop {
                let n = unsafe { libc::waitpid(pid, &mut status, 0) };
                if n == pid {
                    break;
                }
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    bail!("wait for check command failed")
                }
            }
            if libc::WIFEXITED(status) {
                std::process::exit(libc::WEXITSTATUS(status))
            }
            std::process::exit(128 + libc::WTERMSIG(status));
        }
        _ => bail!("use crit check acquire|request|release|cancel|status|restart|logs|exec"),
    };
    println!("{}", serde_json::to_string_pretty(&view)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_identity_is_pinned() {
        let mut o = Owner::session(std::process::id(), 7).unwrap();
        assert!(o.alive());
        o.start += 1;
        assert!(!o.alive());
    }
    #[test]
    fn pane_identity_does_not_depend_on_agent_pid() {
        let a = Owner::session(std::process::id(), 7).unwrap();
        let mut b = a.clone();
        b.pid += 1;
        b.start += 1;
        assert!(a.same(&b));
        b.window_start += 1;
        assert!(!a.same(&b));
    }
    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(&65537u32.to_be_bytes()).unwrap();
        assert!(
            receive::<Request>(&mut b)
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
    }
    fn entry(session: u64) -> Entry {
        Entry {
            owner: Owner::session(std::process::id(), session).unwrap(),
            worktree: PathBuf::from(format!("tree-{session}")),
            branch: session.to_string(),
            revision: "revision".into(),
            automatic: true,
        }
    }
    #[test]
    fn fifo_retries_never_jump_or_duplicate_the_queue() {
        let mut v = empty(PathBuf::from("repo"));
        let a = entry(1);
        let b = entry(2);
        let c = entry(3);
        queue_entry(&mut v, &a).unwrap();
        queue_entry(&mut v, &b).unwrap();
        queue_entry(&mut v, &a).unwrap();
        v.active = Some(v.queue.remove(0));
        queue_entry(&mut v, &c).unwrap();
        assert_eq!(v.active.unwrap().owner.session, 1);
        assert_eq!(
            v.queue.iter().map(|e| e.owner.session).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
    #[test]
    fn automatic_retry_does_not_unpin_manual_turn() {
        let mut v = empty(PathBuf::from("repo"));
        let mut a = entry(1);
        a.automatic = false;
        queue_entry(&mut v, &a).unwrap();
        a.automatic = true;
        queue_entry(&mut v, &a).unwrap();
        assert!(!v.queue[0].automatic);
        a.worktree = PathBuf::from("other");
        assert!(queue_entry(&mut v, &a).is_err());
    }
    #[test]
    fn lifecycle_lock_excludes_successors_until_last_holder_drops() {
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = dir.path().join("project.lock");
        let first = lock(&path, true).unwrap();
        assert!(lock(&path, true).is_err());
        drop(first);
        assert!(lock(&path, true).is_ok());
    }
    #[test]
    fn cleanup_never_signals_a_reused_identity() {
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();
        let ticks = start(pid).unwrap();
        let mut commands = vec![(pid, ticks + 1)];
        stop_exec(&mut commands).unwrap();
        let survived = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        let _ = child.wait();
        assert!(survived);
        assert!(commands.is_empty());
    }
    #[test]
    fn transferred_lock_blocks_successor_after_original_owner_drops() {
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = dir.path().join("project.lock");
        let original = lock(&path, true).unwrap();
        let (a, b) = UnixStream::pair().unwrap();
        send_fd(&a, original.as_raw_fd()).unwrap();
        let fd = receive_fd(b.as_raw_fd());
        assert!(fd >= 0);
        let transferred = unsafe { File::from_raw_fd(fd) };
        drop(original);
        assert!(lock(&path, true).is_err());
        drop(transferred);
        assert!(lock(&path, true).is_ok());
    }
    #[test]
    fn pidfd_liveness_rejects_wrong_start_identity() {
        let pid = std::process::id();
        let ticks = start(pid).unwrap();
        let mut processes = Processes::default();
        assert!(processes.alive(pid, ticks));
        assert!(!processes.alive(pid, ticks + 1));
        assert!(processes.alive(pid, ticks));
    }
}
