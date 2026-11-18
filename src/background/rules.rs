//! The host's rules file, `niri-computer-use.kdl` beside the config the host niri loaded,
//! which a start writes its window rule into and restores to a comment afterwards. The
//! host's config must include it: the server creates the file when it is missing, but
//! never edits the user's config. Every read and write runs on a thread of its own with a
//! deadline, since the config may be on a stuck filesystem.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use super::plan::{INCLUDE_LINE, RULES_FILE, RULES_HEADER};
use super::record::write_atomically;
use crate::stop_key::Config;

/// For each read or write of the host's config files.
const DEADLINE: Duration = Duration::from_secs(2);

/// What `status` reports about the rules file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RulesStatus {
    /// Where it is, once the host niri's config is found.
    pub(crate) path: Option<PathBuf>,
    /// The config the host niri loaded, which must include it.
    pub(crate) config: Option<PathBuf>,
    /// Whether the host niri's config includes it; null when that can't be read.
    pub(crate) included: Option<bool>,
    pub(crate) error: Option<String>,
}

/// The rules file of the niri `pid`, created with only the header when it is missing,
/// once the config niri loaded includes it.
pub(crate) async fn prepare(pid: u32) -> Result<PathBuf, String> {
    on_thread(move || {
        let config = Config::of_niri(pid)?;
        let path = beside(config.main());
        create(&path)?;
        let files = config
            .files()
            .map_err(|error| format!("read the host niri's config: {error}"))?;
        included(&files, config.main(), &path)?;
        Ok(path)
    })
    .await?
}

/// Where the rules file of the niri `pid` is and whether its config includes it, without
/// creating it.
pub(crate) async fn status(pid: u32) -> RulesStatus {
    let read = on_thread(move || {
        let config = Config::of_niri(pid)?;
        let path = beside(config.main());
        let included = config
            .files()
            .map(|files| included(&files, config.main(), &path).is_ok());
        Ok((path, config.main().to_path_buf(), included))
    })
    .await
    .and_then(|read| read);
    match read {
        Ok((path, config, Ok(included))) => RulesStatus {
            path: Some(path),
            config: Some(config),
            included: Some(included),
            error: None,
        },
        Ok((path, config, Err(error))) => RulesStatus {
            path: Some(path),
            config: Some(config),
            included: None,
            error: Some(format!("read the host niri's config: {error}")),
        },
        Err(error) => RulesStatus {
            path: None,
            config: None,
            included: None,
            error: Some(error),
        },
    }
}

/// Replaces the rules file at `path` with `text` at once.
pub(crate) async fn write(path: PathBuf, text: String) -> Result<(), String> {
    on_thread(move || write_atomically(&path, text.as_bytes())).await?
}

/// The rules file beside `main`.
fn beside(main: &Path) -> PathBuf {
    main.parent()
        .unwrap_or_else(|| Path::new("/"))
        .join(RULES_FILE)
}

/// Creates the rules file with only the header, unless it exists.
fn create(path: &Path) -> Result<(), String> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => write_atomically(path, RULES_HEADER.as_bytes()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("create {}: {error}", path.display())),
    }
}

/// Whether `files`, what the config `main` reads, include `rules`, or what to add.
fn included(files: &[PathBuf], main: &Path, rules: &Path) -> Result<(), String> {
    if files.iter().any(|file| file == rules) {
        return Ok(());
    }
    Err(format!(
        "{} doesn't include {}: add the line `{INCLUDE_LINE}` at the end of {}, so that the rule a background desktop's start writes there comes after your own rules and wins. niri 26.04 refuses a whole config that includes a missing file, so the server keeps {} between starts, with only a comment",
        main.display(),
        rules.display(),
        main.display(),
        rules.display()
    ))
}

/// `work`'s result, run on a thread of its own, or why there is none within `DEADLINE`.
/// A thread stuck past it is left to end on its own.
async fn on_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (sender, result) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("background rules".to_owned())
        .spawn(move || {
            sender.send(work()).ok();
        })
        .map_err(|error| format!("start a thread to read niri's config: {error}"))?;
    match tokio::time::timeout(DEADLINE, result).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err("the thread reading niri's config ended without an answer".to_owned()),
        Err(_) => Err(format!(
            "reading or writing niri's config took longer than {DEADLINE:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_file_must_be_included_and_the_error_gives_the_line() {
        let main = Path::new("/home/u/.config/niri/config.kdl");
        let rules = beside(main);
        assert_eq!(
            rules,
            Path::new("/home/u/.config/niri/niri-computer-use.kdl")
        );
        assert_eq!(
            included(&[main.to_path_buf(), rules.clone()], main, &rules),
            Ok(())
        );
        let error = included(&[main.to_path_buf()], main, &rules).unwrap_err();
        assert!(
            error.contains("add the line `include \"niri-computer-use.kdl\"` at the end of /home/u/.config/niri/config.kdl"),
            "{error}"
        );
        assert!(error.contains("refuses a whole config that includes a missing file"));
    }

    #[tokio::test]
    async fn creates_the_file_with_only_the_header_and_keeps_one_there() {
        let dir = crate::test_support::fresh_dir("rules-create");
        let path = dir.join(RULES_FILE);
        create(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), RULES_HEADER);
        write(path.clone(), "// mine\n".to_owned()).await.unwrap();
        create(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "// mine\n");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
