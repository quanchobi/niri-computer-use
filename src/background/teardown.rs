//! Undoing a background desktop, as far as its record says its start got: on the server's
//! normal end, by its crash guardian after the server's death, by the next start for a
//! server that died before it had a guardian, and by a start that failed part-way. Each
//! step runs whatever became of the ones before it, and every failure is reported.

use std::path::Path;
use std::time::Duration;

use niri_ipc::{Action, WorkspaceReferenceArg};
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use tokio::time::Instant;

use super::plan::{RULES_HEADER, WorkspaceName};
use super::record::{Ident, Record};
use super::{WORK_DIRS, rules};
use crate::control::runtime::RuntimeDir;
use crate::niri::listen::Listener;
use crate::runner::Owned;
use crate::{Env, niri};

const PROC: &str = "/proc";
/// How long a process gets to exit after `SIGTERM`, before `SIGKILL`.
const GRACE: Duration = Duration::from_secs(5);
/// How long a process gets to be gone after `SIGKILL`.
const KILLED: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(20);
/// The whole of a crash guardian's teardown.
const GUARDED: Duration = Duration::from_secs(30);
/// For the host niri to load its config once the rules file is restored.
const CONFIG_LOAD: Duration = Duration::from_secs(10);

/// The background desktop's processes the server holds handles on, when it holds them.
#[derive(Debug, Default)]
pub(crate) struct Children {
    pub(crate) niri: Option<Owned>,
    pub(crate) bus: Option<Owned>,
}

/// Undoes what `record` says was done: ends the nested niri and the private bus, through
/// `children` where they are held, takes the name off the host workspace named while it
/// still has it, restores the rules file while it may hold the rule, and removes the
/// nested instance's runtime directory and, unless a process didn't end, the work
/// directory, so that the next start's stale cleanup can try again.
pub(crate) async fn run(record: &Record, children: &mut Children) -> Result<(), String> {
    let mut failures = Vec::new();
    let niri = end(children.niri.as_mut(), record.niri.as_ref()).await;
    note(&mut failures, "end the nested niri", niri);
    let bus = end(children.bus.as_mut(), record.bus_daemon.as_ref()).await;
    note(&mut failures, "end the private bus", bus);
    let ended = failures.is_empty();
    if let (Some(name), Some(id)) = (&record.workspace, record.workspace_id) {
        let unnamed = unname(&record.host_socket, id, name).await;
        note(
            &mut failures,
            "take the name off the host workspace",
            unnamed,
        );
    }
    if let Some(path) = record.rules.as_ref().filter(|_| record.rule_written) {
        let restored = restore(path, &record.host_socket).await;
        note(&mut failures, "restore the rules file", restored);
    }
    if let Some(socket) = &record.nested_socket {
        let removed = RuntimeDir::for_socket(socket).and_then(|runtime| remove(runtime.path()));
        note(
            &mut failures,
            "remove the nested instance's runtime directory",
            removed,
        );
    }
    if ended {
        let removed = remove(&record.work_dir);
        note(&mut failures, "remove the work directory", removed);
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Adds `done`'s failure to `failures`, saying it was `what`.
fn note(failures: &mut Vec<String>, what: &str, done: Result<(), String>) {
    if let Err(error) = done {
        failures.push(format!("{what}: {error}"));
    }
}

/// Restores the rules file at `path`, then waits for the host niri on `host_socket` to
/// load it, so that a start which follows doesn't take that load for its own rule's.
async fn restore(path: &Path, host_socket: &Path) -> Result<(), String> {
    let listener = Listener::open(host_socket).await;
    rules::write(path.to_path_buf(), RULES_HEADER.to_owned()).await?;
    let mut listener =
        listener.map_err(|error| format!("watch the host niri load it: {}", error.detail))?;
    match listener.config_loaded(CONFIG_LOAD).await {
        Ok(false) => Ok(()),
        Ok(true) => Err(
            "the host niri refused its config once the rules file was restored; `niri validate` says why"
                .to_owned(),
        ),
        Err(error) => Err(format!("watch the host niri load it: {}", error.detail)),
    }
}

/// Tears down every background desktop under the host's `runtime` directory whose server
/// has ended, and removes a work directory a start left before it wrote any record.
/// Returns what failed.
pub(crate) async fn stale(runtime: &RuntimeDir) -> Vec<String> {
    let dir = runtime.path().join(WORK_DIRS);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => return vec![format!("read {}: {error}", dir.display())],
    };
    let mut failures = Vec::new();
    for work in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        let done = match Record::read(&work) {
            Ok(Some(record)) if !record.server.alive(Path::new(PROC)) => {
                run(&record, &mut Children::default()).await
            }
            Ok(Some(_)) => Ok(()),
            Ok(None) => remove(&work),
            Err(error) => Err(error),
        };
        if let Err(error) = done {
            failures.push(format!("{}: {error}", work.display()));
        }
    }
    failures
}

