{
  description = "oroshi (卸) — package registry";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    # Test-only inputs for the service nixosTest in O0 task 8.
    nisshi = {
      url = "github:scorsi/nisshi";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
      inputs.treefmt-nix.follows = "treefmt-nix";
    };
    masu = {
      url = "github:scorsi/masu";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
      inputs.treefmt-nix.follows = "treefmt-nix";
      inputs.crane.follows = "crane";
    };
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
      ];

      imports = [ inputs.treefmt-nix.flakeModule ];

      perSystem =
        { pkgs, lib, ... }:
        let
          craneLib = inputs.crane.mkLib pkgs;
          oroshi = pkgs.callPackage ./nix/package.nix { inherit craneLib; };
          common = oroshi.passthru.common;
          cargoArtifacts = oroshi.passthru.cargoArtifacts;
          workspace = (lib.importTOML ./Cargo.toml).workspace;
        in
        {
          packages = {
            inherit oroshi;
            default = oroshi;
          };

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rust-analyzer
              clippy
              rustfmt
              sqlite
              nix
            ];
          };

          checks = {
            cargo-test = craneLib.cargoTest (common // { inherit cargoArtifacts; });
            cargo-clippy = craneLib.cargoClippy (
              common
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
              }
            );
            cargo-fmt = craneLib.cargoFmt (common // { cargoExtraArgs = "--all"; });
            versions =
              assert lib.all (
                member: (lib.importTOML (./. + "/${member}/Cargo.toml")).package.version.workspace or false
              ) workspace.members;
              pkgs.runCommand "oroshi-versions" { } ''
                for binary in ${oroshi}/bin/*; do
                  test "$("$binary" --version)" = "$(basename "$binary") ${workspace.package.version}"
                done
                touch "$out"
              '';
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              rustfmt.enable = true;
              nixfmt.enable = true;
            };
            settings.excludes = [
              "target/**"
              "result*"
              "Cargo.lock"
            ];
          };
        };
    };
}
