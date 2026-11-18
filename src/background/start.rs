//! A background desktop's start, one at a time per host niri, under
//! `background.start.lock` in the host's runtime directory, since the window rule it
//! writes matches any nested niri. It tears down what servers that ended left, checks the
//! rules file, names a host workspace, writes the rule and waits for the host niri to load
//! it, starts the nested niri and waits for its window, restores the rules file, waits for
//! the nested niri to answer on its socket, and then starts a private bus, whose services
//! get the nested display. The record is written before each step that leaves something to
//! undo; a failure undoes what was done.

use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use niri_ipc::{Action, Window, WorkspaceReferenceArg};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::time::Instant;

use super::plan::{self, NIRI_REMOVED, PRIVATE_SERVICES, RULES_HEADER, WorkspaceName};
use super::record::{Ident, Record, write_atomically};
use super::teardown::{self, Children};
use super::{WORK_DIRS, rules};
use crate::control::runtime::RuntimeDir;
use crate::error::ToolError;
use crate::niri::listen::Listener;
use crate::policy::background::{Bus, Config};
use crate::runner::{self, Exact, Owned};
use crate::{Env, cli, niri, watch};

const PROC: &str = "/proc";
const START_LOCK: &str = "background.start.lock";
const LOCK_WAIT: Duration = Duration::from_secs(20);
const RETRY: Duration = Duration::from_millis(20);
/// How long a bus that closed its stdout gets to say how it exited.
const EXIT: Duration = Duration::from_secs(1);
/// For the host niri to load its config once the rules file changed.
const CONFIG_LOAD: Duration = Duration::from_secs(10);
/// For the private bus to say its address.
const BUS: Duration = Duration::from_secs(10);
/// For the nested niri's window to open on the host.
const WINDOW: Duration = Duration::from_secs(15);
/// For the nested niri's socket to appear and answer.
const SOCKET: Duration = Duration::from_secs(10);
/// How much of a child's log a failed start quotes. The teardown that follows removes the
/// work directory the log is in.
const LOG_TAIL: u64 = 2048;
/// The file in the work directory naming the nested instance: its socket's file name.
pub(crate) const INSTANCE: &str = "instance";

/// A background desktop that has started.
#[derive(Debug)]
pub(crate) struct Started {
    pub(crate) record: Record,
    pub(crate) children: Children,
    pub(crate) workspace: WorkspaceName,
    /// The nested niri's socket, resolved.
    pub(crate) socket: PathBuf,
    pub(crate) display: String,
    pub(crate) bus: String,
}

/// Starts the background desktop of `host`'s server, as `config` says.
pub(crate) async fn start(host: &Env, config: Config) -> Result<Started, String> {
    let runtime = RuntimeDir::of(host)?;
    runtime
        .create()
        .map_err(|error| format!("create {}: {error}", runtime.path().display()))?;
    let lock = lock(&runtime).await?;
    for failure in teardown::stale(&runtime).await {
        cli::print_error(&format!(
            "tear down the background desktop a server that ended left: {failure}"
        ));
    }
    let mut start = Start::new(host, config, &runtime)?;
    let started = match start.steps().await {
        Ok((workspace, socket, display, bus)) => Ok(Started {
            record: start.record,
            children: start.children,
            workspace,
            socket,
            display,
            bus,
        }),
        Err(error) => Err(
            match teardown::run(&start.record, &mut start.children).await {
                Ok(()) => error,
                Err(undoing) => format!("{error}; undoing the start: {undoing}"),
            },
        ),
    };
    drop(lock);
    started
}

