//! niri-computer-use: an MCP server that lets AI agents observe a niri desktop.

mod a11y;
mod act;
mod apps;
mod audit;
mod background;
mod bridge;
mod changes;
mod cli;
mod clipboard;
mod confirm;
mod control;
mod coords;
mod discover;
mod elements;
mod engine;
mod error;
mod image_header;
mod input;
mod live;
mod niri;
mod noctalia;
mod notice;
mod observe;
mod overlay_area;
mod policy;
mod ppm;
mod refs;
mod runner;
mod save;
mod session;
mod settle;
mod status;
mod stop_key;
#[cfg(test)]
mod test_support;
mod tools;
mod wait;
mod watch;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::control::runtime::RuntimeDir;

const USAGE: &str = "usage: niri-computer-use serve | status [--check] | stop | resume | recover | watch | engine | guard <server-pid> | paste-keeper | --version";

/// What the server reads from its environment, once at startup. An empty variable
/// counts as unset.
#[derive(Debug, Clone, Default)]
pub(crate) struct Env {
    /// niri's IPC socket: resolved, when `instance` is.
    pub(crate) niri_socket: niri::Socket,
    /// The compositor instance, resolved once at startup, which names the runtime
    /// directory.
    pub(crate) instance: control::runtime::Instance,
    /// `loginctl` and `noctalia`, found by `read` and by no other constructor, so a test's
    /// environment never searches the host's directories.
    pub(crate) desktop: runner::Desktop,
    pub(crate) runtime_dir: Option<PathBuf>,
    pub(crate) wayland_display: Option<OsString>,
    /// The Wayland display's socket, checked against niri at each use.
    pub(crate) display: niri::Display,
    /// `$XDG_STATE_HOME`, or `$HOME/.local/state`, for the audit log.
    pub(crate) state_dir: Option<PathBuf>,
    /// The session bus, where the accessibility bus is looked up:
    /// `DBUS_SESSION_BUS_ADDRESS`, or else the user bus in the runtime directory.
    pub(crate) session_bus: Option<OsString>,
    /// Where the runtime directory, niri's socket and the display came from.
    pub(crate) discovery: discover::Sources,
    /// In a background desktop's environment, its host side: none otherwise.
    pub(crate) background: Option<std::sync::Arc<background::Host>>,
}

impl Env {
    async fn read(probe: discover::Probe) -> Self {
        let env = Self::from_vars(
            |name| std::env::var_os(name),
            &discover::Roots::host(),
            probe,
        )
        .await;
        let search = std::env::var_os("NIRI_COMPUTER_USE_DESKTOP_PATH")
            .filter(|search| !search.is_empty())
            .unwrap_or_else(|| runner::DESKTOP_PATH.into());
        Self {
            desktop: runner::Desktop::find(&search),
            ..env
        }
    }

