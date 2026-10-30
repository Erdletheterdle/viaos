{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "via-agent";
  version = "0.0.1";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "-p" "via-agent" ];
  # The workspace tests run in CI against Docker; the agent build only needs
  # to compile.
  doCheck = false;

  meta = {
    description = "viaos device agent";
    mainProgram = "via-agent";
    license = lib.licenses.asl20;
  };
}
