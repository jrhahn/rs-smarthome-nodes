{
  description = "Async no_std ESP32-C3 smart-home sensor node firmware (Embassy); started as a bird-feeder scale";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, fenix, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Reuse the existing rust-toolchain.toml as the single source of
        # truth: it pins Rust 1.83.0 (required for esp-wifi 0.11's c_char
        # bindings) plus the riscv32imc-unknown-none-elf target.
        rustToolchain = fenix.packages.${system}.fromToolchainFile {
          file = ./rust-toolchain.toml;
          sha256 = "sha256-s1RPtyvDGJaX/BisLT+ifVfuhDT1nZkZ1NcK8sbwELM=";
        };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.espflash # flash + serial monitor over USB-C
            pkgs.gitleaks # secret scanning (see .githooks/pre-commit)
          ];

          # Route git at the tracked hooks so the gitleaks secret scan runs on
          # every commit made from inside the dev shell. `core.hooksPath` is a
          # local setting, so this (re)applies it on shell entry.
          shellHook = ''
            if [ -d .git ]; then
              git config --local core.hooksPath .githooks
            fi
          '';
        };
      });
}
