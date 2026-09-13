# The service as a Nix package.
#
# Kept out of the flake file so the NixOS module can `callPackage` it directly:
# a home server that imports the module should not also have to wire up the
# flake's `packages` output to get the binary.
{ lib, rustPlatform, stdenv }:

rustPlatform.buildRustPackage {
  pname = "smarthome-timeseries";
  version = "0.1.0";

  # Two directories stay out, and both for the same reason: a change in them is
  # not a change to the program, and anything inside `src` that moves forces a
  # full rebuild of it.
  #
  # `target` is gigabytes of incremental artefacts, which would also be copied
  # into the store on the way past.
  #
  # `nix` is this file and the module beside it. The packaging cannot be an
  # input to the package it describes -- Nix reads these through the flake, not
  # out of the derivation -- but while it sat in `src`, editing the module meant
  # recompiling the whole crate. Which is how it was found: capping QuestDB's
  # heap in module.nix cost a thirteen-minute Rust build on the home server.
  #
  # Anchored to the top level rather than matched by name at any depth, so a
  # directory that happens to be called either of these further down still
  # counts as source.
  src =
    let
      root = ./..;
      relative = path: lib.removePrefix (toString root + "/") (toString path);
    in
    lib.cleanSourceWith {
      src = root;
      filter =
        path: type:
        let
          rel = relative path;
        in
        !(type == "directory" && (rel == "target" || rel == "nix"));
    };
  cargoLock.lockFile = ../Cargo.lock;

  # `.cargo/config.toml` in this directory hard-codes x86_64, because it has to
  # override the firmware's riscv32 default and cargo has no way to say "just
  # the host". The environment variable wins over the file, so this is what
  # makes the package build on a Raspberry Pi home server as well.
  CARGO_BUILD_TARGET = stdenv.hostPlatform.rust.rustcTargetSpec;

  meta = {
    description = "MQTT to QuestDB archiver and dashboard for the smart-home sensor fleet";
    mainProgram = "smarthome-timeseries";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
