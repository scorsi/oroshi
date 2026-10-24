{
  lib,
  craneLib,
  sqlite,
  pkg-config,
}:
let
  common = {
    pname = "oroshi";
    version = (lib.importTOML ../Cargo.toml).workspace.package.version;
    strictDeps = true;
    buildInputs = [ sqlite ];
    nativeBuildInputs = [ pkg-config ];
    cargoExtraArgs = "--locked --workspace";
    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        (craneLib.fileset.commonCargoSources ../.)
        ./example.toml
        ../apps/oroshi/tests/fixtures
        ../crates/oroshi-nix/tests/fixtures
        ../crates/oroshi-store/src/schema.sql
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