/// The crash guardian's teardown, once its server `server` has ended: the background
/// desktop whose record is in the runtime directory of the instance `env` names, the
/// nested one, when it is that server's.
pub(crate) async fn guarded(env: &Env, server: u32) -> Result<(), String> {
    let Ok(runtime) = RuntimeDir::of(env) else {
        return Ok(());
    };
    let Some(record) = Record::read(runtime.path())? else {
        return Ok(());
    };
    if record.server.pid != server {
        return Ok(());
    }
    tokio::time::timeout(GUARDED, run(&record, &mut Children::default()))
        .await
        .map_err(|_| {
            format!("the background desktop's teardown didn't finish within {GUARDED:?}")
        })?
}

/// Ends `held`, or else the process `recorded` names.
async fn end(held: Option<&mut Owned>, recorded: Option<&Ident>) -> Result<(), String> {
    match (held, recorded) {
        (Some(child), _) => child.stop(GRACE).await,
        (None, Some(ident)) => end_recorded(ident).await,
        (None, None) => Ok(()),
    }
}

/// Ends the process `ident` names while it is still that process: pinned by a pidfd opened
/// before its identity is checked, so a PID reused after the check names no process this
/// signals. `SIGTERM`, up to `GRACE` to exit, then `SIGKILL`.
async fn end_recorded(ident: &Ident) -> Result<(), String> {
    let proc_root = Path::new(PROC);
    let Some(pid) = i32::try_from(ident.pid).ok().and_then(Pid::from_raw) else {
        return Ok(());
    };
    let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(pidfd) => pidfd,
        Err(rustix::io::Errno::SRCH) => return Ok(()),
        Err(error) => return Err(format!("pidfd_open {}: {error}", ident.pid)),
    };
    for (signal, wait) in [(Signal::TERM, GRACE), (Signal::KILL, KILLED)] {
        if !ident.alive(proc_root) {
            return Ok(());
        }
        match pidfd_send_signal(&pidfd, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => return Err(format!("signal {}: {error}", ident.pid)),
        }
        let until = Instant::now() + wait;
        while ident.alive(proc_root) && Instant::now() < until {
            tokio::time::sleep(POLL).await;
        }
    }
    if ident.alive(proc_root) {
        return Err(format!(
            "process {} didn't exit within {KILLED:?} of SIGKILL",
            ident.pid
        ));
    }
    Ok(())
}

/// Takes the name off the host workspace `id`, if it still has `name`. niri never gives
/// another workspace its ID, so a workspace another start has named since is left alone.
async fn unname(host_socket: &Path, id: u64, name: &WorkspaceName) -> Result<(), String> {
    let socket = niri::Socket::at(host_socket.to_path_buf());
    let workspaces = niri::workspaces(&socket)
        .await
        .map_err(|error| error.detail)?;
    let named = workspaces.iter().any(|workspace| {
        workspace.id == id
            && workspace
                .name
                .as_deref()
                .is_some_and(|other| name.names(other))
    });
    if !named {
        return Ok(());
    }
    let action = Action::UnsetWorkspaceName {
        reference: Some(WorkspaceReferenceArg::Id(id)),
    };
    niri::carry_out(&socket, action)
        .await
        .map_err(|error| error.detail)
}

