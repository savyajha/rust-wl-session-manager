{
  lib,
  rustPlatform,
  configFile ? "/etc/niri-session-manager/config.toml",
}:

rustPlatform.buildRustPackage {
  pname = "niri-session-manager";
  version = "0.1.0";

  src = ./.;

  cargoLock.lockFile = ./Cargo.lock;

  postInstall = ''
    install -Dm644 share/wayland-sessions/niri-rust.desktop.in \
      $out/share/wayland-sessions/niri-rust.desktop
    substituteInPlace $out/share/wayland-sessions/niri-rust.desktop \
      --subst-var-by sessionManager "$out/bin/session-manager" \
      --subst-var-by configFile "${configFile}"
  '';

  passthru.providedSessions = [ "niri-rust" ];

  meta = with lib; {
    description = "Minimal systemd-based session manager for niri";
    license = licenses.mit;
    platforms = platforms.linux;
    mainProgram = "session-manager";
  };
}
