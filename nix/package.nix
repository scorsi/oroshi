{ lib, craneLib }:
let
  common = {
    pname = "oroshi";
    version = (lib.importTOML ../Cargo.toml).workspace.package.version;
    strictDeps = true;
    cargoExtraArgs = "--locked --workspace";
    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        (craneLib.fileset.commonCargoSources ../.)
        ./example.toml
        ../apps/oroshi/tests/fixtures
        ../crates/oroshi-nix/tests/fixtures
      ];
    };
  };
  cargoArtifacts = craneLib.buildDepsOnly common;
in
craneLib.buildPackage (
  common
  // {
    inherit cargoArtifacts;
    doCheck = false; # Tests run in checks.cargo-test.
    meta = {
      mainProgram = "oroshi";
      license = lib.licenses.asl20;
    };
    passthru = { inherit common cargoArtifacts; };
  }
)
