//! The background desktops started from one host instance, as their work directories in
//! the host's runtime directory list them. A desktop counts once its start has named its
//! nested instance there (`instance`), right after it wrote its record to the nested
//! instance's runtime directory, and until its teardown removes the work directory.
//! `stop` and `resume` reach the background desktops through this.

use std::io;
use std::path::Path;

use super::WORK_DIRS;
use super::record::Record;
use super::start::INSTANCE;
use crate::control::runtime::RuntimeDir;

/// A running background desktop, as its work directory names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registered {
    /// Its record, as kept in the host work directory.
    pub(crate) record: Record,
    /// The nested niri's socket's file name, which names its instance.
    pub(crate) instance: String,
    /// The nested instance's runtime directory, with its lease and stop flag.
    pub(crate) nested: RuntimeDir,
}

impl Registered {
    /// The host workspace it is on, or its nested instance while its record names none.
    pub(crate) fn name(&self) -> &str {
        self.record
            .workspace
            .as_ref()
            .map_or(&self.instance, |workspace| workspace.as_str())
    }
}

/// Every background desktop started from the host instance whose runtime directory is
/// `host`, in work directory order, or why one can't be read. None while no start has
/// made the directory that holds the work directories.
pub(crate) fn registered(host: &RuntimeDir) -> Vec<Result<Registered, String>> {
    let dirs = host.path().join(WORK_DIRS);
    let entries = match std::fs::read_dir(&dirs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => return vec![Err(format!("list {}: {error}", dirs.display()))],
    };
    let mut work_dirs: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    work_dirs.sort();
    work_dirs
        .iter()
        .filter_map(|work_dir| read(work_dir).transpose())
        .collect()
}

/// The desktop in `work_dir`; none before its start names its nested instance, or once
/// its teardown removed the directory.
fn read(work_dir: &Path) -> Result<Option<Registered>, String> {
    let path = work_dir.join(INSTANCE);
    let instance = match std::fs::read_to_string(&path) {
        Ok(instance) => instance,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let Some(record) = Record::read(work_dir)? else {
        return Ok(None);
    };
    let socket = record.nested_socket.as_deref().ok_or_else(|| {
        format!(
            "the record in {} names no nested socket, though {} names one",
            work_dir.display(),
            path.display()
        )
    })?;
    Ok(Some(Registered {
        nested: RuntimeDir::for_socket(socket)?,
        instance,
        record,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_the_desktops_whose_start_named_their_nested_instance() {
        let dir = crate::test_support::fresh_dir("registry");
        let host = RuntimeDir::of(&crate::test_support::niri_env(&dir)).unwrap();
        assert_eq!(registered(&host), Vec::new(), "no start made the directory");
        let nested = dir.join("niri.wayland-2.42.sock");
        let running = Record::running(&host, "1-1", &nested, "shop");
        let starting = Record::running(&host, "2-1", &dir.join("niri.wayland-3.43.sock"), "site");
        std::fs::remove_file(starting.work_dir.join(INSTANCE)).unwrap();
        let found = registered(&host);
        assert_eq!(
            found,
            vec![Ok(Registered {
                record: running.clone(),
                instance: "niri.wayland-2.42.sock".to_owned(),
                nested: RuntimeDir::for_socket(&nested).unwrap(),
            })]
        );
        assert_eq!(found[0].as_ref().unwrap().name(), "agent-shop");
        let broken = Record {
            nested_socket: None,
            ..running
        };
        broken.write(&broken.work_dir).unwrap();
        let error = registered(&host).remove(0).unwrap_err();
        assert!(error.contains("names no nested socket"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