impl Started {
    /// Writes the record to the nested instance's runtime directory, where its crash
    /// guardian looks, and names the nested instance in the work directory; then, when the
    /// host instance is stopped, stops the nested one too, so a desktop started while
    /// stopped starts stopped.
    pub(crate) fn settle(&self, host: &RuntimeDir, nested: &RuntimeDir) -> Result<(), String> {
        nested
            .create()
            .map_err(|error| format!("create {}: {error}", nested.path().display()))?;
        self.record.write(nested.path())?;
        let instance = self
            .socket
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        write_atomically(&self.record.work_dir.join(INSTANCE), instance.as_bytes())?;
        if host.stopped().unwrap_or(true) {
            nested.stop().map_err(|error| {
                format!("set the stop flag in {}: {error}", nested.path().display())
            })?;
        }
        Ok(())
    }
}

/// A start under way: what it has done so far, in its record, and the children it holds.
struct Start<'a> {
    host: &'a Env,
    config: Config,
    socket: niri::Socket,
    record: Record,
    children: Children,
}

/// What the steps give: the workspace's name, the nested socket and display, and the bus.
type Steps = (WorkspaceName, PathBuf, String, String);

impl<'a> Start<'a> {
    /// Makes the work directory, `<runtime>/background/<pid>-<start time>/`, and writes
    /// the first record there.
    fn new(host: &'a Env, config: Config, runtime: &RuntimeDir) -> Result<Self, String> {
        let server = Ident::of(Path::new(PROC), std::process::id())
            .ok_or("this server's own process can't be read in /proc")?;
        let host_socket = host.instance.socket().map_err(Clone::clone)?.to_path_buf();
        let work_dir = runtime
            .path()
            .join(WORK_DIRS)
            .join(format!("{}-{}", server.pid, server.start_time));
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&work_dir)
            .map_err(|error| format!("create {}: {error}", work_dir.display()))?;
        let record = Record {
            server,
            work_dir,
            host_socket: host_socket.clone(),
            rules: None,
            rule_written: false,
            workspace: None,
            workspace_id: None,
            bus: config.bus,
            bus_address: None,
            bus_daemon: None,
            niri: None,
            nested_socket: None,
            nested_display: None,
        };
        record.write(&record.work_dir)?;
        Ok(Self {
            host,
            config,
            socket: niri::Socket::at(host_socket),
            record,
            children: Children::default(),
        })
    }

    fn save(&self) -> Result<(), String> {
        self.record.write(&self.record.work_dir)
    }

    async fn steps(&mut self) -> Result<Steps, String> {
        let host_pid = niri::pid(&self.socket)
            .await
            .map_err(|error| error.detail)?;
        let rules = rules::prepare(host_pid).await?;
        self.record.rules = Some(rules.clone());
        self.save()?;
        let (workspace, name) = self.workspace().await?;
        let bus = self.bus_address()?;
        let host_socket = self.record.host_socket.clone();
        let mut listener = Listener::open(&host_socket).await.map_err(detail)?;
        let rule = plan::rules_with(&name, self.config.size);
        self.rule(&mut listener, &rules, rule).await?;
        let (window, pid) = self.niri(&mut listener, host_pid, &bus).await?;
        self.rule(&mut listener, &rules, RULES_HEADER.to_owned())
            .await?;
        if window.workspace_id != Some(workspace) {
            let action = Action::MoveWindowToWorkspace {
                window_id: Some(window.id),
                reference: WorkspaceReferenceArg::Id(workspace),
                focus: false,
            };
            niri::carry_out(&self.socket, action)
                .await
                .map_err(detail)?;
        }
        let (socket, display) = self.nested(pid).await?;
        if self.config.bus == Bus::Private {
            self.private_bus(&bus, &display).await?;
        }
        Ok((name, socket, display, bus))
    }

    /// Names the focused output's last workspace, its ID and name.
    async fn workspace(&mut self) -> Result<(u64, WorkspaceName), String> {
        let output = niri::focused_output(&self.socket)
            .await
            .map_err(detail)?
            .ok_or("the host niri reports no focused output")?;
        let workspaces = niri::workspaces(&self.socket).await.map_err(detail)?;
        let id = plan::pick_workspace(&workspaces, &output.name)?;
        let project = watch::project_of(Path::new(PROC), std::process::id());
        let name = plan::workspace_name(project.as_deref(), std::process::id(), &workspaces);
        self.record.workspace = Some(name.clone());
        self.record.workspace_id = Some(id);
        self.save()?;
        let action = Action::SetWorkspaceName {
            name: name.as_str().to_owned(),
            workspace: Some(WorkspaceReferenceArg::Id(id)),
        };
        niri::carry_out(&self.socket, action)
            .await
            .map_err(detail)?;
        Ok((id, name))
    }

    /// The bus the nested niri's apps get: the host's, or the address of the private one,
    /// which starts once the nested display exists.
    fn bus_address(&mut self) -> Result<String, String> {
        let address = match self.config.bus {
            Bus::Host => self
                .host
                .session_bus
                .as_ref()
                .and_then(|bus| bus.to_str())
                .map(str::to_owned)
                .ok_or("the host has no session bus address in UTF-8")?,
            Bus::Private => plan::unix_address(&self.record.work_dir.join("bus")),
        };
        self.record.bus_address = Some(address.clone());
        self.save()?;
        Ok(address)
    }

    /// Starts a `dbus-daemon` of the desktop's own at `address`, and waits until it says
    /// it listens. The services it starts get the nested `display`, so their windows open
    /// on the background desktop.
    async fn private_bus(&mut self, address: &str, display: &str) -> Result<(), String> {
        let work = self.record.work_dir.clone();
        let log_path = work.join("bus.log");
        let mut log = log_file(&log_path)?;
        let services = work.join("services");
        write_services(&services, &mut log)?;
        let config = work.join("bus.conf");
        write_atomically(
            &config,
            plan::bus_config(&work.join("bus"), &services).as_bytes(),
        )?;
        let program = self
            .host
            .desktop
            .locate("dbus-daemon")
            .ok_or_else(|| format!("no dbus-daemon in {}", self.host.desktop.search))?;
        let args = [
            "--nofork".to_owned(),
            format!("--config-file={}", config.display()),
            "--print-address=1".to_owned(),
        ];
        let runtime_dir = self
            .host
            .runtime_dir
            .as_ref()
            .ok_or("the host has no XDG_RUNTIME_DIR")?;
        let set = plan::bus_env(display, runtime_dir, address);
        let env = Exact {
            set: &set,
            remove: &NIRI_REMOVED,
        };
        let mut daemon = runner::owned(&program, &args, env, log, true).map_err(detail)?;
        self.record.bus_daemon = daemon.pid().and_then(|pid| Ident::of(Path::new(PROC), pid));
        let saved = self.save();
        let listening = listening(&mut daemon).await;
        self.children.bus = Some(daemon);
        saved?;
        listening.map_err(|why| format!("the private dbus-daemon {why}; {}", log_tail(&log_path)))
    }

    /// Replaces the rules file with `text` and waits for the host niri to load it.
    async fn rule(
        &mut self,
        listener: &mut Listener,
        path: &Path,
        text: String,
    ) -> Result<(), String> {
        let writing = text != RULES_HEADER;
        if writing {
            self.record.rule_written = true;
            self.save()?;
        }
        rules::write(path.to_path_buf(), text).await?;
        if !writing {
            self.record.rule_written = false;
            self.save()?;
        }
        let failed = listener.config_loaded(CONFIG_LOAD).await.map_err(detail)?;
        if failed {
            return Err(format!(
                "the host niri refused its config once {} changed; `niri validate` says why",
                path.display()
            ));
        }
        Ok(())
    }

    /// Starts the nested niri, the same program as the host's niri `host_pid`, and waits
    /// for its window. Returns the window and the nested niri's PID.
    async fn niri(
        &mut self,
        listener: &mut Listener,
        host_pid: u32,
        bus: &str,
    ) -> Result<(Window, u32), String> {
        let work = self.record.work_dir.clone();
        let program = niri_program(host_pid)?;
        let config = work.join("niri.kdl");
        write_atomically(&config, plan::NESTED_CONFIG.as_bytes())?;
        let log_path = work.join("niri.log");
        let log = log_file(&log_path)?;
        let display = self
            .host
            .wayland_display
            .as_ref()
            .ok_or("the host has no WAYLAND_DISPLAY")?;
        let runtime_dir = self
            .host
            .runtime_dir
            .as_ref()
            .ok_or("the host has no XDG_RUNTIME_DIR")?;
        let set = plan::niri_env(display, runtime_dir, bus);
        let args = ["-c".to_owned(), config.display().to_string()];
        let env = Exact {
            set: &set,
            remove: &NIRI_REMOVED,
        };
        let mut child = runner::owned(&program, &args, env, log, false).map_err(detail)?;
        let pid = child.pid().ok_or("the nested niri exited at once")?;
        self.record.niri = Ident::of(Path::new(PROC), pid);
        let saved = self.save();
        let window = tokio::select! {
            window = listener.window_of(pid, WINDOW) => window.map_err(detail),
            ended = child.ended() => Err(format!(
                "the nested niri {} before its window opened",
                exit(ended)
            )),
        };
        self.children.niri = Some(child);
        saved?;
        let window = window.map_err(|error| format!("{error}; {}", log_tail(&log_path)))?;
        Ok((window, pid))
    }

    /// Waits for the nested niri `pid`'s socket in the runtime directory, then for it to
    /// answer. Returns the socket, resolved, and the nested display.
    async fn nested(&mut self, pid: u32) -> Result<(PathBuf, String), String> {
        let runtime_dir = self
            .host
            .runtime_dir
            .clone()
            .ok_or("the host has no XDG_RUNTIME_DIR")?;
        let until = Instant::now() + SOCKET;
        let (socket, display) = loop {
            if let Some(found) = find_socket(&runtime_dir, pid) {
                break found;
            }
            if Instant::now() >= until {
                return Err(format!(
                    "the nested niri, PID {pid}, made no socket in {} within {SOCKET:?}",
                    runtime_dir.display()
                ));
            }
            tokio::time::sleep(RETRY).await;
        };
        let nested = niri::Socket::at(socket.clone());
        while let Err(error) = niri::outputs(&nested).await {
            if Instant::now() >= until {
                return Err(format!(
                    "the nested niri didn't answer on {}: {}",
                    socket.display(),
                    error.detail
                ));
            }
            tokio::time::sleep(RETRY).await;
        }
        let socket = std::fs::canonicalize(&socket)
            .map_err(|error| format!("resolve {}: {error}", socket.display()))?;
        self.record.nested_socket = Some(socket.clone());
        self.record.nested_display = Some(display.clone());
        self.save()?;
        Ok((socket, display))
    }
}

