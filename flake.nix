{
  description = "Minimal systemd-based session manager for niri";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        # Runs `command` on the source offline, with the package's vendored dependencies.
        cargoCheck =
          name: tools: command:
          pkgs.stdenv.mkDerivation {
            name = "session-manager-${name}";
            inherit (self.packages.${system}.niri-session-manager) src cargoDeps;
            nativeBuildInputs = [
              pkgs.rustPlatform.cargoSetupHook
              pkgs.cargo
            ]
            ++ tools;
            buildPhase = command;
            installPhase = "touch $out";
            dontFixup = true;
          };
      in
      {
        packages = rec {
          niri-session-manager = pkgs.callPackage ./package.nix { };
          default = niri-session-manager;
        };

        checks = {
          # Real "dry run": boots a VM, runs the actual binary against a fake
          # compositor, asserts the session-lifecycle semantics. Linux + KVM
          # only. Run with: nix build .#checks.<system>.session-lifecycle
          session-lifecycle = import ./tests/session-lifecycle.nix {
            inherit pkgs;
            sessionManager = self.packages.${system}.niri-session-manager;
          };

          fmt = cargoCheck "fmt" [ pkgs.rustfmt ] "cargo fmt --check";
          clippy = cargoCheck "clippy" [
            pkgs.rustc
            pkgs.clippy
          ] "cargo clippy --all-targets -- -D warnings";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.niri-session-manager ];
          packages = with pkgs; [
            rust-analyzer
            clippy
            rustfmt
          ];
        };
      }
    );
}
