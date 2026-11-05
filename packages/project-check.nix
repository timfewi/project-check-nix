# Copy the binaries so current_exe resolves this immutable package prefix, not
# the scanner-independent core. No environment override can replace the tools.
{ pkgs }:
let
  core = pkgs.callPackage ./project-check-core.nix { };
  qualityRules = pkgs.callPackage ./quality-rules.nix { };
  semgrep = import ./semgrep.nix { inherit pkgs; };
  tooling = pkgs.writeText "project-check-tools.json" (
    builtins.toJSON {
      version = 1;
      rules = toString qualityRules;
      scanner = "${semgrep}/bin/semgrep";
      docs = "${core}/bin/project-docs";
      ca = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    }
  );
in
pkgs.runCommand "project-check-0.1.0"
  {
    meta.mainProgram = "project-check";
  }
  ''
    mkdir -p "$out/bin" "$out/share/project-check"
    cp ${core}/bin/project-check ${core}/bin/project-docs "$out/bin/"
    ln -s ${tooling} "$out/share/project-check/tools.json"
  ''
