# The packaged `project-check` runner.
#
# A dependency-free Python runner with immutable rules, the pinned scanner,
# CA bundle and companion renderer baked in as store paths. Both executables
# use `-I` shebangs to isolate them from the ambient Python environment.
{ pkgs }:
let
  qualityRules = pkgs.callPackage ./quality-rules.nix { };
  projectDocs = pkgs.callPackage ./project-docs.nix { };
  runner = pkgs.writeScriptBin "project-check" ''
    #!${pkgs.python3}/bin/python3 -I
    ${builtins.replaceStrings
      [ "@qualityRules@" "@semgrep@" "@cacert@" "@projectDocs@" ]
      [
        (toString qualityRules)
        "${pkgs.semgrep}/bin/semgrep"
        (toString pkgs.cacert)
        "${projectDocs}/bin/project-docs"
      ]
      (builtins.readFile ../runtime/project_check.py)
    }
  '';
in
pkgs.symlinkJoin {
  name = "project-check";
  paths = [
    runner
    projectDocs
  ];
}
