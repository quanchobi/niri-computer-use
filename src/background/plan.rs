//! What a background desktop's start decides from what it has read, with no I/O: which
//! host workspace it takes and its name, the window rule and the rules file around it,
//! the private bus's config and activation files, the nested niri's config and
//! environment, and the nested niri's display from its socket's name.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use niri_ipc::Workspace;
use serde::{Deserialize, Serialize};

use crate::policy::background::HostSize;

/// The file next to the host's niri config that holds the start's window rule.
pub(crate) const RULES_FILE: &str = "niri-computer-use.kdl";
/// The line the host's niri config needs, at its end, so the rule comes after the user's
/// own rules and wins.
pub(crate) const INCLUDE_LINE: &str = "include \"niri-computer-use.kdl\"";
/// The rules file between starts: only a comment. niri 26.04 refuses a config that
/// includes a missing file, so it is never removed.
pub(crate) const RULES_HEADER: &str =
    "// Managed by niri-computer-use. While a background desktop starts, the server
// briefly writes a window rule here that opens its nested niri on its own workspace
// without focusing it. Keep this file included last from config.kdl.
";

/// What every background desktop's workspace name starts with.
const PREFIX: &str = "agent-";
/// The most characters of the project's name a workspace name keeps.
const PROJECT_CHARS: usize = 32;

/// A host workspace's name for a background desktop: `agent-` and characters from
/// `[A-Za-z0-9._-]` only, so it needs no quoting in a config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct WorkspaceName(String);

impl WorkspaceName {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether niri takes `name` for this one: it compares workspace names without
    /// regard to ASCII case.
    pub(crate) fn names(&self, name: &str) -> bool {
        self.0.eq_ignore_ascii_case(name)
    }
}

/// The workspace a background desktop takes on `output`, the focused one: its last, which
/// is empty and unnamed, since niri always keeps an empty workspace there. Naming it never
/// moves the user's workspaces to other indices.
pub(crate) fn pick_workspace(workspaces: &[Workspace], output: &str) -> Result<u64, String> {
    let last = workspaces
        .iter()
        .filter(|workspace| workspace.output.as_deref() == Some(output))
        .max_by_key(|workspace| workspace.idx)
        .ok_or_else(|| format!("niri lists no workspace on the focused output {output}"))?;
    if last.name.is_some() || last.active_window_id.is_some() {
        return Err(format!(
            "the last workspace on {output}, {}, isn't empty and unnamed, as niri keeps it",
            last.idx
        ));
    }
    Ok(last.id)
}

/// The name for the server `server` working in `project`: `agent-<project>`, with what
/// `project` holds outside `[A-Za-z0-9._-]` as `-` and at most `PROJECT_CHARS` of it, or
/// `agent-<server>` without one, and `-2`, `-3`, … added while a workspace has the name.
pub(crate) fn workspace_name(
    project: Option<&str>,
    server: u32,
    workspaces: &[Workspace],
) -> WorkspaceName {
    let part: Option<String> = project
        .map(|project| {
            project
                .chars()
                .take(PROJECT_CHARS)
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        })
        .filter(|part| part.chars().any(|c| c.is_ascii_alphanumeric()));
    let base = format!("{PREFIX}{}", part.unwrap_or_else(|| server.to_string()));
    let taken = |name: &WorkspaceName| {
        workspaces
            .iter()
            .filter_map(|workspace| workspace.name.as_deref())
            .any(|other| name.names(other))
    };
    let mut name = WorkspaceName(base.clone());
    let mut suffix = 1_u32;
    while taken(&name) {
        suffix += 1;
        name = WorkspaceName(format!("{base}-{suffix}"));
    }
    name
}

/// The rules file while a start waits for its nested niri's window: the header, then a
/// rule that opens any nested niri's window on workspace `name`, floating, unfocused, at
/// `size`.
pub(crate) fn rules_with(name: &WorkspaceName, size: HostSize) -> String {
    format!(
        "{RULES_HEADER}
window-rule {{
    match app-id=\"^niri$\" title=\"^niri$\"
    open-on-workspace \"{}\"
    open-focused false
    open-floating true
    default-column-width {{ fixed {}; }}
    default-window-height {{ fixed {}; }}
}}
",
        name.as_str(),
        size.width,
        size.height
    )
}

/// The bus services a private bus may start: flatpak's portal, which
/// `flatpak-spawn --sandbox` needs, flatpak's session helper, which `flatpak run` needs,
/// and the accessibility bus's launcher, for the element tools. The other portals are left
/// out: their dialogs would open on the host's desktop.
pub(crate) const PRIVATE_SERVICES: [&str; 3] = [
    "org.freedesktop.portal.Flatpak",
    "org.freedesktop.Flatpak",
    "org.a11y.Bus",
];

