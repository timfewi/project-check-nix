{ pkgs }:
let
  core = pkgs.callPackage ./project-check-core.nix { };
in
pkgs.runCommand "project-docs-0.1.0"
  {
    meta.mainProgram = "project-docs";
  }
  ''
    mkdir -p "$out/bin"
    ln -s ${core}/bin/project-docs "$out/bin/project-docs"
  ''
