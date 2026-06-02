# NixOS VM test: a real "dry run" of the session lifecycle.
#
# No GUI and no real compositor. A dummy user service `niri.service` stands in
# for the compositor, wired to graphical-session.target exactly as niri does
# upstream (BindsTo=/Before=graphical-session.target). A dummy `ironbar.service`
# stands in for a Wayland-client bar (After=/PartOf=graphical-session.target).
# Because the architecture watches graphical-session.target rather than the
# compositor, these dummies exercise the same systemd semantics a real session
# would. session-manager runs as the real binary against them.
#
# Behaviours asserted:
#   1. single compositor crash      -> session SURVIVES (Restart=always)
#   2. clean compositor quit        -> session SURVIVES (respawn, no OnSuccess)
#   3. compositor crash-loop        -> session ENDS (OnFailure -> shutdown unit)
#   4. explicit stop of gst         -> session ENDS (the logout path)
#   5. shutdown/reboot              -> SystemInitiated: release inhibitor only
#   6. inhibits shutdown, not sleep
#   7. ORDERED teardown: ironbar stops BEFORE niri (socket stays valid)
#
# session-manager runs as a user service so its exit is the observable proxy
# for "teardown ran / session is over".

{ pkgs, sessionManager }:

let
  user = "alice";
  uid = 1000;

  configFile = pkgs.writeText "session-manager-config.toml" ''
    targets = [ "XDG_RUNTIME_DIR" ]
    compositor_service = "niri.service"
    compositor_shutdown = "niri-shutdown.target"
    start_limit_interval_sec = 60
    start_limit_burst = 3
  '';

  # Records a stop timestamp so the test can assert stop ORDER. Each unit
  # records on stop via a SIGTERM trap; `sleep & wait` keeps the trap
  # responsive. The timestamp uses bash's built-in $EPOCHREALTIME
  # (seconds.microseconds) — no external process and no PATH dependency, which
  # matters inside a systemd user service whose PATH excludes coreutils. The '.'
  # is stripped so the value is a plain integer for comparison.
  mkFakeUnit = name: pkgs.writeShellScript "fake-${name}" ''
    trap 'echo "${name} ''${EPOCHREALTIME/./}" >> /tmp/stop-order; exit 0' TERM
    # SIGUSR1 = clean self-exit (compositor "quit" path); only niri traps it.
    trap 'exit 0' USR1
    while true; do sleep 1 & wait $!; done
  '';
