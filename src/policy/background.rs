//! The policy file's `[background]` table and `NIRI_COMPUTER_USE_BACKGROUND`: whether
//! `serve` drives a background desktop of its own, a nested niri shown as one window on a
//! host workspace, instead of the live desktop, which session bus its apps get, and the
//! window's size. A setting that can't be read never falls back to the live desktop:
//! every call then fails with `background_unavailable`.

use std::ffi::OsStr;
use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};

use super::{Loaded, Sharing, Source};

/// Which session bus the background desktop's apps get.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Bus {
    /// A `dbus-daemon` of its own, so portals and single-instance hand-offs don't reach
    /// the host's apps.
    #[default]
    Private,
    /// The host's session bus, where the keyring and saved logins are.
    Host,
}

/// The nested niri's window size, in the host's logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HostSize {
    pub(crate) width: NonZeroU16,
    pub(crate) height: NonZeroU16,
}

/// The window size without a `[background]` table, or with one that sets none.
pub(crate) const DEFAULT_SIZE: HostSize = HostSize {
    width: NonZeroU16::MIN.saturating_add(1279),
    height: NonZeroU16::MIN.saturating_add(799),
};

/// The `[background]` table, whose presence turns background mode on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Table {
    #[serde(default)]
    pub(crate) bus: Bus,
    #[serde(default = "default_width")]
    pub(crate) width: NonZeroU16,
    #[serde(default = "default_height")]
    pub(crate) height: NonZeroU16,
}

const fn default_width() -> NonZeroU16 {
    DEFAULT_SIZE.width
}

const fn default_height() -> NonZeroU16 {
    DEFAULT_SIZE.height
}

impl Table {
    const fn size(self) -> HostSize {
        HostSize {
            width: self.width,
            height: self.height,
        }
    }
}

/// What the policy file says of background mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FromFile {
    /// No table, or no file: the live desktop.
    Absent,
    Table(Table),
    /// The file is invalid, and its text asks for a `[background]` table; the error says
    /// why it is invalid.
    Invalid(String),
}

impl Loaded {
    /// The file's `[background]` table; `Invalid` when the file is invalid and `source`,
    /// the text it was read from, asks for one (`asks_for_background`): a mistake in the
    /// file mustn't put an agent on the live desktop.
    pub(crate) fn background(&self, source: &Source) -> FromFile {
        match (self, source) {
            (Self::Valid(policy), _) => policy.background.map_or(FromFile::Absent, FromFile::Table),
            (Self::Invalid(error), Source::Text { text, .. }) if asks_for_background(text) => {
                FromFile::Invalid(error.clone())
            }
            (Self::Missing | Self::Invalid(_), _) => FromFile::Absent,
        }
    }
}

/// Whether `text` has a `background` table or key at its top level, whatever the rest of
/// it holds. Text that isn't TOML asks for it with a header line `[background]` or
/// `[background.…]`, or, before the first table header, a key `background` or
/// `background.…`.
fn asks_for_background(text: &str) -> bool {
    if let Ok(table) = toml::from_str::<toml::Table>(text) {
        return table.contains_key("background");
    }
    let mut top = true;
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .any(|line| {
            if let Some(header) = line.strip_prefix('[') {
                top = false;
                let name = header.trim_start_matches('[').trim_start();
                return starts_with_key(name, "]");
            }
            top && line
                .split_once('=')
                .is_some_and(|(key, _)| starts_with_key(key.trim(), ""))
        })
}

/// Whether `name` is `background`, followed by `end`, a `.` or the end of the text, with
/// spaces allowed before what follows.
fn starts_with_key(name: &str, end: &str) -> bool {
    name.strip_prefix("background").is_some_and(|rest| {
        let rest = rest.trim_start();
        rest.is_empty() || rest.starts_with('.') || (!end.is_empty() && rest.starts_with(end))
    })
}

/// What `NIRI_COMPUTER_USE_BACKGROUND` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wanted {
    Off,
    On(Bus),
}

/// A background desktop's settings, once decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) bus: Bus,
    pub(crate) size: HostSize,
}

/// Whether `serve` drives a background desktop, from the policy file's `[background]` and
/// `NIRI_COMPUTER_USE_BACKGROUND`, which wins on whether it does and on the bus, while the
/// size still comes from the file. `shared = "required"` conflicts with it: a background
/// desktop's engine runs in the server's own process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Background {
    pub(crate) policy: FromFile,
    /// The variable: none when unset or empty, and an error for any other value.
    pub(crate) env: Result<Option<Wanted>, String>,
    /// The shared setting the session decided.
    pub(crate) sharing: Sharing,
}

