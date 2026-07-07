{
  description = "Async ESP32-C3 bird-feeder scale firmware (no_std, Embassy)";

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
          ];
        };
      });
}