/// Where activation files are looked for: `$XDG_DATA_HOME`, or `~/.local/share`, then
/// each of `$XDG_DATA_DIRS`, or `/usr/local/share:/usr/share`, each under `dbus-1/services`.
pub(crate) fn service_dirs(
    data_home: Option<&OsStr>,
    home: Option<&Path>,
    data_dirs: Option<&OsStr>,
) -> Vec<PathBuf> {
    let home_dir = data_home
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".local/share")));
    let dirs = data_dirs
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| OsStr::new("/usr/local/share:/usr/share"));
    home_dir
        .into_iter()
        .chain(std::env::split_paths(dirs).filter(|dir| dir.is_absolute()))
        .map(|dir| dir.join("dbus-1/services"))
        .collect()
}

/// The activation file for service `name` from `text`, one found for it, reduced to its
/// `Name` and `Exec`: without `SystemdService`, the private bus runs the service itself
/// rather than asking the host's systemd, which would start it on the host's bus. None when
/// `text` names another service or no command.
pub(crate) fn reduced_service(name: &str, text: &str) -> Option<String> {
    let mut section = false;
    let (mut named, mut exec) = (None, None);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line == "[D-BUS Service]";
            continue;
        }
        let Some((key, value)) = line
            .split_once('=')
            .filter(|_| section && !line.starts_with('#'))
        else {
            continue;
        };
        match key.trim() {
            "Name" => named = Some(value.trim()),
            "Exec" => exec = Some(value.trim()),
            _ => {}
        }
    }
    let exec = exec.filter(|exec| !exec.is_empty() && named == Some(name))?;
    Some(format!("[D-BUS Service]\nName={name}\nExec={exec}\n"))
}

/// The bytes a D-Bus address may hold unescaped.
fn address_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-_/.\\*".contains(&byte)
}

/// The D-Bus address of the Unix socket at `path`, its other bytes escaped as `%xx`.
pub(crate) fn unix_address(path: &Path) -> String {
    let mut address = "unix:path=".to_owned();
    for &byte in path.as_os_str().as_bytes() {
        if address_safe(byte) {
            address.push(char::from(byte));
        } else {
            write!(address, "%{byte:02x}").ok();
        }
    }
    address
}

/// `text` with XML's special characters escaped.
fn xml_escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A private session bus's config: one Unix socket at `socket`, EXTERNAL authentication,
/// the activation files in `services` alone, and every message allowed, as on a session bus
/// of one user.
pub(crate) fn bus_config(socket: &Path, services: &Path) -> String {
    format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{}</listen>
  <auth>EXTERNAL</auth>
  <servicedir>{}</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        xml_escaped(&unix_address(socket)),
        xml_escaped(&services.to_string_lossy())
    )
}

/// The nested niri's config: no hotkey overlay, client-side decorations or animations,
/// screenshots kept off disk, every window as wide as the screen with no gaps, ring or
/// border, and a few binds for a human. It binds no stop key: the host's stop key, which
/// the host niri takes before the nested window sees it, stops a background desktop's
/// agent through `stop`'s fan-out, and `status.stop_key` checks the host's config. A
/// nested bind would only fire from the agent's own `key` presses, or from Alt, the
/// nested Mod, rather than the user's host Mod.
pub(crate) const NESTED_CONFIG: &str = r#"// A background desktop's nested niri, written by niri-computer-use.

hotkey-overlay {
    skip-at-startup
}

prefer-no-csd

animations {
    off
}

screenshot-path null

layout {
    gaps 0
    center-focused-column "never"
    default-column-width { proportion 1.0; }
    focus-ring { off; }
    border { off; }
}

// Mod is Alt inside a nested niri.
binds {
    Mod+Q { close-window; }
    Mod+Left { focus-column-left; }
    Mod+Right { focus-column-right; }
    Mod+Up { focus-window-up; }
    Mod+Down { focus-window-down; }
    Mod+F { maximize-column; }
}
"#;

/// What the nested niri must not inherit: what would let its window take focus on the
/// host (an activation token), or reach the host's niri or X server.
pub(crate) const NIRI_REMOVED: [&str; 5] = [
    "XDG_ACTIVATION_TOKEN",
    "DESKTOP_STARTUP_ID",
    "NIRI_SOCKET",
    "DISPLAY",
    "WAYLAND_SOCKET",
];

