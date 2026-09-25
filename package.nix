{
  lib,
  rustPlatform,
  configFile ? "/etc/rust-wl-session-manager/config.toml",
}:

rustPlatform.buildRustPackage {
  pname = "rust-wl-session-manager";
  version = "0.1.0";

  src = ./.;

  cargoLock.lockFile = ./Cargo.lock;

  postInstall = ''
    install -Dm644 share/wayland-sessions/niri-rust-wl.desktop.in \
      $out/share/wayland-sessions/niri-rust-wl.desktop
    substituteInPlace $out/share/wayland-sessions/niri-rust-wl.desktop \
      --subst-var-by sessionManager "$out/bin/rust-wl-session-manager" \
      --subst-var-by configFile "${configFile}"
  '';

  passthru.providedSessions = [ "niri-rust-wl" ];

  meta = with lib; {
    description = "Minimal systemd-based session manager for Wayland compositors (tested with niri)";
    license = licenses.mit;
    platforms = platforms.linux;
    mainProgram = "rust-wl-session-manager";
  };
}