/// Takes `background.start.lock` in `runtime` within `LOCK_WAIT`.
async fn lock(runtime: &RuntimeDir) -> Result<File, String> {
    let path = runtime.path().join(START_LOCK);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let until = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < until => {
                tokio::time::sleep(RETRY).await;
            }
            Err(TryLockError::WouldBlock) => {
                return Err(format!(
                    "another background desktop's start held {} for {LOCK_WAIT:?}",
                    path.display()
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err(format!("lock {}: {error}", path.display()));
            }
        }
    }
}

/// Writes the private bus's activation files to `services`, noting in `log` each service
/// it found none for.
fn write_services(services: &Path, log: &mut File) -> Result<(), String> {
    DirBuilder::new()
        .mode(0o700)
        .create(services)
        .map_err(|error| format!("create {}: {error}", services.display()))?;
    let var = |name| std::env::var_os(name);
    let home = var("HOME").map(PathBuf::from);
    let search = plan::service_dirs(
        var("XDG_DATA_HOME").as_deref(),
        home.as_deref(),
        var("XDG_DATA_DIRS").as_deref(),
    );
    for name in PRIVATE_SERVICES {
        let reduced = search.iter().find_map(|dir| {
            let text = std::fs::read_to_string(dir.join(format!("{name}.service"))).ok()?;
            plan::reduced_service(name, &text)
        });
        match reduced {
            Some(file) => {
                write_atomically(&services.join(format!("{name}.service")), file.as_bytes())?;
            }
            None => {
                writeln!(
                    log,
                    "niri-computer-use: no activation file for {name} in {search:?}"
                )
                .ok();
            }
        }
    }
    Ok(())
}

