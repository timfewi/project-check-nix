{
  description = "Portable, argv-only project verification runner (project-check)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/eaad089433ca2bb662274377d33df3d0e51ef28b";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      projectCheck = pkgs.callPackage ./packages/project-check.nix { };
      qualityRules = pkgs.callPackage ./packages/quality-rules.nix { };
    in
    {
      packages.${system} = {
        default = projectCheck;
        project-check = projectCheck;
        quality-rules = qualityRules;
      };

      checks.${system} = {
        # Unit tests exercise the runner contract (argv handling, timeouts,
        # missing tools, baseline report parsing, watch batching) without a
        # scanner, so the fast gate stays cheap.
        python-tests =
          pkgs.runCommand "project-check-python-tests"
            {
              nativeBuildInputs = [ pkgs.python3 ];
            }
            ''
              cd ${self}
              python3 -m unittest discover -s tests -t . -p 'test_*.py' -v
              touch "$out"
            '';
        # Round-trips the immutable portable rules against their positive and
        # negative fixtures through the real Semgrep scanner. Opt-in: it builds
        # Semgrep.
        quality-rules = pkgs.callPackage ./packages/quality-rules-check.nix {
          inherit qualityRules;
        };
      };

      formatter.${system} = pkgs.nixfmt;
    };
}
