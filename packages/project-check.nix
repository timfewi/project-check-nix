# The packaged `project-check` runner.
#
# A single dependency-free Python file with its three build-time inputs baked in
# as absolute store paths: the immutable portable quality-rules package, the
# pinned Semgrep scanner and the CA bundle. The `-I` shebang isolates the script
# from the ambient Python environment.
{ pkgs }:
let
  qualityRules = pkgs.callPackage ./quality-rules.nix { };
in
pkgs.writeScriptBin "project-check" ''
  #!${pkgs.python3}/bin/python3 -I
  ${builtins.replaceStrings
    [ "@qualityRules@" "@semgrep@" "@cacert@" ]
    [
      (toString qualityRules)
      "${pkgs.semgrep}/bin/semgrep"
      (toString pkgs.cacert)
    ]
    (builtins.readFile ../runtime/project_check.py)
  }
''
