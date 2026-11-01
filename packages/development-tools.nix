# Keep both developer entrypoints on the same nixpkgs toolchain.
{ pkgs }:
with pkgs;
[
  bashInteractive
  cargo
  clippy
  devenv
  git
  just
  nix
  nixfmt
  ruff
  rustc
  rustfmt
]