    /// The environment `var` reads, with what it lacks of the session discovered under
    /// `roots` the way `probe` says. An empty variable counts as unset.
    async fn from_vars(
        var: impl Fn(&str) -> Option<OsString> + Sync,
        roots: &discover::Roots<'_>,
        probe: discover::Probe,
    ) -> Self {
        let var = |name| var(name).filter(|value| !value.is_empty());
        let given = discover::Given {
            runtime_dir: var("XDG_RUNTIME_DIR").map(PathBuf::from),
            niri_socket: var("NIRI_SOCKET").map(PathBuf::from),
            wayland_display: var("WAYLAND_DISPLAY"),
        };
        let session = discover::session(given, roots, probe).await;
        // Connections go to the resolved socket, so a symlink retargeted later doesn't move
        // them to another niri.
        let niri_socket = match (session.instance.socket(), session.niri_socket) {
            (Ok(resolved), _) => niri::Socket::at(resolved.to_path_buf()),
            (Err(_), found) => found.map_or_else(niri::Socket::unknown, niri::Socket::at),
        };
        let mut env = Self {
            background: None,
            niri_socket,
            instance: session.instance,
            desktop: runner::Desktop::default(),
            wayland_display: session.wayland_display,
            display: niri::Display::default(),
            // Without the variable, D-Bus clients (libdbus, sd-bus, zbus) use the user bus
            // systemd starts at `$XDG_RUNTIME_DIR/bus`.
            session_bus: var("DBUS_SESSION_BUS_ADDRESS").or_else(|| {
                session.runtime_dir.as_ref().map(|dir| {
                    let mut address = OsString::from("unix:path=");
                    address.push(dir.join("bus"));
                    address
                })
            }),
            runtime_dir: session.runtime_dir,
            state_dir: var("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".local/state"))),
            discovery: session.sources,
        };
        env.display = niri::Display::new(env.wayland_socket());
        env
    }

    /// A background desktop's environment: this process's, with niri's socket, the display
    /// and the session bus the nested niri's, the host's runtime directory, and the
    /// desktop's programs found for `host`. `link` is its host side. The process's own
    /// environment isn't changed, so the notifications it sends still reach the host's bus.
    async fn nested(
        host: &Self,
        socket: &std::path::Path,
        display: &str,
        bus: &str,
        link: std::sync::Arc<background::Host>,
    ) -> Self {
        let runtime_dir = host.runtime_dir.clone();
        let env = Self::from_vars(
            |name| match name {
                "NIRI_SOCKET" => Some(socket.into()),
                "WAYLAND_DISPLAY" => Some(display.into()),
                "DBUS_SESSION_BUS_ADDRESS" => Some(bus.into()),
                "XDG_RUNTIME_DIR" => runtime_dir.clone().map(PathBuf::into_os_string),
                _ => std::env::var_os(name),
            },
            &discover::Roots::host(),
            discover::Probe::Connect,
        )
        .await;
        Self {
            desktop: host.desktop.clone(),
            background: Some(link),
            ..env
        }
    }

    /// The file name of niri's socket, resolved when it could be, which names the compositor
    /// instance.
    pub(crate) fn instance(&self) -> Option<String> {
        self.niri_socket
            .path()
            .ok()
            .and_then(std::path::Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
    }

    /// The host's environment: a background desktop's host side, or this one on the live
    /// desktop. The user's lock state, Noctalia and stop key belong to the host.
    pub(crate) fn host(&self) -> &Self {
        self.background.as_ref().map_or(self, |link| &link.env)
    }

    /// The Wayland display's socket: `WAYLAND_DISPLAY`, under `XDG_RUNTIME_DIR` unless it
    /// is an absolute path.
    fn wayland_socket(&self) -> Result<PathBuf, String> {
        let display = std::path::Path::new(self.wayland_display.as_ref().ok_or_else(|| {
            match &self.discovery.wayland_display {
                discover::Source::Missing(detail) => detail.clone(),
                discover::Source::Environment | discover::Source::Discovered => {
                    "WAYLAND_DISPLAY is not set".to_owned()
                }
            }
        })?);
        if display.is_absolute() {
            return Ok(display.to_path_buf());
        }
        let runtime = self.runtime_dir.as_ref().ok_or_else(|| {
            format!(
                "XDG_RUNTIME_DIR is not set, so WAYLAND_DISPLAY {} names no socket",
                display.display()
            )
        })?;
        Ok(runtime.join(display))
    }

    /// The session variables as found, for the server's children.
    fn session_vars(&self) -> Vec<(&'static str, OsString)> {
        let socket = self
            .niri_socket
            .path()
            .ok()
            .map(|path| path.as_os_str().to_owned());
        [
            (
                "XDG_RUNTIME_DIR",
                self.runtime_dir.clone().map(PathBuf::into_os_string),
            ),
            ("NIRI_SOCKET", socket),
            ("WAYLAND_DISPLAY", self.wayland_display.clone()),
        ]
        .into_iter()
        .filter_map(|(name, value)| Some((name, value?)))
        .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// Serve MCP over stdin and stdout.
    Serve,
    /// Print the readiness report, as JSON or, with `--check`, as one line per
    /// prerequisite.
    Status { check: bool },
    /// Set the stop flag for this niri instance and every background desktop started
    /// from it.
    Stop,
    /// Clear those stop flags.
    Resume,
    /// Clear the input-dirty marker after ending the input child and asking the human.
    Recover,
    /// The shared engine a bridge starts: serve every client of this niri instance on its
    /// socket.
    Engine(engine::host::Start),
    /// The crash guardian `serve` starts: after the server's end, release what its marker
    /// names.
    Guard(u32),
    /// The clipboard keeper `paste` starts: hold the pasted text, then restore the
    /// clipboard.
    PasteKeeper,
    /// Print what agents are doing on this niri instance as JSON lines, for a shell.
    Watch,
    /// Print the version.
    Version,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let command = command(&args);
    if command == Some(Command::Version) {
        cli::say(concat!("niri-computer-use ", env!("CARGO_PKG_VERSION")));
        return ExitCode::SUCCESS;
    }
    // The flag commands must work while niri hangs, and pick only where a flag goes.
    let probe = match command {
        Some(Command::Stop | Command::Resume | Command::Watch) => discover::Probe::Offline,
        _ => discover::Probe::Connect,
    };
    let mut env = Env::read(probe).await;
    if let Some(Command::Engine(start)) = &command
        && start.runtime_dir_discovered
    {
        env.discovery.runtime_dir = discover::Source::Discovered;
    }
    let given = session::Given::read(|name| std::env::var_os(name));
    // A background desktop's server passes on its nested session's variables instead, once
    // the desktop runs: only the first call counts.
    if command != Some(Command::Serve) || !drives_background(&given) {
        runner::pass_on(env.session_vars());
    }
    if let Some(Command::Status { check }) = command {
        return status(&env, given, check).await;
    }
    let result = match command {
        Some(Command::Serve) => serve(env, given).await,
        Some(Command::Stop) => {
            RuntimeDir::of(&env).and_then(|runtime| control::flag::stop(&runtime))
        }
        Some(Command::Resume) => {
            RuntimeDir::of(&env).and_then(|runtime| control::flag::resume(&runtime))
        }
        Some(Command::Recover) => control::recover::run(&env).await,
        Some(Command::Watch) => watch::run(&env).await,
        Some(Command::Engine(start)) => engine::host::run(env, start.unscoped).await,
        Some(Command::Guard(server)) => control::guard::run(&env, server).await,
        Some(Command::PasteKeeper) => input::keeper::run(&env).await,
        Some(Command::Version | Command::Status { .. }) | None => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            cli::print_error(&message);
            ExitCode::FAILURE
        }
    }
}

/// The `status` subcommand: the report as JSON, or with `check` as text that fails while
/// anything blocks the server.
async fn status(env: &Env, given: session::Given, check: bool) -> ExitCode {
    let audit = audit::Audit::new(env.state_dir.clone());
    let settings = session::Settings::new(given);
    let accessibility = a11y::detect(env.session_bus.as_deref()).await;
    let sources = status::Sources {
        engine: None,
        event_stream: None,
        config_failed: niri::config_failed(&env.niri_socket).await.ok(),
        stop_key: Some(&stop_key::Reader::default()),
        lease_notice_error: None,
        audit: &audit,
        lease: control::desk::status_without_desk(env),
        policy: &settings.policy,
        unrestricted: &settings.unrestricted,
        confirm_lease: &settings.confirm_lease,
        tools: &settings.tools,
        shared: &settings.shared,
        background: &settings.background,
        place: settings.place(),
        apps: &settings.apps,
        accessibility: &accessibility,
        accessibility_switch: None,
    };
    let report = status::collect(env, sources).await;
    if check {
        let verdict = status::check::verdict(&report, &stop_key::own_program());
        cli::say(&verdict.text);
        return if verdict.blockers == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    match cli::print_json(&report) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            cli::print_error(&format!("print status: {error}"));
            ExitCode::FAILURE
        }
    }
}

fn command(args: &[OsString]) -> Option<Command> {
    match args {
        [only] => single(only.to_str()?),
        [status, flag] if status == "status" && flag == "--check" => {
            Some(Command::Status { check: true })
        }
        [engine, flag] if engine == "engine" && flag == engine::host::RUNTIME_DIR_DISCOVERED => {
            Some(Command::Engine(engine::host::Start {
                runtime_dir_discovered: true,
                unscoped: None,
            }))
        }
        [engine, flag, reason] if engine == "engine" && flag == engine::host::UNSCOPED => {
            Some(Command::Engine(engine::host::Start {
                unscoped: Some(reason.to_str()?.to_owned()),
                runtime_dir_discovered: false,
            }))
        }
        [guard, server] if guard == "guard" => server.to_str()?.parse().ok().map(Command::Guard),
        _ => None,
    }
}

/// A subcommand given alone.
fn single(arg: &str) -> Option<Command> {
    Some(match arg {
        "serve" => Command::Serve,
        "status" => Command::Status { check: false },
        "stop" => Command::Stop,
        "resume" => Command::Resume,
        "recover" => Command::Recover,
        "watch" => Command::Watch,
        "paste-keeper" => Command::PasteKeeper,
        "engine" => Command::Engine(engine::host::Start::default()),
        "--version" => Command::Version,
        _ => return None,
    })
}

/// Whether the client's `serve` drives a background desktop, or fails every call for one
/// it can't.
fn drives_background(given: &session::Given) -> bool {
    session::Settings::new(given.clone()).background.setting() != Ok(None)
}

/// Serves the client on stdin and stdout: from a background desktop of its own when its
/// settings say so, else through the shared engine in shared mode, unless it can't be
/// reached or refuses this client, and standalone otherwise. A client that requires the
/// shared engine is never served standalone: without it, every call fails with
/// `engine_unavailable` and tries the engine again, so the client is served once the cause
/// is gone. Decided before the client's first byte is read.
async fn serve(env: Env, given: session::Given) -> Result<(), String> {
    let settings = session::Settings::new(given.clone());
    if let Some(setting) = settings.background.setting().transpose() {
        return serve_background(env, given, &settings, setting).await;
    }
    let sharing = settings.shared.setting();
    let reached = match (&settings.shared.env, sharing) {
        (Err(note), _) => Err((None, note.clone())),
        (Ok(_), policy::Sharing::Off) => return standalone(env, settings, None).await,
        (Ok(_), policy::Sharing::On | policy::Sharing::Required) => {
            // The engine reports the setting decided here, from the whole policy file, which
            // the hello may not carry.
            let given = session::Given {
                shared: Some(sharing.variable().into()),
                ..given
            };
            reach_engine(&env, given).await
        }
    };
    let tools = tools::Server::listed(&settings);
    let log = bridge::log::Log::new(&env, &tools);
    let (target, reason) = match reached {
        Ok((target, link)) => return bridge::run(target, Some(link), None, log).await,
        Err(unreached) => unreached,
    };
    if sharing == policy::Sharing::Required {
        let Some(target) = target else {
            cli::print_error(&format!(
                "the shared engine is required, so every tool fails with engine_unavailable: {reason}"
            ));
            return bridge::unavailable::Unavailable::shared(tools, &reason, log)
                .run()
                .await;
        };
        cli::print_error(&format!(
            "the shared engine is required, so every call fails with engine_unavailable until one is reached: {reason}"
        ));
        let before = bridge::before::Before::new(tools);
        return bridge::run(target, None, Some(before), log).await;
    }
    cli::print_error(&format!("serving this client standalone: {reason}"));
    standalone(env, settings, Some(reason)).await
}

/// Serves the client on stdin and stdout from the background desktop `setting` describes,
/// started on the client's first call and torn down at its end, or on SIGTERM, SIGHUP or
/// SIGINT, before the crash guardian exists or after. Without one, every call fails with
/// `background_unavailable` and the reason: a server set to drive a background desktop
/// never drives the live one.
async fn serve_background(
    env: Env,
    given: session::Given,
    settings: &session::Settings,
    setting: Result<policy::background::Config, String>,
) -> Result<(), String> {
    let tools = background::listed(tools::Server::listed(settings));
    let log = bridge::log::Log::new(&env, &tools);
    let config = match setting {
        Ok(config) => config,
        Err(reason) => {
            cli::print_error(&format!(
                "the background desktop can't be used, so every tool fails with background_unavailable: {reason}"
            ));
            return bridge::unavailable::Unavailable::background(tools, &reason, log)
                .run()
                .await;
        }
    };
    let desktop = std::sync::Arc::new(background::Desktop::new(env, given, config));
    let target = bridge::Target::Background(std::sync::Arc::clone(&desktop));
    let before = bridge::before::Before::new(tools);
    let (served, signalled) = tokio::select! {
        served = bridge::run(target, None, Some(before), log) => (served, None),
        signal = terminated() => (Ok(()), Some(signal)),
    };
    let torn_down = desktop
        .finish()
        .await
        .map_err(|error| format!("tear down the background desktop: {error}"));
    if let Some(signal) = signalled {
        // Returning would wait for the thread reading stdin, which the client keeps open.
        cli::exit_at_once(match torn_down {
            Ok(()) => format!("{signal}: the server ends"),
            Err(error) => format!("{signal}: the server ends; {error}"),
        });
    }
    match (served, torn_down) {
        (Ok(()), torn_down) => torn_down,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(torn_down)) => Err(format!("{error}; {torn_down}")),
    }
}

