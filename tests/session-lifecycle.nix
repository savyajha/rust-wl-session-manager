# NixOS VM test: a real "dry run" of the session lifecycle.
#
# No GUI and no real compositor. A dummy user service `niri.service` stands in
# for the compositor, wired to graphical-session.target exactly as niri does
# upstream (BindsTo=/Before=graphical-session.target). A dummy `ironbar.service`
# stands in for a Wayland-client bar (After=/PartOf=graphical-session.target).
# A dummy `portal.service` is Requisite= the target like xdg-desktop-portal,
# which keeps the target "needed" after the compositor dies. These dummies
# exercise the same systemd semantics a real session would. session-manager
# runs as the real binary against them.
#
# Behaviours asserted:
#   1. compositor unit is used as packaged: no runtime drop-in, no Restart=
#   2. inhibits shutdown, not sleep
#   3. explicit `systemctl restart` of the compositor -> session SURVIVES
#   4. explicit stop of gst         -> session ENDS (the logout path), ORDERED:
#                                      ironbar stops BEFORE niri (socket stays valid)
#   5. compositor crash             -> session ENDS: gst is stopped explicitly
#                                      even though a Requisite= unit (the fake
#                                      portal) keeps it "needed"
#   6. clean compositor quit        -> session ENDS (same)
#   7. shutdown/reboot              -> SystemInitiated: release inhibitor only
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
    # before gst, so it stops AFTER gst — i.e. last). No Restart=, like the
    # packaged niri unit: a compositor exit of any kind ends the session.
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

    # Fake portal, shaped like xdg-desktop-portal.service upstream: not a
    # Wayland client (so it outlives the compositor) and Requisite= the
    # session target, which keeps the target "needed" after the compositor is
    # gone and defeats its StopWhenUnneeded=. Without an explicit stop of the
    # target, a compositor exit would leave the session up and unusable.
    systemd.user.services.portal = {
      description = "Fake portal (test stand-in for xdg-desktop-portal)";
      wantedBy = [ "graphical-session.target" ];
      after = [ "graphical-session.target" ];
      partOf = [ "graphical-session.target" ];
      requisite = [ "graphical-session.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = mkFakeUnit "portal";
        KillSignal = "SIGTERM";
      };
    };

    # graphical-session.target — the anchor the session's units hang off.
    # Defined explicitly so it exists as a stoppable unit in the test.
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
        wait_active("portal.service")

    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("user@${toString uid}.service")

    with subtest("session-manager starts compositor, session target, and clients"):
        start_session()
        # The compositor unit is used exactly as packaged: session-manager
        # writes no runtime drop-in and adds no restart policy.
        machine.fail("test -e /run/user/${toString uid}/systemd/user/niri.service.d")
        restart = uctl("systemctl --user show -p Restart --value niri.service").strip()
        assert restart == "no", f"expected Restart=no on niri, got: {restart!r}"

    with subtest("inhibits shutdown (delay) but never sleep"):
        smline = machine.succeed(
            "COLUMNS=200 systemd-inhibit --list --no-legend --no-pager "
            "| grep session-manager"
        )
        assert "shutdown" in smline, f"expected a shutdown inhibitor, got: {smline}"
        assert smline.rstrip().endswith("delay"), f"expected mode=delay, got: {smline}"
        assert "sleep" not in smline, f"sleep must not be inhibited, got: {smline}"

    with subtest("explicit restart of the compositor -> session survives"):
        # `systemctl restart niri` is a stop + start in one job. The pending
        # start keeps graphical-session.target needed, so StopWhenUnneeded=
        # does not fire and the session carries on.
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

    with subtest("compositor crash -> session ends"):
        # The fake portal keeps gst "needed", so gst does NOT stop by itself.
        # session-manager must notice niri is gone and stop gst explicitly via
        # niri-shutdown.target; its PartOf= clients (ironbar, portal) stop with
        # it. No ordering to assert: niri is already dead before anything stops.
        start_session()
        uctl("systemctl --user kill --signal=SIGKILL niri.service")
        wait_inactive("session-manager.service")
        wait_inactive("graphical-session.target")
        wait_inactive("ironbar.service")
        wait_inactive("portal.service")

    with subtest("clean compositor quit -> session ends"):
        start_session()
        uctl("systemctl --user kill --signal=SIGUSR1 niri.service")
        wait_inactive("session-manager.service")
        wait_inactive("graphical-session.target")
        wait_inactive("ironbar.service")
        wait_inactive("portal.service")

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
