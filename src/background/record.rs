//! A background desktop's record, `background.json`: everything its teardown needs, kept
//! up to date in the host work directory while it starts and, once it runs, also in the
//! nested instance's runtime directory, where its crash guardian looks. Processes are
//! named by their identity, so a PID reused after a reboot or a process's end is never
//! taken for the one recorded.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::plan::WorkspaceName;
use crate::control::procs;
use crate::policy::background::Bus;

/// The record's file name, in either directory.
pub(crate) const FILE: &str = "background.json";

/// One process for good: its PID, its start time, and the boot it started in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Ident {
    pub(crate) pid: u32,
    /// `/proc/<pid>/stat`'s field 22, in clock ticks since boot.
    pub(crate) start_time: u64,
    /// `/proc/sys/kernel/random/boot_id`.
    pub(crate) boot_id: String,
}

impl Ident {
    /// The process `pid` under `proc_root` as it is now; none once it has exited, as a
    /// zombie has, or when it can't be read.
    pub(crate) fn of(proc_root: &Path, pid: u32) -> Option<Self> {
        let stat = procs::stat(proc_root, pid).filter(|stat| !stat.exited())?;
        let boot = std::fs::read_to_string(proc_root.join("sys/kernel/random/boot_id")).ok()?;
        Some(Self {
            pid,
            start_time: stat.start_time,
            boot_id: boot.trim().to_owned(),
        })
    }

    /// Whether the process still runs: the same PID, start time and boot.
    pub(crate) fn alive(&self, proc_root: &Path) -> bool {
        Self::of(proc_root, self.pid).as_ref() == Some(self)
    }
}

/// What a background desktop's teardown undoes, as far as its start got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The server the background desktop belongs to.
    pub(crate) server: Ident,
    /// `<host runtime dir>/background/<id>/`.
    pub(crate) work_dir: PathBuf,
    /// The host niri's socket, where the workspace is named.
    pub(crate) host_socket: PathBuf,
    /// The host's rules file, once found.
    pub(crate) rules: Option<PathBuf>,
    /// Whether the rules file may still hold the start's window rule.
    pub(crate) rule_written: bool,
    /// The host workspace's name, set before niri is asked to name it.
    pub(crate) workspace: Option<WorkspaceName>,
    /// The ID of the host workspace named, which niri never gives another workspace.
    #[serde(default)]
    pub(crate) workspace_id: Option<u64>,
    pub(crate) bus: Bus,
    /// The address the nested niri's apps get.
    pub(crate) bus_address: Option<String>,
    /// The private bus's `dbus-daemon`.
    pub(crate) bus_daemon: Option<Ident>,
    /// The nested niri.
    pub(crate) niri: Option<Ident>,
    /// The nested niri's socket, resolved, which names its instance.
    pub(crate) nested_socket: Option<PathBuf>,
    /// The nested niri's Wayland display.
    pub(crate) nested_display: Option<String>,
}

impl Record {
    /// Writes the record to `dir`, replacing the one there at once.
    pub(crate) fn write(&self, dir: &Path) -> Result<(), String> {
        let text = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("write the background desktop's record: {error}"))?;
        write_atomically(&dir.join(FILE), &text)
    }

    /// The record in `dir`; none when there is no file.
    pub(crate) fn read(dir: &Path) -> Result<Option<Self>, String> {
        let path = dir.join(FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| format!("read {}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read {}: {error}", path.display())),
        }
    }
}

