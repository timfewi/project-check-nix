{ pkgs, ... }:
{
  packages = import ./packages/development-tools.nix { inherit pkgs; };

  # Use only caches already configured by the operator.
  cachix.enable = false;

  tasks."project-check:environment-smoke" = {
    before = [ "devenv:enterTest" ];
    exec = ''
      set -euo pipefail
      cargo --version
      rustc --version
      cargo clippy --version
      cargo fmt --version
      ruff --version
      git --version
      nix --version
      nixfmt --version
      just --version
      just --fmt --check
      printf '%s\n' 'Developer environment smoke passed.'
    '';
  };
}
