# The Rust binaries, one package per crate from the one workspace.
{
  lib,
  rustPlatform,
  makeBinaryWrapper,
  crate,
  bin,
  description,
  # Programs the binary runs, put on its PATH.
  runtimeInputs ? [ ],
  # The built web console, compiled into via-server.
  web ? null,
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

  env = lib.optionalAttrs (web != null) { VIA_WEB_DIST = web; };

  nativeBuildInputs = lib.optional (runtimeInputs != [ ]) makeBinaryWrapper;
  postInstall = lib.optionalString (runtimeInputs != [ ]) ''
    wrapProgram $out/bin/${bin} --suffix PATH : ${lib.makeBinPath runtimeInputs}
  '';

  meta = {
    inherit description;
    mainProgram = bin;
    license = lib.licenses.asl20;
  };
}