/// What the nested niri gets beyond the server's environment: the host's display and
/// runtime directory, the bus its apps get, and a swap interval of 0 for both EGL
/// vendors, `vblank_mode=0` for Mesa's and `__GL_SYNC_TO_VBLANK=0` for NVIDIA's. Without
/// it the host's slow frame callbacks for a workspace not shown stall the nested niri's
/// loop whenever an app redraws, such as a terminal's blinking cursor.
pub(crate) fn niri_env(
    host_display: &OsStr,
    runtime_dir: &Path,
    bus: &str,
) -> Vec<(&'static str, OsString)> {
    vec![
        ("WAYLAND_DISPLAY", host_display.to_owned()),
        ("XDG_RUNTIME_DIR", runtime_dir.as_os_str().to_owned()),
        ("DBUS_SESSION_BUS_ADDRESS", bus.into()),
        ("vblank_mode", "0".into()),
        ("__GL_SYNC_TO_VBLANK", "0".into()),
    ]
}

/// What the private bus gets beyond the server's environment, and passes on to the
/// services it starts: the nested display, so their windows open on the background
/// desktop, the host's runtime directory, and its own address.
pub(crate) fn bus_env(
    nested_display: &str,
    runtime_dir: &Path,
    address: &str,
) -> Vec<(&'static str, OsString)> {
    vec![
        ("WAYLAND_DISPLAY", nested_display.into()),
        ("XDG_RUNTIME_DIR", runtime_dir.as_os_str().to_owned()),
        ("DBUS_SESSION_BUS_ADDRESS", address.into()),
    ]
}