/// Tells apart the temporary files of writes made at once by this process.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `path` through a file beside it, renamed over it, so a reader sees
/// the old content or the new, never part of either. The file is the user's alone.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let write = WRITES.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_file_name(format!(".{name}.{}.{write}.tmp", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()));
    if let Err(error) = written.and_then(|()| std::fs::rename(&temporary, path)) {
        std::fs::remove_file(&temporary).ok();
        return Err(format!("write {}: {error}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
impl Record {
    /// A running desktop's record in the work directory `id` under `host`, as its start
    /// leaves it: also in the nested instance's runtime directory, with its nested socket
    /// `nested_socket` named in `instance`, on the workspace `agent-<project>`.
    pub(crate) fn running(
        host: &crate::control::runtime::RuntimeDir,
        id: &str,
        nested_socket: &Path,
        project: &str,
    ) -> Self {
        let work_dir = host.path().join(super::WORK_DIRS).join(id);
        std::fs::create_dir_all(&work_dir).unwrap();
        let record = Self {
            server: Ident {
                pid: 1,
                start_time: 1,
                boot_id: "boot".to_owned(),
            },
            work_dir,
            host_socket: "/run/user/1000/niri.wayland-1.5.sock".into(),
            rules: None,
            rule_written: false,
            workspace: Some(super::plan::workspace_name(Some(project), 1, &[])),
            workspace_id: Some(2),
            bus: Bus::Private,
            bus_address: Some("unix:path=/b".to_owned()),
            bus_daemon: None,
            niri: None,
            nested_socket: Some(nested_socket.to_path_buf()),
            nested_display: Some("wayland-2".to_owned()),
        };
        record.write(&record.work_dir).unwrap();
        let nested = crate::control::runtime::RuntimeDir::for_socket(nested_socket).unwrap();
        nested.create().unwrap();
        record.write(nested.path()).unwrap();
        let instance = nested_socket.file_name().unwrap().to_string_lossy();
        std::fs::write(record.work_dir.join("instance"), instance.as_bytes()).unwrap();
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/proc` with one process and a boot id.
    fn fake_proc(root: &Path, pid: u32, state: char, start: u64) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let fields = ["0"; 17].join(" ");
        std::fs::write(
            dir.join("stat"),
            format!("{pid} (n i (r) {state} 1 {fields} {start} 0 0\n"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("sys/kernel/random")).unwrap();
        std::fs::write(root.join("sys/kernel/random/boot_id"), "boot-1\n").unwrap();
    }

    #[test]
    fn a_process_is_alive_only_with_its_start_time_and_boot() {
        let root = crate::test_support::fresh_dir("record-ident");
        fake_proc(&root, 42, 'S', 900);
        let ident = Ident::of(&root, 42).unwrap();
        assert_eq!(
            ident,
            Ident {
                pid: 42,
                start_time: 900,
                boot_id: "boot-1".to_owned()
            }
        );
        assert!(ident.alive(&root));
        let reused = Ident {
            start_time: 901,
            ..ident.clone()
        };
        assert!(!reused.alive(&root));
        let other_boot = Ident {
            boot_id: "boot-0".to_owned(),
            ..ident
        };
        assert!(!other_boot.alive(&root));
        fake_proc(&root, 42, 'Z', 900);
        assert!(!ident.alive(&root));
        assert_eq!(Ident::of(&root, 43), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_record_reads_back_as_written() {
        let dir = crate::test_support::fresh_dir("record-file");
        assert_eq!(Record::read(&dir), Ok(None));
        let record = Record {
            server: Ident {
                pid: 7,
                start_time: 1,
                boot_id: "b".to_owned(),
            },
            work_dir: dir.join("work"),
            host_socket: "/run/user/1000/niri.wayland-1.5.sock".into(),
            rules: Some("/home/u/.config/niri/niri-computer-use.kdl".into()),
            rule_written: true,
            workspace: Some(super::super::plan::workspace_name(Some("shop"), 7, &[])),
            workspace_id: Some(12),
            bus: Bus::Private,
            bus_address: Some("unix:path=/b,guid=1".to_owned()),
            bus_daemon: None,
            niri: None,
            nested_socket: None,
            nested_display: None,
        };
        record.write(&dir).unwrap();
        assert_eq!(Record::read(&dir), Ok(Some(record)));
        let text = std::fs::read_to_string(dir.join(FILE)).unwrap();
        assert!(text.contains("\"workspace\": \"agent-shop\""), "{text}");
        assert!(text.contains("\"bus\": \"private\""), "{text}");
        std::fs::write(dir.join(FILE), "{").unwrap();
        assert!(Record::read(&dir).unwrap_err().contains(FILE));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary file is left"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
