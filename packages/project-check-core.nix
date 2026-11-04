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
  cargoBuildFlags = [
    "--bin"
    "project-check"
    "--bin"
    "project-docs"
  ];
  cargoTestFlags = [
    "--features"
    "test-support"
  ];
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
    cargo clippy --offline --locked --release --all-targets --features test-support \
      --target ${pkgs.stdenv.hostPlatform.rust.rustcTargetSpec} -- -D warnings
  '';
  # Contract fixtures are built for tests only and must never enter runtime
  # packages, even if a Cargo install hook discovers their test build output.
  postInstall = ''
    rm -f "$out/bin/project-check-contract-child"
  '';
  meta = {
    description = "Native, bounded, argv-only project verification and documentation renderer";
    license = pkgs.lib.licenses.mit;
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
      "x86_64-darwin"
    ];
    mainProgram = "project-check";
  };
}