/// What `status` reports about the setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ConfigStatus {
    /// Whether this server drives a background desktop, or would if `error` didn't stop
    /// it.
    pub(crate) enabled: bool,
    pub(crate) bus: Bus,
    pub(crate) width: NonZeroU16,
    pub(crate) height: NonZeroU16,
    /// `policy`, `env` or `both`: what turned it on; null while off.
    pub(crate) source: Option<&'static str>,
    /// Why every call fails with `background_unavailable`.
    pub(crate) error: Option<String>,
}

impl Background {
    /// The variable's value, read as `Background::env` says.
    pub(crate) fn parse_env(value: Option<&OsStr>) -> Result<Option<Wanted>, String> {
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        match value.to_str() {
            Some("off") => Ok(Some(Wanted::Off)),
            Some("private") => Ok(Some(Wanted::On(Bus::Private))),
            Some("host") => Ok(Some(Wanted::On(Bus::Host))),
            _ => Err(format!(
                "NIRI_COMPUTER_USE_BACKGROUND is \"{}\"; it takes private, host or off, and a wrong value never falls back to the live desktop",
                value.display()
            )),
        }
    }

    /// The background desktop to drive; none for the live desktop; or why every call
    /// fails with `background_unavailable`.
    pub(crate) fn setting(&self) -> Result<Option<Config>, String> {
        let config = match (&self.env, &self.policy) {
            (Err(error), _) => return Err(error.clone()),
            (Ok(Some(Wanted::Off) | None), FromFile::Absent) | (Ok(Some(Wanted::Off)), _) => {
                return Ok(None);
            }
            (Ok(None), FromFile::Invalid(error)) => {
                return Err(format!(
                    "the policy file asks for a background desktop, but it is invalid: {error}"
                ));
            }
            (Ok(None), FromFile::Table(table)) => Config {
                bus: table.bus,
                size: table.size(),
            },
            (Ok(Some(Wanted::On(bus))), file) => Config {
                bus: *bus,
                size: match file {
                    FromFile::Table(table) => table.size(),
                    FromFile::Absent | FromFile::Invalid(_) => DEFAULT_SIZE,
                },
            },
        };
        if self.sharing == Sharing::Required {
            return Err(
                "a background desktop's engine runs in this server's own process, and shared = \"required\" (or NIRI_COMPUTER_USE_SHARED=required) asks for the shared engine; drop one of them".to_owned(),
            );
        }
        Ok(Some(config))
    }