/// The display of the niri `pid` whose socket is called `file_name`, which niri names
/// `niri.<display>.<pid>.sock`.
pub(crate) fn nested_display(file_name: &str, pid: u32) -> Option<&str> {
    let rest = file_name.strip_prefix("niri.")?.strip_suffix(".sock")?;
    let (display, owner) = rest.rsplit_once('.')?;
    (owner == pid.to_string() && !display.is_empty()).then_some(display)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(
        id: u64,
        idx: u8,
        output: &str,
        name: Option<&str>,
        window: Option<u64>,
    ) -> Workspace {
        Workspace {
            id,
            idx,
            name: name.map(str::to_owned),
            output: Some(output.to_owned()),
            is_urgent: false,
            is_active: false,
            is_focused: false,
            active_window_id: window,
        }
    }

    #[test]
    fn takes_the_focused_outputs_last_empty_unnamed_workspace() {
        let workspaces = [
            workspace(1, 1, "DP-1", None, Some(2)),
            workspace(12, 3, "DP-1", None, None),
            workspace(5, 2, "DP-1", Some("chat"), None),
            workspace(13, 2, "HDMI-A-1", None, None),
        ];
        assert_eq!(pick_workspace(&workspaces, "DP-1"), Ok(12));
        assert_eq!(pick_workspace(&workspaces, "HDMI-A-1"), Ok(13));
        assert!(pick_workspace(&workspaces, "eDP-1").is_err());
        let named_last = [workspace(1, 1, "DP-1", Some("mail"), None)];
        assert!(pick_workspace(&named_last, "DP-1").is_err());
        let full_last = [workspace(1, 1, "DP-1", None, Some(4))];
        assert!(pick_workspace(&full_last, "DP-1").is_err());
    }

    #[test]
    fn names_the_workspace_for_the_project_and_adds_a_number_while_taken() {
        let none: [Workspace; 0] = [];
        let name = |project, workspaces: &[Workspace]| {
            workspace_name(project, 4711, workspaces)
                .as_str()
                .to_owned()
        };
        assert_eq!(name(Some("shop"), &none), "agent-shop");
        assert_eq!(name(Some("my shop/é"), &none), "agent-my-shop--");
        assert_eq!(name(Some("a.b_c-d"), &none), "agent-a.b_c-d");
        let long = "x".repeat(40);
        assert_eq!(
            name(Some(&long), &none),
            format!("agent-{}", "x".repeat(32))
        );
        assert_eq!(name(None, &none), "agent-4711");
        assert_eq!(name(Some("--"), &none), "agent-4711");
        assert_eq!(name(Some(""), &none), "agent-4711");
        let taken = [
            workspace(1, 1, "DP-1", Some("Agent-Shop"), None),
            workspace(2, 2, "DP-1", Some("agent-shop-2"), None),
        ];
        assert_eq!(name(Some("shop"), &taken), "agent-shop-3");
        let shop = workspace_name(Some("shop"), 1, &none);
        assert!(shop.names("AGENT-shop") && !shop.names("agent-shop-2"));
    }

    #[test]
    fn the_rule_places_any_nested_niri_on_the_workspace_and_the_header_stays() {
        let size = HostSize {
            width: std::num::NonZeroU16::new(1280).unwrap(),
            height: std::num::NonZeroU16::new(800).unwrap(),
        };
        let rules = rules_with(&workspace_name(Some("shop"), 1, &[]), size);
        assert!(rules.starts_with(RULES_HEADER));
        for line in [
            "match app-id=\"^niri$\" title=\"^niri$\"",
            "open-on-workspace \"agent-shop\"",
            "open-focused false",
            "open-floating true",
            "default-column-width { fixed 1280; }",
            "default-window-height { fixed 800; }",
        ] {
            assert!(rules.contains(line), "{line}: {rules}");
        }
        assert!(RULES_HEADER.lines().all(|line| line.starts_with("//")));
    }

    #[test]
    fn reduces_an_activation_file_to_its_name_and_command() {
        let a11y = "[D-BUS Service]\nName=org.a11y.Bus\nExec=/usr/libexec/at-spi-bus-launcher\nSystemdService=at-spi-dbus-bus.service\n";
        assert_eq!(
            reduced_service("org.a11y.Bus", a11y).as_deref(),
            Some("[D-BUS Service]\nName=org.a11y.Bus\nExec=/usr/libexec/at-spi-bus-launcher\n")
        );
        assert_eq!(reduced_service("org.freedesktop.Flatpak", a11y), None);
        assert_eq!(
            reduced_service("org.a11y.Bus", "[D-BUS Service]\nName=org.a11y.Bus\n"),
            None
        );
        assert_eq!(
            reduced_service("org.a11y.Bus", "[Other]\nName=org.a11y.Bus\nExec=/x\n"),
            None
        );
        let commented = "[D-BUS Service]\n# Exec=/bad\nName = org.a11y.Bus\nExec = /good --flag\n";
        assert_eq!(
            reduced_service("org.a11y.Bus", commented).as_deref(),
            Some("[D-BUS Service]\nName=org.a11y.Bus\nExec=/good --flag\n")
        );
    }

    #[test]
    fn looks_for_activation_files_where_the_data_directories_are() {
        assert_eq!(
            service_dirs(None, Some(Path::new("/home/u")), None),
            ["/home/u/.local/share", "/usr/local/share", "/usr/share"]
                .map(|dir| PathBuf::from(dir).join("dbus-1/services"))
        );
        assert_eq!(
            service_dirs(
                Some(OsStr::new("/data")),
                Some(Path::new("/home/u")),
                Some(OsStr::new("/a:relative:/b"))
            ),
            ["/data", "/a", "/b"].map(|dir| PathBuf::from(dir).join("dbus-1/services"))
        );
    }

    #[test]
    fn the_bus_listens_on_its_one_socket_and_starts_only_its_services() {
        let config = bus_config(Path::new("/run/x y/bus"), Path::new("/run/a&b/services"));
        assert!(config.contains("<type>session</type>"));
        assert!(config.contains("<listen>unix:path=/run/x%20y/bus</listen>"));
        assert!(config.contains("<auth>EXTERNAL</auth>"));
        assert!(config.contains("<servicedir>/run/a&amp;b/services</servicedir>"));
        assert_eq!(unix_address(Path::new("/r/b,=s")), "unix:path=/r/b%2c%3ds");
    }

    #[test]
    fn the_nested_config_binds_no_stop_key() {
        assert!(!NESTED_CONFIG.contains("stop"), "{NESTED_CONFIG}");
        for line in [
            "screenshot-path null",
            "prefer-no-csd",
            "gaps 0",
            "skip-at-startup",
        ] {
            assert!(NESTED_CONFIG.contains(line), "{line}");
        }
    }

    #[test]
    fn reads_the_nested_display_from_its_socket_name() {
        assert_eq!(
            nested_display("niri.wayland-2.4242.sock", 4242),
            Some("wayland-2")
        );
        assert_eq!(nested_display("niri.wayland-2.4242.sock", 42), None);
        assert_eq!(
            nested_display("niri.my.display.7.sock", 7),
            Some("my.display")
        );
        assert_eq!(nested_display("niri..7.sock", 7), None);
        assert_eq!(nested_display("other.wayland-2.7.sock", 7), None);
        let bus = bus_env("wayland-2", Path::new("/run/user/1000"), "unix:path=/b");
        assert!(bus.contains(&("WAYLAND_DISPLAY", "wayland-2".into())));
        assert!(bus.contains(&("DBUS_SESSION_BUS_ADDRESS", "unix:path=/b".into())));
        let env = niri_env(
            OsStr::new("wayland-1"),
            Path::new("/run/user/1000"),
            "unix:path=/b",
        );
        assert!(env.contains(&("vblank_mode", "0".into())));
        assert!(env.contains(&("__GL_SYNC_TO_VBLANK", "0".into())));
        assert!(env.contains(&("WAYLAND_DISPLAY", "wayland-1".into())));
        assert!(
            NIRI_REMOVED.contains(&"NIRI_SOCKET") && NIRI_REMOVED.contains(&"XDG_ACTIVATION_TOKEN")
        );
    }
}
