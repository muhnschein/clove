#!/bin/sh
# Fail if the SonarQube scan silently skipped a file it could not parse.
#
# The scanner does not fail on a file it cannot read: it logs one WARN and
# analyses nothing. A file that is never read reports zero issues, which the
# dashboard renders identically to a file that is clean — so coverage can
# erode to nothing without a single red build. This reads the scan log and
# says so out loud.
#
# Same argument as check-dicts.sh, one tool over: a checker that has quietly
# stopped checking prints ok forever.
#
# Usage:
#   ci/check-scan-coverage.sh <scan.log>
#   ci/check-scan-coverage.sh --self-test
set -eu

# Files whose parse failure is a known bug in the analyser, not in the script.
# All three parse clean under `sh -n` and `bash -n`; what the analyser chokes
# on is a function definition inside an `if` block (fuzz-coverage.sh,
# fuzz-sanitizer-ab.sh — the latter gives up at 1:1, so the whole file goes
# unread) and `${VAR:+ ...}` alternate-value expansion (fuzz.sh).
#
# Rewriting working, tested CI scripts to suit somebody else's parser is the
# wrong trade, so they are baselined here instead. The gate then fails on a
# file that is NOT on this list — a new gap — and equally on one that IS but
# has stopped failing, because an allowlist nobody prunes is the same rot in
# a different place. Either way the fix is one line in this file.
KNOWN='ci/fuzz-coverage.sh
ci/fuzz-sanitizer-ab.sh
ci/fuzz.sh'

# The two shell analysers word it differently and disagree about where the
# file broke:
#   WARN  Cannot parse 'ci/fuzz.sh:234:32'
#   WARN  Syntax error in /abs/path/to/ci/fuzz.sh at 234:31
# Reduce either to a bare repo-relative path, so the result does not depend
# on which sensor spoke or on the runner's checkout directory.
extract() {
    scan_log="$1"
    root="${GITHUB_WORKSPACE:-$PWD}"
    grep -hoE "Cannot parse '[^']+'|Syntax error in [^ ]+ at " "$scan_log" 2>/dev/null \
        | sed -e "s|^Cannot parse '||" -e "s|'\$||" \
              -e 's|^Syntax error in ||' -e 's| at $||' \
              -e "s|^$root/||" \
              -e 's|:[0-9][0-9]*:[0-9][0-9]*$||' \
        | sort -u
    return $?
}

# The matcher restates somebody else's log format, which is exactly the kind
# of thing that drifts: reword the WARN upstream and this gate goes green
# forever. So it carries a self-test over both phrasings, an absolute path,
# a duplicate, and lines that must not match.
if [ "${1:-}" = "--self-test" ]; then
    tmp=$(mktemp -d) || exit 1
    trap 'rm -rf "$tmp"' EXIT
    GITHUB_WORKSPACE=/home/runner/work/clove/clove
    export GITHUB_WORKSPACE

    cat > "$tmp/fixture.log" <<'FIXTURE'
06:22:24.693 WARN  Cannot parse 'ci/fuzz-coverage.sh:226:19'
06:22:25.010 WARN  Cannot parse 'ci/fuzz.sh:234:32'
06:22:36.356 WARN  Syntax error in /home/runner/work/clove/clove/ci/fuzz.sh at 234:31
06:22:36.351 WARN  Syntax error in /home/runner/work/clove/clove/ci/aardvark.sh at 93:17
06:22:19.241 INFO  105 files indexed (done) | time=42ms
06:22:31.060 WARN  Duplication reported for 'crates/clove-core/tests/model.rs' will be ignored because it's a test file.
06:22:43.566 INFO  ANALYSIS SUCCESSFUL, you can find the results at: https://sonarcloud.io/dashboard
06:22:21.051 WARN  Your code is analyzed as compatible with all Python 3 versions by default.
FIXTURE

    # aardvark sorts first, and fuzz.sh appears under both phrasings: the
    # expected output proves the absolute path is stripped and the duplicate
    # collapsed, and that no INFO or unrelated WARN leaks through.
    cat > "$tmp/want" <<'WANT'
ci/aardvark.sh
ci/fuzz-coverage.sh
ci/fuzz.sh
WANT

    extract "$tmp/fixture.log" > "$tmp/got"
    if ! cmp -s "$tmp/got" "$tmp/want"; then
        echo "check-scan-coverage: --self-test FAILED" >&2
        echo "  the scan-log matcher has stopped matching. Expected:" >&2
        sed 's/^/    /' "$tmp/want" >&2
        echo "  got:" >&2
        sed 's/^/    /' "$tmp/got" >&2
        exit 1
    fi

    : > "$tmp/empty.log"
    if [ -n "$(extract "$tmp/empty.log")" ]; then
        echo "check-scan-coverage: --self-test FAILED: matched an empty log" >&2
        exit 1
    fi

    echo "check-scan-coverage: self-test ok"
    exit 0
fi

log="${1:-}"
if [ -z "$log" ]; then
    echo "usage: $0 <scan.log> | --self-test" >&2
    exit 1
fi
[ -f "$log" ] || { echo "check-scan-coverage: $log not found" >&2; exit 1; }

tmp=$(mktemp -d) || exit 1
trap 'rm -rf "$tmp"' EXIT

extract "$log" > "$tmp/got"
printf '%s\n' "$KNOWN" | sort -u > "$tmp/known"
comm -23 "$tmp/got" "$tmp/known" > "$tmp/new"
comm -13 "$tmp/got" "$tmp/known" > "$tmp/fixed"

status=0

if [ -s "$tmp/new" ]; then
    echo "check-scan-coverage: the scanner could not parse:" >&2
    sed 's/^/  /' "$tmp/new" >&2
    echo >&2
    echo "It analysed nothing in those files. That is a silent gap in" >&2
    echo "coverage, not a clean result. Either restate the code in a form" >&2
    echo "the analyser accepts, or — if the fault is upstream, which"    >&2
    echo "\`sh -n\` and \`bash -n\` will tell you — baseline it in KNOWN"  >&2
    echo "in this file, with the reason." >&2
    status=1
fi

if [ -s "$tmp/fixed" ]; then
    [ "$status" -eq 0 ] || echo >&2
    echo "check-scan-coverage: these are baselined in KNOWN but parsed" >&2
    echo "cleanly this run:" >&2
    sed 's/^/  /' "$tmp/fixed" >&2
    echo >&2
    echo "The analyser bug they document is fixed. Drop them from KNOWN" >&2
    echo "in this file — a baseline nobody prunes stops being a baseline" >&2
    echo "and starts being a blind spot." >&2
    status=1
fi

if [ "$status" -eq 0 ]; then
    echo "check-scan-coverage: ok ($(wc -l < "$tmp/known" | tr -d ' ') known-unparseable, no new gaps)"
fi

exit "$status"
