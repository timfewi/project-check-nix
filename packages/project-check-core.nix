# Both native binaries and contract tests, without a Semgrep build dependency.
{ pkgs }:
pkgs.rustPlatform.buildRustPackage {
  pname = "project-check-core";
  version = "0.1.0";
  src = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../tests
      ../scripts/check-semgrep-tests.py
      ../.semgrep/portable
      ../pyproject.toml
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  strictDeps = true;
  nativeBuildInputs = [
    pkgs.rustfmt
    pkgs.clippy
    pkgs.ruff
  ];
  nativeCheckInputs = [
    pkgs.git
    pkgs.bash
    pkgs.coreutils
  ];
  env.RUSTFLAGS = "-D warnings";
  preBuild = ''
    cargo fmt --all -- --check
    ruff check --no-cache scripts/check-semgrep-tests.py
    ruff format --check --no-cache scripts/check-semgrep-tests.py
  '';
  preCheck = ''
    cargo clippy --offline --locked --release --all-targets \
      --target ${pkgs.stdenv.hostPlatform.rust.rustcTargetSpec} -- -D warnings
  '';
  meta = {
    description = "Native, bounded, argv-only project verification and documentation renderer";
    license = pkgs.lib.licenses.mit;
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    mainProgram = "project-check";
  };
}
