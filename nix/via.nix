# The Rust binaries, one package per crate from the one workspace.
{
  lib,
  rustPlatform,
  crate,
  bin,
  description,
}:

rustPlatform.buildRustPackage {
  pname = crate;
  version = "0.0.1";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      # via-agent's tests read the example config.
      ../nix/viaos.example.toml
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [
    "-p"
    crate
  ];
  # `cargo test --workspace` runs in CI, against Docker; a package build
  # only needs to compile.
  doCheck = false;

  meta = {
    inherit description;
    mainProgram = bin;
    license = lib.licenses.asl20;
  };
}
