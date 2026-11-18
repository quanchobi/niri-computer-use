//! `stop` and `resume`: the stop flag of a niri instance and of every background desktop
//! started from it, so the host's stop key and the Noctalia plugin's Stop reach the agents
//! driving those desktops too. Each flag is set or cleared on its own, and every one that
//! can't be is reported after the others were.
//!
//! `stop` sets the host's flag first. A desktop whose start hasn't named its nested
//! instance yet isn't listed, but its start copies the host's flag right after naming it,
//! so it starts stopped either way.

use super::runtime::RuntimeDir;
use crate::background::registry::{self, Registered};

/// Sets the stop flag of `host` and of each background desktop started from it.
pub(crate) fn stop(host: &RuntimeDir) -> Result<(), String> {
    let mut failures = Vec::new();
    if let Err(error) = host.stop() {
        failures.push(format!(
            "set the stop flag in {}: {error}",
            host.path().display()
        ));
    }
    for desktop in registry::registered(host) {
        let set = desktop.and_then(|desktop| {
            desktop.nested.stop().map_err(|error| {
                format!(
                    "set the stop flag of the background desktop {} in {}: {error}",
                    desktop.name(),
                    desktop.nested.path().display()
                )
            })
        });
        failures.extend(set.err());
    }
    reported(&failures)
}

/// Clears the stop flag of `host` and of each background desktop started from it, except
/// where the input-dirty marker says input may be stuck.
pub(crate) fn resume(host: &RuntimeDir) -> Result<(), String> {
    let mut failures: Vec<String> = host.resume().err().into_iter().collect();
    for desktop in registry::registered(host) {
        failures.extend(desktop.and_then(|desktop| resumed(&desktop)).err());
    }
    reported(&failures)
}

fn resumed(desktop: &Registered) -> Result<(), String> {
    desktop.nested.resume().map_err(|error| {
        format!(
            "the background desktop {} stays stopped: {error}",
            desktop.name()
        )
    })
}

/// Every failure, one per line, or none.
fn reported(failures: &[String]) -> Result<(), String> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::record::Record;
    use crate::control::runtime::INPUT_DIRTY;

    /// A host instance in `dir` with background desktops on `projects`, and their nested
    /// instances' runtime directories.
    fn host_with(dir: &std::path::Path, projects: &[&str]) -> (RuntimeDir, Vec<RuntimeDir>) {
        let host = RuntimeDir::of(&crate::test_support::niri_env(dir)).unwrap();
        let nested = projects
            .iter()
            .enumerate()
            .map(|(index, project)| {
                let socket = dir.join(format!("niri.wayland-{}.{index}.sock", index + 2));
                Record::running(&host, &format!("{index}-1"), &socket, project);
                RuntimeDir::for_socket(&socket).unwrap()
            })
            .collect();
        (host, nested)
    }

    #[test]
    fn stop_and_resume_reach_every_background_desktop() {
        let dir = crate::test_support::fresh_dir("flag-fan-out");
        let (host, nested) = host_with(&dir, &["shop", "site"]);
        stop(&host).unwrap();
        assert!(host.stopped().unwrap());
        assert!(nested.iter().all(|nested| nested.stopped().unwrap()));
        resume(&host).unwrap();
        assert!(!host.stopped().unwrap());
        assert!(nested.iter().all(|nested| !nested.stopped().unwrap()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stop_sets_every_flag_it_can_and_names_the_one_it_couldnt() {
        let dir = crate::test_support::fresh_dir("flag-stop-fails");
        let (host, nested) = host_with(&dir, &["shop", "site"]);
        // A file where the first nested directory would be, so its flag can't be set.
        std::fs::remove_dir_all(nested[0].path()).unwrap();
        std::fs::write(nested[0].path(), "").unwrap();
        let error = stop(&host).unwrap_err();
        assert!(error.contains("background desktop agent-shop"), "{error}");
        assert!(!error.contains("agent-site"), "{error}");
        assert!(host.stopped().unwrap());
        assert!(nested[1].stopped().unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resume_clears_every_flag_but_one_whose_input_may_be_stuck() {
        let dir = crate::test_support::fresh_dir("flag-resume-dirty");
        let (host, nested) = host_with(&dir, &["shop", "site"]);
        stop(&host).unwrap();
        std::fs::write(nested[1].path().join(INPUT_DIRTY), "").unwrap();
        let error = resume(&host).unwrap_err();
        assert!(
            error.contains("background desktop agent-site stays stopped"),
            "{error}"
        );
        assert!(error.contains("recover"), "{error}");
        assert!(!host.stopped().unwrap());
        assert!(!nested[0].stopped().unwrap());
        assert!(nested[1].stopped().unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