/// The niri program the niri `pid` runs: the nested niri is the same build.
fn niri_program(pid: u32) -> Result<PathBuf, String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .map_err(|error| format!("read the host niri's program (PID {pid}): {error}"))?;
    Ok(exe
        .to_str()
        .and_then(|exe| exe.strip_suffix(" (deleted)"))
        .map_or_else(|| exe.clone(), PathBuf::from))
}

/// The socket of the niri `pid` in `runtime_dir`, and its display.
fn find_socket(runtime_dir: &Path, pid: u32) -> Option<(PathBuf, String)> {
    std::fs::read_dir(runtime_dir)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let name = entry.file_name();
            let display = plan::nested_display(name.to_str()?, pid)?.to_owned();
            Some((entry.path(), display))
        })
}

/// A log file for a child's stderr, appended to, the user's alone.
fn log_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))
}

/// The end of the log at `path`, for an error: the last `LOG_TAIL` bytes the child wrote,
/// or why they can't be read.
fn log_tail(path: &Path) -> String {
    let read = || -> std::io::Result<String> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(LOG_TAIL)))?;
        let mut bytes = Vec::new();
        file.take(LOG_TAIL).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
    };
    match read() {
        Ok(tail) if tail.is_empty() => "its log is empty".to_owned(),
        Ok(tail) => format!("its log ends: {tail}"),
        Err(error) => format!("its log {} can't be read: {error}", path.display()),
    }
}