in
pkgs.testers.runNixOSTest {
  name = "session-manager-lifecycle";

  nodes.machine = { config, pkgs, ... }: {
    users.users.${user} = {
      isNormalUser = true;
      uid = uid;
      linger = true;
    };

    systemd.packages = [ sessionManager ];

    # The test user is lingering/seatless, so polkit would deny the shutdown
    # Inhibit() that session-manager makes (in production it runs inside the
    # greetd PAM session, which is allowed). Grant it explicitly.
    security.polkit.enable = true;
    security.polkit.extraConfig = ''
      polkit.addRule(function(action, subject) {
        if (action.id.indexOf("org.freedesktop.login1.inhibit") == 0
            && subject.user == "${user}") {
          return polkit.Result.YES;
        }
      });
    '';

    # Fake compositor, wired to graphical-session.target like niri upstream:
    # BindsTo (gst falls when niri is gone for good) + Before (niri starts
    # before gst, so it stops AFTER gst — i.e. last). NO Restart=/OnFailure=
    # here: session-manager injects those at runtime via the drop-in, so this
    # also proves the drop-in takes effect.
    systemd.user.services.niri = {
      description = "Fake compositor (test stand-in for niri)";
      unitConfig = {
        BindsTo = "graphical-session.target";
        Before = "graphical-session.target";
      };
      serviceConfig = {
        Type = "simple";
        ExecStart = mkFakeUnit "niri";
        KillSignal = "SIGTERM";
      };
    };

    # Fake Wayland-client bar: After=/PartOf=graphical-session.target, so it
    # starts after the target and — crucially — stops BEFORE niri.
    systemd.user.services.ironbar = {
      description = "Fake bar (test stand-in for ironbar)";
      wantedBy = [ "graphical-session.target" ];
      after = [ "graphical-session.target" ];
      partOf = [ "graphical-session.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = mkFakeUnit "ironbar";
        KillSignal = "SIGTERM";
      };
    };

    # graphical-session.target — the anchor session-manager watches. Defined
    # explicitly so it exists as a stoppable unit in the test.
    systemd.user.targets.graphical-session = {
      description = "Current graphical user session (test)";
    };

    # Shutdown trigger: Conflicts=graphical-session.target, so starting it stops
    # the target (mirrors niri-shutdown.target upstream). DefaultDependencies=no
    # + StopWhenUnneeded matches the real unit.
    systemd.user.targets.niri-shutdown = {
      description = "Fake compositor shutdown";
      unitConfig = {
        DefaultDependencies = false;
        StopWhenUnneeded = true;
        Conflicts = "graphical-session.target";
        After = "graphical-session.target";
      };
    };

    systemd.user.services.session-manager = {
      description = "Session manager under test";
      serviceConfig = {
        Type = "simple";
        ExecStart = "${sessionManager}/bin/session-manager --config ${configFile}";
      };
    };

    virtualisation.memorySize = 1024;
  };

  testScript = ''
    PREFIX = "sudo -u ${user} XDG_RUNTIME_DIR=/run/user/${toString uid} "

    def uctl(cmd):
        return machine.succeed(PREFIX + cmd)

    def wait_active(unit, timeout=30):
        machine.wait_until_succeeds(PREFIX + f"systemctl --user is-active {unit}", timeout=timeout)

    def wait_inactive(unit, timeout=30):
        machine.wait_until_fails(PREFIX + f"systemctl --user is-active {unit}", timeout=timeout)

    def is_active(unit):
        return uctl(f"systemctl --user is-active {unit}")

    def start_session():
        uctl("systemctl --user reset-failed")
        machine.succeed("rm -f /tmp/stop-order")
        uctl("systemctl --user start session-manager.service")
        wait_active("niri.service")
        wait_active("graphical-session.target")
        wait_active("ironbar.service")

    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("user@${toString uid}.service")

    with subtest("session-manager starts compositor, session target, and clients"):
        start_session()
        # The runtime drop-in must exist and carry policy, NOT anchor wiring.
        conf = "/run/user/${toString uid}/systemd/user/niri.service.d/50-session-manager.conf"
        machine.succeed(f"test -f {conf}")
        machine.succeed(f"grep -q 'Restart=always' {conf}")
        machine.succeed(f"grep -q 'RestartMode=direct' {conf}")
        machine.succeed(f"grep -q 'OnFailure=niri-shutdown.target' {conf}")
        machine.fail(f"grep -q 'compositor.target' {conf}")

    with subtest("inhibits shutdown (delay) but never sleep"):
        smline = machine.succeed(
            "COLUMNS=200 systemd-inhibit --list --no-legend --no-pager "
            "| grep session-manager"
        )
        assert "shutdown" in smline, f"expected a shutdown inhibitor, got: {smline}"
        assert smline.rstrip().endswith("delay"), f"expected mode=delay, got: {smline}"
        assert "sleep" not in smline, f"sleep must not be inhibited, got: {smline}"

    with subtest("single compositor crash -> session survives"):
        uctl("systemctl --user reset-failed niri.service")
        uctl("systemctl --user kill --signal=SIGKILL niri.service")
        wait_active("niri.service")
        is_active("graphical-session.target")
        is_active("session-manager.service")

    with subtest("clean compositor quit -> session survives (respawn, no logout)"):
        uctl("systemctl --user reset-failed niri.service")
        uctl("systemctl --user kill --signal=SIGUSR1 niri.service")
        wait_active("niri.service")
        is_active("graphical-session.target")
        is_active("session-manager.service")

    with subtest("explicit restart of the compositor -> session survives"):
        # `systemctl restart niri` is a clean stop + start. Because niri only
        # BindsTo=/Before= graphical-session.target (directional: gst stopping
        # stops niri, not the reverse) and there is no OnSuccess= handler,
        # restarting niri must NOT take the session down. Only a direct stop of
        # graphical-session.target ends the session.
        uctl("systemctl --user reset-failed niri.service")
        uctl("systemctl --user restart niri.service")
        wait_active("niri.service")
        is_active("graphical-session.target")
        is_active("session-manager.service")
        is_active("ironbar.service")

    with subtest("explicit stop of the session target -> ORDERED logout"):
        # The canonical logout path. Stopping graphical-session.target tears
        # down clients (ironbar) before the compositor (niri), per their
        # ordering deps. session-manager observes the target inactive and exits.
        machine.succeed("rm -f /tmp/stop-order")
        uctl("systemctl --user stop graphical-session.target")
        wait_inactive("session-manager.service")
        wait_inactive("niri.service")
        wait_inactive("ironbar.service")
        # ORDER ASSERTION: ironbar's stop timestamp must precede niri's, so the
        # bar never outlives the compositor's Wayland socket.
        order = machine.succeed("cat /tmp/stop-order")
        machine.log(f"stop-order contents:\n{order}")
        ts = {}
        for line in order.strip().splitlines():
            parts = line.split()
            if len(parts) == 2:
                ts[parts[0]] = int(parts[1])
        assert "ironbar" in ts and "niri" in ts, f"missing stop records: {order!r}"
        assert ts["ironbar"] < ts["niri"], (
            f"ironbar must stop before niri, got: {order!r}"
        )

    with subtest("crash-loop -> session ends via OnFailure (ordered)"):
        start_session()
        machine.succeed("rm -f /tmp/stop-order")
        # Crash faster than the start limit (burst=3 / 60s) so niri reaches the
        # failed state; OnFailure=niri-shutdown.target then stops gst.
        for _ in range(5):
            machine.execute(PREFIX + "systemctl --user kill --signal=SIGKILL niri.service")
            machine.sleep(1)
        wait_inactive("session-manager.service")
        wait_inactive("graphical-session.target")
        # Ordering still holds on the crash-loop logout path: ironbar stopped
        # via the gst teardown, before niri's final stop.
        order = machine.succeed("cat /tmp/stop-order")
        ts = {name: int(t) for name, t in (l.split() for l in order.strip().splitlines())}
        if "ironbar" in ts and "niri" in ts:
            assert ts["ironbar"] < ts["niri"], f"ironbar must stop before niri, got: {order}"

    with subtest("system shutdown -> SystemInitiated path, session comes back"):
        start_session()
        machine.succeed(
            "systemd-inhibit --list --no-legend --no-pager | grep -q session-manager"
        )

        machine.shutdown()
        machine.start()
        machine.wait_for_unit("multi-user.target")
        machine.wait_for_unit("user@${toString uid}.service")

        # Inspect only the session-manager instance alive at poweroff: the
        # previous boot's journal after its final "session started". (Earlier
        # subtests in that boot logged "running teardown", so a whole-boot grep
        # would be unsound.)
        prev = machine.succeed("journalctl -b -1 --no-pager")
        at_shutdown = prev.rsplit("session started", 1)[-1]
        assert "system shutdown signalled" in at_shutdown, (
            "expected SystemInitiated branch at poweroff; got:\n" + at_shutdown
        )
        assert "running teardown" not in at_shutdown, (
            "SystemInitiated path must skip teardown; got:\n" + at_shutdown
        )
  '';
}
