{
  description = "Portable, argv-only project verification runner (project-check)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/7a0f122f5090cf4c2ade2a13a0e229d4e19ba71f";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          projectCheck = pkgs.callPackage ./packages/project-check.nix { };
          qualityRules = pkgs.callPackage ./packages/quality-rules.nix { };
        in
        {
          default = projectCheck;
          project-check = projectCheck;
          quality-rules = qualityRules;
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          qualityRules = pkgs.callPackage ./packages/quality-rules.nix { };
        in
        {
          # Unit tests exercise the runner contract (argv handling, timeouts,
          # missing tools, baseline report parsing, watch batching) without a
          # scanner, so the fast gate stays cheap.
          python-tests =
            pkgs.runCommand "project-check-python-tests"
              {
                nativeBuildInputs = [
                  pkgs.git
                  pkgs.python3
                  pkgs.ruff
                ];
              }
              ''
                cd ${self}
                ruff check --no-cache runtime tests/test_*.py scripts/check-semgrep-tests.py
                ruff format --check --no-cache runtime tests/test_*.py scripts/check-semgrep-tests.py
                python3 -m unittest discover -s tests -t . -p 'test_*.py' -v
                touch "$out"
              '';
          # Round-trips the immutable portable rules against their positive and
          # negative fixtures through the real Semgrep scanner. Opt-in: it builds
          # Semgrep.
          quality-rules = pkgs.callPackage ./packages/quality-rules-check.nix {
            inherit qualityRules;
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