fn detail(error: ToolError) -> String {
    error.detail
}

/// How a child ended, as an error says it.
fn exit(ended: Result<std::process::ExitStatus, String>) -> String {
    ended.map_or_else(|error| error, |status| format!("exited with {status}"))
}

/// Waits up to `BUS` for `daemon` to print its address, which it does once it listens, or
/// says why it didn't, with how it exited when it did.
async fn listening(daemon: &mut Owned) -> Result<(), String> {
    let mut reader = BufReader::new(daemon.stdout.take().ok_or("has no stdout")?);
    let mut line = String::new();
    let read = tokio::select! {
        read = tokio::time::timeout(BUS, reader.read_line(&mut line)) => read,
        ended = daemon.ended() => return Err(exit(ended)),
    };
    match read {
        Err(_) => Err(format!("gave no address within {BUS:?}")),
        Ok(Err(error)) => Err(format!("couldn't be read: {error}")),
        Ok(Ok(_)) if !line.trim().is_empty() => Ok(()),
        Ok(Ok(_)) => Err(tokio::time::timeout(EXIT, daemon.ended())
            .await
            .map_or_else(
                |_| "closed its stdout without an address".to_owned(),
                |ended| format!("closed its stdout and {}", exit(ended)),
            )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_start_quotes_the_end_of_the_log_the_child_wrote() {
        let dir = crate::test_support::fresh_dir("log-tail");
        let log = dir.join("niri.log");
        let mut text = "x".repeat(5000);
        text.push_str("\nError: failed to create the winit backend\n");
        std::fs::write(&log, &text).unwrap();
        let tail = log_tail(&log);
        assert!(
            tail.starts_with("its log ends: x") && tail.ends_with("winit backend"),
            "{tail}"
        );
        assert!(tail.len() <= "its log ends: ".len() + 2048, "{tail}");
        std::fs::write(&log, "").unwrap();
        assert_eq!(log_tail(&log), "its log is empty");
        assert!(log_tail(&dir.join("gone.log")).contains("can't be read"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
