# Verify the portable rules against their synthetic fixtures.
#
# The pinned Semgrep engine's `--test` path crashes on Rust fixtures, so this
# scans the fixtures with the same scanner used in production and then asserts
# the `ruleid`/`ok` annotations against the resulting JSON.
{ pkgs, qualityRules }:
pkgs.runCommand "project-check-quality-rules-check"
  {
    nativeBuildInputs = [
      pkgs.semgrep
      pkgs.python3
    ];
  }
  ''
    export SEMGREP_SEND_METRICS=off
    export SEMGREP_SETTINGS_FILE="$TMPDIR/settings.yml"
    export SEMGREP_LOG_FILE="$TMPDIR/semgrep.log"
    export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
    mkdir -p foreign/project/lib
    cp ${../tests/portable-quality/foreign.py} foreign/project/lib/foreign.py
    semgrep scan --config ${qualityRules} --strict --json \
      --no-rewrite-rule-ids --metrics=off --disable-version-check --jobs 1 \
      foreign/project/lib > report.json
    python3 ${../scripts/check-semgrep-tests.py} report.json ${qualityRules} foreign/project/lib
    touch "$out"
  ''
