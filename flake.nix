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
      in
      {
        packages = rec {
          niri-session-manager = pkgs.callPackage ./package.nix { };
          default = niri-session-manager;
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