/// Removes `dir` and what it holds, unless it is gone already.
fn remove(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("remove {}: {error}", dir.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt as _;

    use crate::policy::background::Bus;

    /// A record of a start that got as far as writing its rule, by a server that has
    /// ended, whose host niri can't be reached.
    fn dead(runtime: &RuntimeDir, rules: &Path) -> Record {
        let work = runtime.path().join(WORK_DIRS).join("1-1");
        std::fs::create_dir_all(&work).unwrap();
        Record {
            server: Ident {
                pid: u32::MAX,
                start_time: 1,
                boot_id: "gone".to_owned(),
            },
            work_dir: work,
            host_socket: runtime.path().join("no-niri.sock"),
            rules: Some(rules.to_path_buf()),
            rule_written: true,
            workspace: None,
            workspace_id: None,
            bus: Bus::Host,
            bus_address: None,
            bus_daemon: None,
            niri: Some(Ident {
                pid: u32::MAX,
                start_time: 1,
                boot_id: "gone".to_owned(),
            }),
            nested_socket: None,
            nested_display: None,
        }
    }

    #[tokio::test]
    async fn a_dead_servers_desktop_is_torn_down_and_a_live_ones_kept() {
        let dir = crate::test_support::fresh_dir("teardown-stale");
        let runtime = RuntimeDir::for_socket(&dir.join("niri.wayland-1.5.sock")).unwrap();
        let rules = dir.join("niri-computer-use.kdl");
        std::fs::write(&rules, "window-rule {}\n").unwrap();
        let record = dead(&runtime, &rules);
        record.write(&record.work_dir).unwrap();
        let live = runtime.path().join(WORK_DIRS).join("2-2");
        std::fs::create_dir_all(&live).unwrap();
        Record {
            server: Ident::of(Path::new(PROC), std::process::id()).unwrap(),
            work_dir: live.clone(),
            rule_written: false,
            ..record.clone()
        }
        .write(&live)
        .unwrap();
        let empty = runtime.path().join(WORK_DIRS).join("3-3");
        std::fs::create_dir_all(&empty).unwrap();
        let failures = stale(&runtime).await;
        // The rules file is restored, though no niri answers to load it.
        assert!(
            matches!(failures.as_slice(), [only] if only.contains("watch the host niri load it")),
            "{failures:?}"
        );
        assert!(!record.work_dir.exists() && !empty.exists());
        assert!(live.exists());
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), RULES_HEADER);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn every_step_runs_and_every_failure_is_reported() {
        let dir = crate::test_support::fresh_dir("teardown-fails");
        let runtime = RuntimeDir::for_socket(&dir.join("niri.wayland-1.5.sock")).unwrap();
        let rules = dir.join("niri-computer-use.kdl");
        let record = Record {
            workspace: Some(super::super::plan::workspace_name(Some("shop"), 1, &[])),
            workspace_id: Some(1),
            rules: Some(dir.join("missing-dir").join("niri-computer-use.kdl")),
            ..dead(&runtime, &rules)
        };
        let error = run(&record, &mut Children::default()).await.unwrap_err();
        assert!(
            error.contains("take the name off the host workspace"),
            "{error}"
        );
        assert!(error.contains("restore the rules file"), "{error}");
        assert!(!record.work_dir.exists(), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A process whose identity no longer matches, as after its PID was reused, is never
    /// signalled.
    #[tokio::test]
    async fn a_process_whose_identity_differs_is_not_signalled() {
        let dir = crate::test_support::fresh_dir("teardown-reused");
        let log = std::fs::File::create(dir.join("log")).unwrap();
        let env = crate::runner::Exact {
            set: &[],
            remove: &[],
        };
        let args = ["30".to_owned()];
        let mut child =
            crate::runner::owned(Path::new("/bin/sleep"), &args, env, log, false).unwrap();
        let pid = child.pid().unwrap();
        let ident = Ident::of(Path::new(PROC), pid).unwrap();
        let reused = Ident {
            start_time: ident.start_time + 1,
            ..ident.clone()
        };
        assert_eq!(end_recorded(&reused).await, Ok(()));
        assert!(
            child.exited().is_none(),
            "a mismatched identity was signalled"
        );
        assert_eq!(end_recorded(&ident).await, Ok(()));
        assert!(child.ended().await.unwrap().signal().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A guardian tears down only its own server's desktop.
    #[tokio::test]
    async fn a_guardian_leaves_another_servers_desktop_alone() {
        let dir = crate::test_support::fresh_dir("teardown-guarded");
        let env = crate::test_support::niri_env(&dir);
        let nested = RuntimeDir::of(&env).unwrap();
        nested.create().unwrap();
        let rules = dir.join("niri-computer-use.kdl");
        let record = Record {
            rule_written: false,
            ..dead(&nested, &rules)
        };
        record.write(nested.path()).unwrap();
        assert_eq!(guarded(&env, 4242).await, Ok(()));
        assert!(
            record.work_dir.exists() && nested.path().join(super::super::record::FILE).exists()
        );
        assert_eq!(guarded(&env, record.server.pid).await, Ok(()));
        assert!(!record.work_dir.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