/// The first of SIGTERM, SIGHUP and SIGINT, by name; never when they can't be watched,
/// which leaves them their default action.
async fn terminated() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut hangup), Ok(mut interrupt)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
        signal(SignalKind::interrupt()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = hangup.recv() => "SIGHUP",
        _ = interrupt.recv() => "SIGINT",
    }
}

/// The engine's target and a link to it, or why there is none, with the target when trying
/// again later may reach one.
async fn reach_engine(
    env: &Env,
    given: session::Given,
) -> Result<(bridge::Target, bridge::Link), (Option<bridge::Target>, String)> {
    let target = bridge::Target::new(env, given).map_err(|reason| (None, reason))?;
    match bridge::connect(&target).await {
        Ok(link) => Ok((target, link)),
        Err(bridge::Unreached::Never(reason)) => Err((None, reason)),
        Err(unreached) => Err((Some(target), unreached.detail())),
    }
}

/// Serves the one client on stdin and stdout. `fallback` says why it isn't served by the
/// shared engine it asked for.
async fn standalone(
    env: Env,
    settings: session::Settings,
    fallback: Option<String>,
) -> Result<(), String> {
    let engine = engine::Engine::start(env, status::Mode::Standalone, fallback).await?;
    let engine = std::sync::Arc::new(engine);
    let session = engine.open_session(std::process::id(), settings);
    engine::host::serve_alone(&engine, session, tokio::io::stdin(), tokio::io::stdout()).await
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn the_wayland_socket_is_under_the_runtime_directory_unless_absolute() {
        let env = |display: &str, runtime: Option<&str>| Env {
            wayland_display: Some(display.into()),
            runtime_dir: runtime.map(PathBuf::from),
            ..Env::default()
        };
        assert_eq!(
            env("wayland-1", Some("/run/user/1000")).wayland_socket(),
            Ok(PathBuf::from("/run/user/1000/wayland-1"))
        );
        assert_eq!(
            env("/tmp/w/wayland-9", None).wayland_socket(),
            Ok(PathBuf::from("/tmp/w/wayland-9"))
        );
        assert_eq!(
            env("wayland-1", None).wayland_socket(),
            Err("XDG_RUNTIME_DIR is not set, so WAYLAND_DISPLAY wayland-1 names no socket".into())
        );
        assert_eq!(
            Env::default().wayland_socket(),
            Err("WAYLAND_DISPLAY is not set".into())
        );
    }

    #[tokio::test]
    async fn the_session_bus_is_in_the_discovered_runtime_directory() {
        let root = test_support::fresh_dir("env-bus");
        let euid = rustix::process::geteuid().as_raw();
        let runtime = root.join(euid.to_string());
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let roots = discover::Roots {
            run_user: &root,
            proc: &root.join("proc"),
            euid,
        };
        let env = Env::from_vars(|_| None, &roots, discover::Probe::Connect).await;
        let mut bus = OsString::from("unix:path=");
        bus.push(runtime.join("bus"));
        assert_eq!(env.session_bus, Some(bus));
        assert_eq!(env.runtime_dir, Some(runtime));
        let given = Env::from_vars(
            |name| (name == "DBUS_SESSION_BUS_ADDRESS").then(|| "unix:path=/b".into()),
            &roots,
            discover::Probe::Connect,
        )
        .await;
        assert_eq!(given.session_bus, Some("unix:path=/b".into()));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A client that forwards niri's socket but not the runtime directory, for a niri
    /// whose runtime directory isn't the default one, such as a nested niri, and one that
    /// reaches both through another root that links to them.
    #[tokio::test]
    async fn servers_for_one_niri_share_its_lease_and_stop_flag_however_they_reach_it() {
        use crate::control::lease::Lease;
        use crate::control::runtime::RuntimeDir;

        let root = test_support::fresh_dir("env-nested");
        let euid = rustix::process::geteuid().as_raw();
        let default = root.join("run").join(euid.to_string());
        let nested = root.join("nested");
        for dir in [&default, &nested] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let roots = discover::Roots {
            run_user: &root.join("run"),
            proc: &root.join("proc"),
            euid,
        };
        let socket = nested.join("niri.wayland-2.42.sock");
        let _niri = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&nested, &alias).unwrap();
        let full = Env::from_vars(
            |name| match name {
                "XDG_RUNTIME_DIR" => Some(nested.clone().into()),
                "NIRI_SOCKET" => Some(socket.clone().into()),
                _ => None,
            },
            &roots,
            discover::Probe::Connect,
        )
        .await;
        let partial = Env::from_vars(
            |name| (name == "NIRI_SOCKET").then(|| socket.clone().into()),
            &roots,
            discover::Probe::Connect,
        )
        .await;
        let aliased = Env::from_vars(
            |name| match name {
                "XDG_RUNTIME_DIR" => Some(alias.clone().into()),
                "NIRI_SOCKET" => Some(alias.join("niri.wayland-2.42.sock").into()),
                _ => None,
            },
            &roots,
            discover::Probe::Connect,
        )
        .await;
        let (full, partial, aliased) = (
            RuntimeDir::of(&full).unwrap(),
            RuntimeDir::of(&partial).unwrap(),
            RuntimeDir::of(&aliased).unwrap(),
        );
        let _held = Lease::acquire(&full, "full").unwrap();
        assert!(Lease::acquire(&partial, "partial").is_err());
        assert!(Lease::acquire(&aliased, "aliased").is_err());
        full.stop().unwrap();
        assert!(partial.stopped().unwrap());
        assert!(aliased.stopped().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn takes_exactly_one_known_subcommand() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(command(&args(&["serve"])), Some(Command::Serve));
        assert_eq!(
            command(&args(&["status"])),
            Some(Command::Status { check: false })
        );
        assert_eq!(
            command(&args(&["status", "--check"])),
            Some(Command::Status { check: true })
        );
        assert_eq!(command(&args(&["stop"])), Some(Command::Stop));
        assert_eq!(command(&args(&["resume"])), Some(Command::Resume));
        assert_eq!(command(&args(&["watch"])), Some(Command::Watch));
        assert_eq!(command(&args(&["recover"])), Some(Command::Recover));
        assert_eq!(command(&args(&["guard", "42"])), Some(Command::Guard(42)));
        assert_eq!(
            command(&args(&["paste-keeper"])),
            Some(Command::PasteKeeper)
        );
        for bad in [
            &[][..],
            &["stop", "now"],
            &["serve", "status"],
            &["--help"],
            &["guard"],
            &["guard", "-1"],
            &["engine", "--unscoped"],
            &["engine", "--runtime-dir-discovered", "x"],
        ] {
            assert_eq!(command(&args(bad)), None, "{bad:?}");
        }
    }
}
