//! Background desktop mode: a server the user sets to it drives a nested niri of its own,
//! shown as one window on a host workspace of its own, instead of the live desktop, so
//! the user's focus, keyboard, pointer and layout aren't touched. It is started on the
//! client's first tool call, served by an engine in the server's own process, and torn
//! down when the client ends, or by the server's crash guardian when the server dies.
//!
//! The work for one start lives in `<host runtime dir>/background/<pid>-<start time>/`:
//! the record, the nested niri's config and log and, with a private bus, its config,
//! activation files, socket and log. `plan.rs` decides what the start writes and picks,
//! with no I/O; `start.rs` and `teardown.rs` do it and undo it; `rules.rs` handles the
//! host's rules file; `record.rs` keeps what teardown needs; `registry.rs` lists the
//! running desktops of a host instance, for `stop` and `resume`.

pub(crate) mod plan;
pub(crate) mod record;
pub(crate) mod registry;
pub(crate) mod rules;
mod start;
pub(crate) mod teardown;

use std::sync::Arc;

use rmcp::model::Tool;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::control::runtime::RuntimeDir;
use crate::engine::hello::{self, Build, Exe, Hello, Own};
use crate::engine::{Engine, host};
use crate::policy::background::Config;
use crate::session::Given;
use crate::{Env, control, runner, status};
use plan::WorkspaceName;
use start::Started;

/// The directory under the host instance's runtime directory that holds each start's
/// work directory.
pub(crate) const WORK_DIRS: &str = "background";

/// The tools Noctalia serves, which belong to the host's shell rather than the nested
/// desktop.
const HOST_SHELL_TOOLS: [&str; 2] = ["shell_panel", "noctalia"];

/// A started background desktop's host side, which its nested `Env` carries.
#[derive(Debug)]
pub(crate) struct Host {
    /// The host's environment, as the server read it.
    pub(crate) env: Env,
    /// The host workspace the nested niri's window is on.
    pub(crate) workspace: WorkspaceName,
    /// The nested niri's socket's file name, which names its instance.
    pub(crate) nested_instance: String,
}

/// `tools` without the ones that belong to the host's shell.
pub(crate) fn listed(tools: Vec<Tool>) -> Vec<Tool> {
    tools
        .into_iter()
        .filter(|tool| !HOST_SHELL_TOOLS.contains(&tool.name.as_ref()))
        .collect()
}

/// The background desktop of one server, for its one client: started by the first call
/// that reaches it, and kept until `finish`.
#[derive(Debug)]
pub(crate) struct Desktop {
    host: Env,
    given: Given,
    config: Config,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The started desktop and its environment.
    started: Option<(Started, Env)>,
    /// Its engine, once one started.
    served: Option<Served>,
    /// `finish` has run, so nothing starts again.
    finished: bool,
}

/// An engine serving the background desktop, what it compares a hello with, and the
/// hello the client's connections send.
#[derive(Debug, Clone)]
struct Served {
    engine: Arc<Engine>,
    own: Arc<Own>,
    hello: Vec<u8>,
}

impl Desktop {
    /// The background desktop `config` describes, of the server on the host `host`, whose
    /// client's environment gave `given`.
    pub(crate) fn new(host: Env, given: Given, config: Config) -> Self {
        Self {
            host,
            given,
            config,
            state: Mutex::new(State::default()),
        }
    }

    /// A connection to the desktop's engine and the hello to send on it, starting the
    /// desktop and its engine first unless they run. The start runs on a task of its own,
    /// so a client that ends meanwhile doesn't cut it short; `finish` tears down whatever
    /// it started.
    pub(crate) async fn connect(self: &Arc<Self>) -> Result<(UnixStream, Vec<u8>), String> {
        let desktop = Arc::clone(self);
        let served = tokio::spawn(async move { desktop.reach().await })
            .await
            .map_err(|error| format!("the background desktop's start ended: {error}"))??;
        let (bridge_end, engine_end) = UnixStream::pair()
            .map_err(|error| format!("make a connection to the engine: {error}"))?;
        tokio::spawn(host::connection(
            served.engine,
            served.own,
            engine_end,
            false,
        ));
        Ok((bridge_end, served.hello))
    }

