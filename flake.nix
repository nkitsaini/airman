{
  description = "airman: a high-quality, user-friendly, and robust TUI replacement for bluetoothctl and nmtui";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));
    in
    {
      # `nix develop` -> a shell with the Rust toolchain used to build/test this crate.
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            pkg-config
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
          ];

          shellHook = ''
            export RUST_BACKTRACE=1
          '';
        };
      });

      # `nix build` -> the release binary
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "airman";
          version = "0.1.0";
          # Keep local build artifacts and logs out of the Nix source closure.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "A high-quality TUI replacement for bluetoothctl and nmtui";
            mainProgram = "airman";
            license = with pkgs.lib.licenses; [ mit ];
          };
        };
      });

      # Focused, local QEMU validation of real daemon ownership and the
      # diagnostics path. This is intentionally not a CI workflow.
      checks = forAllSystems (pkgs: {
        daemon-integration = import ./tests/nixos.nix {
          inherit pkgs;
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      });
    };
}
