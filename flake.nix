{
  description = "Async no_std ESP32-C3 smart-home sensor node firmware (Embassy); started as a bird-feeder scale";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # espflash 4.x refuses to flash an image without an ESP-IDF app descriptor,
    # which esp-hal 0.22 does not emit (`esp_app_desc!` arrived in 0.23).
    # Forcing it through with `--ignore-app-descriptor` writes garbage into the
    # image header's min-efuse-revision fields, and the second-stage bootloader
    # then rejects the app on every boot:
    #   Image requires efuse blk rev >= v145.58, but chip is v1.3
    #   No bootable app partitions in the partition table
    # So pin the flasher to the 3.x line until esp-hal is bumped.
    nixpkgs-espflash.url = "github:NixOS/nixpkgs/nixos-24.11";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, nixpkgs-espflash, fenix, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        pkgsEspflash = import nixpkgs-espflash { inherit system; };

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
            pkgsEspflash.espflash # flash + serial monitor over USB-C (3.x, see above)
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
