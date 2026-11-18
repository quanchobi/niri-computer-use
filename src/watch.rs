//! `watch`: what agents are doing on this niri instance, as JSON lines on stdout for a
//! shell to show. One `hello` line first, then `state` whenever the lease, the stop flag
//! the input-dirty marker, the agents' servers or the background desktops change, `action`
//! for each tool call the audit log records for this instance or one of its background
//! desktops, the last `BACKLOG` of them first, `live` for each record of the action in
//! progress (`crate::live`) on this instance, and `alive` every `ALIVE`, so a reader can
//! tell a quiet desktop from a watch that ended. The lines carry only what the audit log,
//! the lease records and the background desktops' records already hold, and for `live`
//! the `app_id` niri reports for the window an action is about, never typed text or window
//! titles. A background desktop's actions get no `live` lines: their coordinates are the
//! nested niri's, not the host's the overlay draws on.

mod clients;
mod run;
mod tail;
mod wake;

pub(crate) use run::run;

use serde::Serialize;
use serde_json::Value;

pub(crate) use clients::{Client, project_of};

use crate::control::lease::Holder;
use crate::policy::background::Bus;

/// The format's version, in `hello`. A reader refuses a version it doesn't know.
pub(crate) const FORMAT: u32 = 1;

/// How many recorded calls `watch` sends when it starts.
const BACKLOG: usize = 20;

/// The longest audit line read; a longer one is skipped.
const LINE_LIMIT: usize = 64 * 1024;

/// One line of `watch`'s output.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Event<'a> {
    Hello {
        format: u32,
        version: &'static str,
        instance: &'a str,
        /// Where the overlay publishes the robot's area (`overlay_area`), which this
        /// build's settle waits leave out; absent for a path that isn't UTF-8.
        #[serde(skip_serializing_if = "Option::is_none")]
        overlay: Option<&'a str>,
    },
    State(&'a State),
    Action(Value),
    Live(Value),
    Alive,
}

/// What the desk looks like now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct State {
    pub(crate) lease: Option<Driver>,
    pub(crate) stopped: bool,
    pub(crate) input_dirty: bool,
    /// Every agent's server on this instance.
    pub(crate) clients: Vec<Client>,
    /// The background desktops started from this instance.
    pub(crate) background: Vec<Background>,
}

/// A background desktop started from this instance, a nested niri on a host workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Background {
    /// The nested niri's socket's file name, as the audit log names its instance.
    pub(crate) instance: String,
    /// The host workspace its window is on.
    pub(crate) workspace: String,
    /// The session bus its apps get: `private` or `host`.
    pub(crate) bus: Bus,
    /// Who holds its lease, or is asking for it.
    pub(crate) lease: Option<Driver>,
    /// Whether its stop flag is set.
    pub(crate) stopped: bool,
}

/// Who holds the lease, or is asking for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Driver {
    /// Such as `claude-code/4711`, as in the audit log.
    pub(crate) session: String,
    pub(crate) since: String,
    /// The holder took the lock and waits for the user to allow it.
    pub(crate) pending: bool,
    /// The last part of the agent's working directory, such as the project's name.
    pub(crate) project: Option<String>,
}

impl Driver {
    pub(crate) fn new(holder: Holder, project: Option<String>) -> Self {
        Self {
            session: holder.label,
            since: holder.since,
            pending: holder.pending,
            project,
        }
    }
}

/// The server PID a session label such as `claude-code/4711` ends with.
pub(crate) fn label_pid(label: &str) -> Option<u32> {
    label.rsplit_once('/')?.1.parse().ok()
}

/// The parent PID in a `/proc/<pid>/status` text.
pub(crate) fn parent_pid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))?
        .trim()
        .parse()
        .ok()
}

/// The call an audit line records, if it is one on `instance` or on one of the
/// `background` desktops, without the instance; a background desktop's call gets
/// `background` with its workspace instead.
pub(crate) fn action(line: &[u8], instance: &str, background: &[Background]) -> Option<Value> {
    let mut value: Value = serde_json::from_slice(line).ok()?;
    let object = value.as_object_mut()?;
    let on = object.remove("instance")?;
    let on = on.as_str()?;
    if on != instance {
        let desktop = background.iter().find(|desktop| desktop.instance == on)?;
        object.insert("background".into(), desktop.workspace.clone().into());
    }
    Some(value)
}

/// Which record of which call a `live.json` holds: its `pid`, `call` and `phase`.
pub(crate) type LiveKey = (u64, u64, String);

