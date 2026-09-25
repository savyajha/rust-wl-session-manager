# Design

session-manager starts a Wayland compositor as a systemd user unit, pushes
session environment variables into the systemd user manager, holds a logind
shutdown inhibitor, and ends the graphical session when the compositor stops
for good. This document records why it works the way it does.

## What ends a session

The compositor's packaged unit binds itself to `graphical-session.target`
(niri: `BindsTo=` and `Before=`), and session clients are `PartOf=` the target.
Stopping the target therefore stops the clients first and the compositor last.

session-manager watches the compositor unit, not the target. Every way a
session ends (a clean quit, a crash, or a logout that stops the target) ends
with the compositor stopped, so the compositor is the one reliable signal.

The target does not reliably stop by itself when the compositor dies.
`xdg-desktop-portal.service` is `Requisite=` the target and outlives the
compositor, which keeps the target "needed" and defeats its
`StopWhenUnneeded=`. session-manager therefore always ends the session
explicitly by starting the configured `compositor_shutdown` unit (for niri,
`niri-shutdown.target`), which `Conflicts=` the target.

Teardown then waits for the target and the compositor to stop before it
restores the environment, so the restore cannot race units that are still
stopping. This relies on the compositor unit being bound to the target (niri's
packaged unit has `BindsTo=`), so that stopping the target stops the
compositor and the wait is bounded.

## No restart policy for the compositor

The compositor unit is used exactly as packaged, with no `Restart=` added.
Every Wayland client dies with the compositor, so restarting it would leave
an empty session with no clients. Any compositor exit ends the session.

## Restarts versus exits

An explicit `systemctl --user restart` of the compositor passes through
`inactive` with a start job still queued. The session is over only when the
compositor is `inactive` or `failed` and its `Job` property shows no queued
job. A restart that is dropped (for example, by a failed condition) clears the
job and so also ends the session.

At startup, session-manager subscribes to `JobRemoved` before `StartUnit` and
waits for its own start job. A result other than `done` ends the session.

## Shutdown inhibitor

session-manager takes a logind `shutdown` inhibitor in `delay` mode. When
logind sends `PrepareForShutdown(true)`, session-manager releases the lock and
exits without making any D-Bus calls: logind's delay budget
(`InhibitDelayMaxSec`) is 5 seconds by default, and systemd stops the session
units itself during shutdown.

Sleep is not inhibited. Suspend and resume leave the session untouched; ending
the session on `PrepareForSleep` would log the user out on every suspend.

## Environment

Before pushing anything, session-manager snapshots the user manager's
environment as a map from name to value. It then pushes the configured
`env_vars` from its own environment. Variables that are unset or not valid
UTF-8 are skipped with a warning, since D-Bus strings must be UTF-8.

At teardown, it compares the current environment with the snapshot. Variables
added during the session are unset, and variables that were changed or
removed are restored. Both happen in one atomic
`UnsetAndSetEnvironment` call.

If session-manager exits uncleanly (for example, it is killed), nothing is
restored. The leftover variables then become part of the next session's
snapshot and are kept after that session ends. This gap is accepted.

Only variable names are logged, never values.

## Logging

session-manager logs to journald, falling back to stderr when journald is
unavailable. Values are interpolated into the message text rather than attached
as structured fields, because `journalctl` shows only the message by default.

## Exit status

session-manager exits with 0 when the compositor exits, on SIGTERM, and on
system shutdown. It exits with 1 when startup, the session, or teardown fails.
Each error is logged once. Command-line usage errors exit with 2.
