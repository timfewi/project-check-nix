{
  description = "Portable, argv-only project verification runner (project-check)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/7a0f122f5090cf4c2ade2a13a0e229d4e19ba71f";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      # The pinned nixpkgs scanner has no Intel macOS engine. Native core and
      # renderer outputs remain available there; the portable installer supplies
      # its explicitly pinned Intel-compatible scanner separately.
      scannerSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          qualityRules = pkgs.callPackage ./packages/quality-rules.nix { };
        in
        {
          project-check-core = pkgs.callPackage ./packages/project-check-core.nix { };
          project-docs = pkgs.callPackage ./packages/project-docs.nix { };
          quality-rules = qualityRules;
        }
        // nixpkgs.lib.optionalAttrs (builtins.elem system scannerSystems) (
          let
            projectCheck = pkgs.callPackage ./packages/project-check.nix { };
          in
          {
            default = projectCheck;
            project-check = projectCheck;
          }
        )
      );

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          qualityRules = pkgs.callPackage ./packages/quality-rules.nix { };
        in
        {
          # Native contracts and linting do not depend on the Semgrep package.
          rust-tests = pkgs.callPackage ./packages/project-check-core.nix { };
        }
        // nixpkgs.lib.optionalAttrs (builtins.elem system scannerSystems) {
          # Round-trips the immutable portable rules against their positive and
          # negative fixtures through the real Semgrep scanner. Opt-in: it builds
          # Semgrep.
          quality-rules = pkgs.callPackage ./packages/quality-rules-check.nix {
            inherit qualityRules;
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = import ./packages/development-tools.nix { inherit pkgs; };
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