/// The overlay's record in `bytes`, if it is one: a JSON object of at most `live::LIMIT`
/// bytes with a numeric `pid` and `call` and a string `phase`.
pub(crate) fn live(bytes: &[u8]) -> Option<(LiveKey, Value)> {
    if bytes.len() > crate::live::LIMIT {
        return None;
    }
    let record: Value = serde_json::from_slice(bytes).ok()?;
    let key = (
        record.get("pid")?.as_u64()?,
        record.get("call")?.as_u64()?,
        record.get("phase")?.as_str()?.to_owned(),
    );
    record.is_object().then_some((key, record))
}

/// Splits bytes read in pieces into whole lines, dropping a line longer than
/// `LINE_LIMIT` and keeping a last line without its newline for the next piece.
#[derive(Debug, Default)]
pub(crate) struct Lines {
    partial: Vec<u8>,
    overlong: bool,
}

impl Lines {
    pub(crate) fn feed(&mut self, piece: &[u8]) -> Vec<Vec<u8>> {
        let mut pieces = piece.split(|&byte| byte == b'\n');
        let last = pieces.next_back().unwrap_or_default();
        let lines = pieces.filter_map(|line| self.end(line)).collect();
        self.push(last);
        lines
    }

    /// Ends the line with its last bytes, returning it unless it was too long.
    fn end(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        self.push(bytes);
        let line = std::mem::take(&mut self.partial);
        (!std::mem::take(&mut self.overlong)).then_some(line)
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.partial.len() + bytes.len() > LINE_LIMIT {
            self.overlong = true;
            self.partial.clear();
        } else if !self.overlong {
            self.partial.extend_from_slice(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Vec<Vec<u8>> = Vec::new();

    #[test]
    fn a_line_split_across_reads_comes_out_once_and_whole() {
        let mut lines = Lines::default();
        assert_eq!(lines.feed(b"{\"a\":"), NONE);
        assert_eq!(lines.feed(b"1}\n{\"b\""), vec![b"{\"a\":1}".to_vec()]);
        assert_eq!(lines.feed(b":2}\n"), vec![b"{\"b\":2}".to_vec()]);
        assert_eq!(lines.feed(b""), NONE);
    }

    #[test]
    fn an_overlong_line_is_skipped_and_the_next_one_still_read() {
        let mut lines = Lines::default();
        let long = vec![b'x'; LINE_LIMIT / 2 + 1];
        assert_eq!(lines.feed(&long), NONE);
        assert_eq!(lines.feed(&long), NONE);
        assert_eq!(lines.feed(b"tail\nnext\n"), vec![b"next".to_vec()]);
    }

    #[test]
    fn only_calls_on_this_instance_or_its_background_desktops_are_actions() {
        let line = br#"{"ts":"t","session":"claude-code/7","instance":"niri.1.sock","tool":"key"}"#;
        let action = action(line, "niri.1.sock", &[]).unwrap();
        assert_eq!(action["tool"], "key");
        assert!(action.get("instance").is_none());
        assert!(action.get("background").is_none());
        assert_eq!(super::action(line, "niri.2.sock", &[]), None);
        assert_eq!(
            super::action(br#"{"tool":"key"}"#, "niri.1.sock", &[]),
            None
        );
        assert_eq!(super::action(b"not json", "niri.1.sock", &[]), None);
        let desktop = Background {
            instance: "niri.1.sock".to_owned(),
            workspace: "agent-shop".to_owned(),
            bus: Bus::Private,
            lease: None,
            stopped: false,
        };
        let nested = super::action(line, "niri.2.sock", &[desktop]).unwrap();
        assert_eq!(nested["background"], "agent-shop");
        assert!(nested.get("instance").is_none());
    }

    #[test]
    fn a_live_record_needs_its_key_and_fits_the_limit() {
        let record = br#"{"pid":7,"call":2,"phase":"aim","tool":"click"}"#;
        let (key, value) = live(record).unwrap();
        assert_eq!(key, (7, 2, "aim".to_owned()));
        assert_eq!(value["tool"], "click");
        assert_eq!(live(br#"{"pid":7,"phase":"aim"}"#), None);
        assert_eq!(live(b"{\"pid\":7,"), None);
        let long = format!(
            r#"{{"pid":7,"call":2,"phase":"aim","pad":"{}"}}"#,
            "x".repeat(5000)
        );
        assert_eq!(live(long.as_bytes()), None);
    }

    #[test]
    fn reads_the_server_pid_and_its_parent() {
        assert_eq!(label_pid("claude-code/4711"), Some(4711));
        assert_eq!(label_pid("odd name/x"), None);
        let status = "Name:\tniri-computer-u\nPid:\t4711\nPPid:\t4100\nTracerPid:\t0\n";
        assert_eq!(parent_pid(status), Some(4100));
        assert_eq!(parent_pid("Name:\tx\n"), None);
    }
}