    /// The engine, started with the desktop as needed. A desktop whose engine didn't start
    /// is kept for the next try, since the session variables passed on to children are
    /// its own for good. The state stays locked throughout, so one start runs at a time
    /// and `finish` waits for it.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the lock is held across the start on purpose, so starts and finish don't overlap"
    )]
    async fn reach(&self) -> Result<Served, String> {
        let mut state = self.state.lock().await;
        if let Some(served) = &state.served {
            return Ok(served.clone());
        }
        if state.finished {
            return Err("this server is ending".to_owned());
        }
        let (started, nested) = match state.started.take() {
            Some(up) => up,
            None => self.start().await?,
        };
        let served = Box::pin(self.serve(&nested)).await;
        state.started = Some((started, nested));
        let served = served?;
        state.served = Some(served.clone());
        Ok(served)
    }

    /// Starts the desktop and settles its record, or tears down what was started.
    async fn start(&self) -> Result<(Started, Env), String> {
        let didnt = |error: String| format!("the background desktop didn't start: {error}");
        let mut started = start::start(&self.host, self.config).await.map_err(didnt)?;
        match self.settle(&started).await {
            Ok(nested) => Ok((started, nested)),
            Err(error) => Err(didnt(
                match teardown::run(&started.record, &mut started.children).await {
                    Ok(()) => error,
                    Err(undoing) => format!("{error}; undoing the start: {undoing}"),
                },
            )),
        }
    }

    /// The nested environment of `started`, once its record is where its guardian looks.
    async fn settle(&self, started: &Started) -> Result<Env, String> {
        let link = Arc::new(Host {
            env: self.host.clone(),
            workspace: started.workspace.clone(),
            nested_instance: started
                .socket
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        });
        let nested = Env::nested(
            &self.host,
            &started.socket,
            &started.display,
            &started.bus,
            link,
        )
        .await;
        started.settle(&RuntimeDir::of(&self.host)?, &RuntimeDir::of(&nested)?)?;
        Ok(nested)
    }

    /// Passes the nested session's variables on to every child from now on, then starts
    /// its engine, whose crash guardian gets them.
    async fn serve(&self, nested: &Env) -> Result<Served, String> {
        runner::pass_on(nested.session_vars());
        let niri_socket = nested
            .instance
            .socket()
            .map_err(Clone::clone)?
            .to_path_buf();
        let display = nested.display.path().ok().map(hello::resolved);
        let exe = Exe::current()?;
        let hello = Hello::new(
            exe,
            &niri_socket,
            display.as_deref(),
            &nested.desktop.search,
            self.given.clone(),
        )?;
        let mut line =
            serde_json::to_vec(&hello).map_err(|error| format!("write the hello: {error}"))?;
        line.push(b'\n');
        let own = Own {
            pid: std::process::id(),
            exe,
            file: Build::current().exe,
            niri_socket,
            wayland_socket: display,
            desktop_path: nested.desktop.search.clone(),
        };
        let engine = Engine::start(nested.clone(), status::Mode::Standalone, None).await?;
        Ok(Served {
            engine: Arc::new(engine),
            own: Arc::new(own),
            hello: line,
        })
    }

    /// Ends the engine's sessions, waits for the input cleanups they left, then tears the
    /// desktop down. The engine, and with it the guardian's watch, goes only after, so the
    /// guardian finds nothing left to tear down.
    pub(crate) async fn finish(&self) -> Result<(), String> {
        let (served, started) = {
            let mut state = self.state.lock().await;
            state.finished = true;
            (state.served.take(), state.started.take())
        };
        if let Some(served) = &served {
            served.engine.end_sessions().await;
            control::cleanup::settled().await;
        }
        let Some((mut started, _nested)) = started else {
            return Ok(());
        };
        let done = teardown::run(&started.record, &mut started.children).await;
        drop(served);
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_shells_tools_are_not_listed() {
        let session = crate::test_support::session(1);
        let tools = crate::tools::Server::listed(session.settings());
        let names = |list: &[Tool]| -> Vec<String> {
            list.iter().map(|tool| tool.name.to_string()).collect()
        };
        assert!(names(&tools).contains(&"shell_panel".to_owned()));
        let listed = names(&listed(tools));
        assert!(!listed.contains(&"shell_panel".to_owned()));
        assert!(!listed.contains(&"noctalia".to_owned()));
        assert!(listed.contains(&"desktop_state".to_owned()));
    }
}
