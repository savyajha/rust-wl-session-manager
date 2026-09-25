# rust-wl-session-manager

A small systemd-based session manager for Wayland compositors. It starts the
compositor as a systemd user unit, manages the session's environment in the
systemd user manager, and ends the graphical session cleanly when the
compositor stops.

> **Please read before using**
>
> - This is a personal project, built for my own desktop. It is shared in case
>   it is useful to others, with no promise of support or stability.
> - It was written with the help of an AI assistant (Anthropic's Claude). Every
>   change was reviewed by me, and the behaviour is covered by the tests
>   described below, but you should review it yourself before relying on it.
> - It has only been tested with [niri](https://github.com/YaLTeR/niri), under
>   [greetd](https://sr.ht/~kennylevinsen/greetd/) on NixOS. It may work with
>   any compositor that meets the [requirements](#requirements), but no other
>   compositor has been tried.

## What it does

- Starts the compositor's own packaged systemd unit (e.g. `niri.service`) and
  waits for its start job. If the compositor fails to start, the session ends
  instead of hanging.
- Pushes a configured list of environment variables into the systemd user
  manager, after taking a snapshot of its environment. When the session ends,
  variables added during the session are unset and changed or removed ones are
  restored, in one atomic call.
- Holds a logind shutdown inhibitor in `delay` mode, so a logout is not cut
  off by a shutdown. On shutdown it releases the lock at once and lets systemd
  stop the session. Sleep is not inhibited.
- Watches the compositor unit, and ends the session when the compositor stops
  for good: a quit, a crash, or a logout. An explicit
  `systemctl --user restart` of the compositor does not end the session.
- Ends the session explicitly, by starting a shutdown unit that conflicts with
  `graphical-session.target`, so the target and its clients stop even when a
  unit such as `xdg-desktop-portal.service` would otherwise keep it up.

The reasoning behind each of these is in [DESIGN.md](DESIGN.md).

Compared with niri's own `niri-session` script, which starts `niri.service`
and waits for it to exit, rust-wl-session-manager adds the environment snapshot and
restore, the shutdown inhibitor, explicit teardown of `graphical-session.target`,
and handling for a compositor that fails to start.

## Requirements

- Linux with systemd (tested with systemd 261) and logind.
- A compositor with a systemd user unit that:
  - is bound to `graphical-session.target` (`BindsTo=` and `Before=`), and
  - finishes its start job only once the compositor is ready (e.g.
    `Type=notify`).
- A unit that `Conflicts=` `graphical-session.target`, used to end the session.
  niri ships `niri.service` and `niri-shutdown.target`, which meet both
  requirements.
- A display manager or greeter that starts sessions from
  `wayland-sessions/*.desktop` files.

## Configuration

rust-wl-session-manager takes exactly one argument, the path to a TOML config file:

```sh
rust-wl-session-manager --config /path/to/config.toml
```

```toml
# Variables copied from rust-wl-session-manager's own environment into the systemd
# user manager. Unset variables are skipped with a warning.
env_vars = [
  "XDG_SESSION_ID",
  "XDG_RUNTIME_DIR",
  "XDG_SEAT",
  "XDG_VTNR",
  "DBUS_SESSION_BUS_ADDRESS",
  "PATH",
]

# The compositor's systemd user unit.
compositor_service = "niri.service"

# A unit that Conflicts= graphical-session.target; starting it ends the session.
compositor_shutdown = "niri-shutdown.target"
```

Unknown keys are rejected.

## Installation

### Nix (flake)

The flake provides the package as `packages.<system>.default`. It includes a
session file, `wayland-sessions/niri-rust-wl.desktop`, which runs
rust-wl-session-manager with the config file given by the `configFile` override.

A NixOS example:

```nix
{ inputs, pkgs, ... }:
let
  configFile = (pkgs.formats.toml { }).generate "rust-wl-session-manager.toml" {
    env_vars = [
      "XDG_SESSION_ID"
      "XDG_RUNTIME_DIR"
      "XDG_SEAT"
      "XDG_VTNR"
      "DBUS_SESSION_BUS_ADDRESS"
      "PATH"
    ];
    compositor_service = "niri.service";
    compositor_shutdown = "niri-shutdown.target";
  };
  session = inputs.rust-wl-session-manager.packages.${pkgs.stdenv.hostPlatform.system}.default.override {
    configFile = "${configFile}";
  };
in
{
  programs.niri.enable = true;
  services.displayManager.sessionPackages = [ session ];
}
```

Then select "Niri (rust-wl-session-manager)" in your greeter.

### Other distributions

Build it with Cargo:

```sh
cargo build --release
```

Install `target/release/rust-wl-session-manager`, write a config file, and add a
session file such as `/usr/share/wayland-sessions/niri-rust-wl.desktop`:

```ini
[Desktop Entry]
Name=Niri (rust-wl-session-manager)
Exec=/usr/local/bin/rust-wl-session-manager --config /etc/rust-wl-session-manager/config.toml
Type=Application
DesktopNames=niri
```

## Logs and exit status

rust-wl-session-manager logs to journald, or to stderr when journald is unavailable:

```sh
journalctl --user -t rust-wl-session-manager
```

It exits with 0 when the session ends normally, 1 when startup, the session or
teardown fails, and 2 on a command-line usage error.

## Testing

```sh
nix flake check
```

This runs `cargo fmt --check`, `cargo clippy -D warnings`, the unit tests, and
a NixOS VM test that runs the real binary against fake compositor and client
units. The VM test covers startup, restarts, crashes, clean quits, logout
ordering, environment restore, a compositor that fails to start, SIGTERM
during startup, and system shutdown. It needs KVM.

`cargo test` runs the unit tests alone.

## License

[MIT](LICENSE)
