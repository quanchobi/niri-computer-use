---
title: First session
description: Bind the stop key, then let an agent look at your desktop and act on it.
sidebar:
  order: 4
---

## Bind the stop key

Before an agent acts on your desktop, give yourself a key that takes it back. Add a bind to your niri config (`~/.config/niri/config.kdl`):

```kdl
binds {
    Mod+Shift+Escape allow-inhibiting=false allow-when-locked=true hotkey-overlay-title="Stop the AI agent (niri-computer-use)" { spawn "/home/you/.local/bin/niri-computer-use" "stop"; }
}
```

Put the line inside your existing `binds` block, with the binary's full path: the install script prints this line with yours, and `cargo install` puts the binary in `~/.cargo/bin`. niri reloads its config when you save it; `niri validate` checks it first. `allow-inhibiting=false` keeps the key working while an app inhibits shortcuts, and `allow-when-locked=true` on the lock screen. niri passes `NIRI_SOCKET` to what it spawns, so the stop reaches this niri instance.

The key cancels the running action, takes the lease back and refuses further actions until you run, with the same full path:

```sh
~/.local/bin/niri-computer-use resume
```

After `cargo install` it's `~/.cargo/bin/niri-computer-use resume`.

Then check everything at once, with the same full path:

```sh
~/.local/bin/niri-computer-use status --check
```

Each line is a prerequisite with `✓` or `✗` and, under each `✗`, how to fix it. The stop key line turns `✓` once niri has loaded your bind and the program it names is there; it says `can't tell` when niri refused the config, the server couldn't read it, or the bind runs the program in a form the server doesn't read, such as through `systemd-run`. The command exits 1 while something blocks the server.

See [Safety](../../concepts/safety/#stop-and-resume) for what a stop does and doesn't undo.

## Look first

Start your agent and ask it something that only needs looking, for example:

> Use the niri-computer-use tools: call status, then tell me which windows are open on my focused workspace.

The agent calls `status`, then `desktop_state`. Neither needs the lease, and neither changes anything. Ask about something on screen, and it takes a `screenshot`, then a region screenshot if the text is small.

## Then act

Ask for something that changes the desktop:

> Focus my browser window, then put focus back where it was.

The agent calls `acquire_desktop`, which notes the window you were on, then `focus`, then `release_desktop` with `restore_focus: true`. Each action's result says whether niri accepted it and what niri saw happen.

If `acquire_desktop` fails, its error says why; the [error reference](../../reference/errors/) says what to do. The most common first-run refusal is `screen_locked` while the screen is unlocked; see [Troubleshooting](../../reference/troubleshooting/#the-lease-is-refused-with-screen_locked-while-the-screen-is-unlocked).

## Let it start apps

By default the agent can focus, close and type into windows you open, but not start new ones. The easy start is to let `launch` take your installed apps by their desktop entry id, such as `org.gnome.Nautilus` or `firefox`. Put this line in `~/.config/niri-computer-use/policy.toml`, creating the file if needed, and restart the agent's session:

```toml
launch_desktop_entries = true
```

The server then starts an app's own `Exec` line, the command its menu entry runs, without a shell, under the same rules as presets. It takes only apps installed for the whole system, such as those under `/usr/share`; an app installed in your home directory, as `flatpak install --user` does, needs a preset. See [`launch_desktop_entries`](../../concepts/configuration/#launch_desktop_entries) for what it refuses.

To allow only a few apps, list them as [presets](../../concepts/configuration/#preset) instead. [`unrestricted`](../../concepts/configuration/#unrestricted), or `NIRI_COMPUTER_USE_UNRESTRICTED=1` in a client's server settings, goes further: that client's agent can then start any program through `niri_action`'s `Spawn`.

## Work in a background desktop

By default the agent works on your desktop. To give it a desktop of its own instead, a nested niri on a workspace of its own that starts at the agent's first call and closes when its session ends, set up the rules file once, then turn background mode on.

While a background desktop starts, the server writes a window rule to `niri-computer-use.kdl` beside your niri config, which opens the nested niri's window on its workspace without focusing it. Create the file first: niri 26.04 refuses a whole config that includes a file that doesn't exist.

```sh
touch ~/.config/niri/niri-computer-use.kdl
```

Then add this line at the end of `~/.config/niri/config.kdl`, after your own window rules, so the server's rule wins:

```kdl
include "niri-computer-use.kdl"
```

`niri validate` checks it. The server never edits `config.kdl`. Between starts the file holds only a comment, and the server never removes it.

Then turn it on, for every client, with this table in `~/.config/niri-computer-use/policy.toml`:

```toml
[background]
```

or for one client, with `NIRI_COMPUTER_USE_BACKGROUND=private` in that client's server settings, or `host` for your session bus. Restart the agent's session. With the variable set, `status --check` shows the lines background mode adds:

```sh
NIRI_COMPUTER_USE_BACKGROUND=private ~/.local/bin/niri-computer-use status --check
```

```text
✓ background rules file: /home/you/.config/niri/niri-computer-use.kdl, included by /home/you/.config/niri/config.kdl
✓ dbus-daemon: /usr/bin/dbus-daemon, for the private bus
```

The agent's first call starts the desktop on a new workspace named after its project, such as `agent-niri-computer-use`, without moving your focus. Switch to that workspace to watch. The agent starts apps there as on your desktop, from presets and desktop entries. With the private bus, those apps get no keyring or portals, and a flatpak app's file dialog cancels: give a file to an app as an argument in its preset, or use `bus = "host"`. See [`[background]`](../../concepts/configuration/#background) for the settings and [Safety](../../concepts/safety/#background-desktops) for what a background desktop keeps apart and what it doesn't.