    pub(crate) fn status(&self) -> ConfigStatus {
        let setting = self.setting();
        let from_file = matches!(self.policy, FromFile::Table(_) | FromFile::Invalid(_));
        let from_env = matches!(self.env, Ok(Some(Wanted::On(_))) | Err(_));
        let enabled = !matches!(setting, Ok(None));
        let source = match (
            enabled,
            from_file && self.env != Ok(Some(Wanted::Off)),
            from_env,
        ) {
            (false, _, _) | (true, false, false) => None,
            (true, true, true) => Some("both"),
            (true, true, false) => Some("policy"),
            (true, false, true) => Some("env"),
        };
        let table = match &self.policy {
            FromFile::Table(table) => Some(*table),
            FromFile::Absent | FromFile::Invalid(_) => None,
        };
        let size = table.map_or(DEFAULT_SIZE, Table::size);
        // The bus asked for, also when an error stops it, so `status --check` checks what
        // the server would run.
        let bus = match (&self.env, table) {
            (Ok(Some(Wanted::On(bus))), _) => *bus,
            (_, Some(table)) => table.bus,
            (_, None) => Bus::default(),
        };
        ConfigStatus {
            enabled,
            bus,
            width: size.width,
            height: size.height,
            source,
            error: setting.err(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decision for a policy file holding `text`, or none for `None`, the variable
    /// `env`, and the shared setting `sharing`.
    fn decided(text: Option<&str>, env: Option<&str>, sharing: Sharing) -> Background {
        let source = text.map_or(Source::Missing, |text| Source::Text {
            path: "/p.toml".into(),
            text: text.to_owned(),
        });
        Background {
            policy: Loaded::from_source(&source, false).background(&source),
            env: Background::parse_env(env.map(OsStr::new)),
            sharing,
        }
    }

    fn setting(text: Option<&str>, env: Option<&str>) -> Result<Option<Config>, String> {
        decided(text, env, Sharing::On).setting()
    }

    fn size(width: u16, height: u16) -> HostSize {
        HostSize {
            width: NonZeroU16::new(width).unwrap(),
            height: NonZeroU16::new(height).unwrap(),
        }
    }

    #[test]
    fn the_table_turns_it_on_and_the_variable_wins_on_whether_and_which_bus() {
        let private = Config {
            bus: Bus::Private,
            size: DEFAULT_SIZE,
        };
        assert_eq!(setting(None, None), Ok(None));
        assert_eq!(setting(Some("tools = \"act\""), None), Ok(None));
        assert_eq!(setting(Some("[background]"), None), Ok(Some(private)));
        let sized = "[background]\nbus = \"host\"\nwidth = 1600\nheight = 900";
        let host = Config {
            bus: Bus::Host,
            size: size(1600, 900),
        };
        assert_eq!(setting(Some(sized), None), Ok(Some(host)));
        assert_eq!(setting(Some(sized), Some("off")), Ok(None));
        // The variable picks the bus; the size still comes from the file.
        assert_eq!(
            setting(Some(sized), Some("private")),
            Ok(Some(Config {
                bus: Bus::Private,
                ..host
            }))
        );
        assert_eq!(
            setting(None, Some("host")),
            Ok(Some(Config {
                bus: Bus::Host,
                size: DEFAULT_SIZE,
            }))
        );
        assert_eq!(setting(None, Some("")), Ok(None));
    }

    #[test]
    fn a_wrong_variable_or_an_invalid_file_asking_for_it_never_falls_back_to_the_live_desktop() {
        let wrong = setting(None, Some("yes")).unwrap_err();
        assert!(
            wrong.contains("\"yes\"") && wrong.contains("never falls back"),
            "{wrong}"
        );
        assert!(setting(Some("[background]"), Some("Private")).is_err());
        for invalid in [
            "[background]\nbus = \"session\"",
            "[background]\nwidth = 0",
            "[background]\ncolour = 1",
            "background = 3",
            "capture_dir = \"shots\"\n[background]",
            "[[\n[background]",
            "[ background ] # spaced\n[[",
            "[background.extra]\n[[",
            "background.bus = \"host\"\n[[",
        ] {
            let error = setting(Some(invalid), None).unwrap_err();
            assert!(error.contains("/p.toml"), "{invalid}: {error}");
        }
        // The variable still decides on its own.
        assert_eq!(
            setting(Some("[background]\nwidth = 0"), Some("off")),
            Ok(None)
        );
        assert!(matches!(
            setting(Some("[background]\nwidth = 0"), Some("host")),
            Ok(Some(Config { bus: Bus::Host, .. }))
        ));
        for live in [
            "capture_dir = \"shots\"",
            "[[\nnot_background = 1",
            "[preset]\nbackground = 1\n[[",
            "[backgrounds]\n[[",
        ] {
            assert_eq!(setting(Some(live), None), Ok(None), "{live}");
        }
    }

    #[test]
    fn a_required_shared_engine_conflicts_with_it() {
        let conflict = decided(Some("[background]"), None, Sharing::Required)
            .setting()
            .unwrap_err();
        assert!(conflict.contains("shared = \"required\""), "{conflict}");
        assert_eq!(
            decided(Some("[background]"), Some("off"), Sharing::Required).setting(),
            Ok(None)
        );
        assert!(
            decided(None, None, Sharing::Off)
                .setting()
                .is_ok_and(|config| config.is_none())
        );
    }

    #[test]
    fn status_reports_the_bus_asked_for_while_a_conflict_stops_it() {
        let status = decided(Some("[background]"), Some("host"), Sharing::Required).status();
        assert!(status.error.is_some());
        assert_eq!(status.bus, Bus::Host);
    }

    #[test]
    fn status_says_what_turned_it_on_and_why_it_fails() {
        let status =
            |text, env| serde_json::to_value(decided(text, env, Sharing::On).status()).unwrap();
        assert_eq!(
            status(None, None),
            serde_json::json!({
                "enabled": false, "bus": "private", "width": 1280, "height": 800,
                "source": null, "error": null
            })
        );
        let file = Some("[background]\nbus = \"host\"\nheight = 700");
        assert_eq!(
            status(file, None),
            serde_json::json!({
                "enabled": true, "bus": "host", "width": 1280, "height": 700,
                "source": "policy", "error": null
            })
        );
        assert_eq!(status(file, Some("private"))["source"], "both");
        assert_eq!(status(file, Some("private"))["bus"], "private");
        assert_eq!(status(None, Some("host"))["source"], "env");
        let off = status(file, Some("off"));
        assert_eq!(
            (off["enabled"].clone(), off["source"].clone()),
            (false.into(), serde_json::Value::Null)
        );
        let wrong = status(None, Some("on"));
        assert_eq!(wrong["enabled"], true);
        assert_eq!(wrong["source"], "env");
        assert!(wrong["error"].as_str().unwrap().contains("\"on\""));
    }
}
