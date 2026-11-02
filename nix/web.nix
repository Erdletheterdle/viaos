# The web console's static files, compiled into via-server.
#
# Dependencies are a fixed-output derivation, so they may use the network; it
# installs every platform's native binaries so one hash serves all systems.
# After changing web/bun.lock, set outputHash to lib.fakeHash and build to
# learn the new one.
{
  lib,
  stdenv,
  stdenvNoCC,
  autoPatchelfHook,
  bun,
  cacert,
}:
let
  src = lib.fileset.toSource {
    root = ../web;
    fileset = lib.fileset.difference ../web (
      lib.fileset.unions [
        (lib.fileset.maybeMissing ../web/node_modules)
        (lib.fileset.maybeMissing ../web/dist)
      ]
    );
  };
  deps = stdenvNoCC.mkDerivation {
    pname = "via-console-deps";
    version = "0";
    inherit src;
    nativeBuildInputs = [
      bun
      cacert
    ];
    dontConfigure = true;
    buildPhase = ''
      export HOME=$TMPDIR
      bun install --frozen-lockfile --ignore-scripts --no-progress --os='*' --cpu='*'
    '';
    installPhase = ''
      mkdir $out
      cp -R node_modules $out/
    '';
    dontFixup = true;
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = "sha256-VeVZlJ3/GAPpK8H2CyAkRWmE9WcgddHAlc/rEMZEjRY=";
  };
in
stdenv.mkDerivation {
  pname = "via-console";
  version = "0";
  inherit src;
  nativeBuildInputs = [
    bun
    autoPatchelfHook
  ];
  buildInputs = [ stdenv.cc.cc.lib ];
  # Other platforms' binaries and the C libraries they want are not ours.
  autoPatchelfIgnoreMissingDeps = true;
  dontConfigure = true;
  dontAutoPatchelf = true;
  buildPhase = ''
    export HOME=$TMPDIR
    cp -R ${deps}/node_modules node_modules
    chmod -R u+w node_modules
    autoPatchelf node_modules
    bun --bun node_modules/typescript/bin/tsc --noEmit
    bun --bun node_modules/vite/bin/vite.js build
  '';
  installPhase = ''
    cp -R dist $out
  '';
  dontFixup = true;
}
