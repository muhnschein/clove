#!/bin/sh
# Gate on the libFuzzer dictionaries in fuzz/dicts/.
#
# The nightly fuzz job passes `-dict=fuzz/dicts/<target>.dict` for every target
# in its matrix, and libFuzzer exits 1 — before fuzzing anything — if that file
# is missing or carries one line it cannot parse. Nothing on the per-push path
# reads these files, so a typo in one is invisible until the scheduled run.
# That is how `text.dict` shipped `"\r"`: the target was fine, the job was red,
# and a night of fuzzing bought nothing.
#
# The grammar is libFuzzer's own ParseDictionaryFile. Blank lines and lines
# whose first non-space character is `#` are skipped; every other line is
# `"token"` or `name="token"`, and inside the quotes the only escapes are `\\`,
# `\"` and `\xHH`. `\r`, `\n` and `\t` are not escapes here — spell them
# `\x0d`, `\x0a`, `\x09`.
#
#   ./ci/check-dicts.sh              # gate every dictionary
#   ./ci/check-dicts.sh --self-test  # check the checker still rejects
#
# The self-test exists for the same reason check-net-deps.sh has one: a parser
# that quietly stops matching prints ok forever, and this one is a restatement
# of somebody else's grammar, so it is exactly the kind that drifts.
set -eu

cd "$(dirname "$0")/.."

# ParseOneDictionaryEntry, line for line. Prints libFuzzer's own message for a
# line it would reject, so the failure here reads the same as the failure there.
PARSER='
function is_space(ch) {
    return ch == " " || ch == "\t" || ch == "\r" || ch == "\v" || ch == "\f"
}
function is_hex(ch) { return index("0123456789abcdefABCDEF", ch) > 0 }

function parse_entry(s,   n, L, R, pos, ch, esc) {
    n = length(s)
    if (n == 0) return 0
    L = 1; R = n
    while (L < R && is_space(substr(s, L, 1))) L++
    while (R > L && is_space(substr(s, R, 1))) R--
    # Three characters minimum: two quotes and something between them.
    if (R - L < 2) return 0
    if (substr(s, R, 1) != "\"") return 0
    R--
    # Skip whatever precedes the opening quote, which is how `name="tok"` parses.
    while (L < R && substr(s, L, 1) != "\"") L++
    if (L >= R) return 0
    L++
    for (pos = L; pos <= R; pos++) {
        ch = substr(s, pos, 1)
        if (ch != "\\") continue
        if (pos + 1 > R) return 0
        pos++
        esc = substr(s, pos, 1)
        if (esc == "\\" || esc == "\"") continue
        if (esc != "x") return 0
        if (pos + 2 > R) return 0
        if (!is_hex(substr(s, pos + 1, 1))) return 0
        if (!is_hex(substr(s, pos + 2, 1))) return 0
        pos += 2
    }
    return 1
}

{
    line = $0
    pos = 1
    while (pos <= length(line) && is_space(substr(line, pos, 1))) pos++
    if (pos > length(line)) next                      # blank
    if (substr(line, pos, 1) == "#") next             # comment
    if (parse_entry(line)) { entries++; next }
    printf "%s: ParseDictionaryFile: error in line %d\n\t\t%s\n", FILENAME, FNR, line
    bad++
}

END {
    if (bad) exit 1
    if (entries == 0) {
        printf "%s: no entries; libFuzzer treats an empty dictionary as an error\n", FILENAME
        exit 1
    }
}
'

check_file() { awk "$PARSER" "$1" >&2; }

if [ "${1:-}" = "--self-test" ]; then
    tmp=$(mktemp) || exit 1
    trap 'rm -f "$tmp"' EXIT
    status=0

    # Accepted: a bare token, the `name=` form, both string escapes, a hex
    # escape, and a line with space around it.
    while IFS= read -r case; do
        printf '%s\n' "$case" > "$tmp"
        if ! check_file "$tmp" 2>/dev/null; then
            echo "check-dicts: self-test: rejected a valid entry: $case" >&2
            status=1
        fi
    done <<'GOOD'
"abc"
name="abc"
"\x0d"
"\\"
"\""
"a\\b\"c\x41"
  "abc"	
GOOD

    # Rejected. The first two are the regression this check exists for.
    while IFS= read -r case; do
        printf '%s\n' "$case" > "$tmp"
        if check_file "$tmp" 2>/dev/null; then
            echo "check-dicts: self-test: accepted an invalid entry: $case" >&2
            status=1
        fi
    done <<'BAD'
"\r"
"\n"
"\t"
"\q"
"abc
abc"
abc
""
"\x4"
"\xzz"
"\"
BAD

    # A file of nothing but comments has no entries, which libFuzzer refuses.
    printf '# just a comment\n' > "$tmp"
    if check_file "$tmp" 2>/dev/null; then
        echo "check-dicts: self-test: accepted a dictionary with no entries" >&2
        status=1
    fi

    [ "$status" -eq 0 ] && echo "check-dicts: self-test ok"
    exit "$status"
fi

status=0

# Every target CI fuzzes needs a dictionary, because ci.yml passes `-dict=`
# unconditionally. ci/fuzz.sh is more forgiving — it passes the flag only when
# the file is there — so a target added with no dictionary passes locally and
# fails only in the scheduled job. The targets on disk are the list; keeping a
# second copy of it here is the mistake this file is trying to prevent.
for target in fuzz/fuzz_targets/*.rs; do
    [ -e "$target" ] || continue
    name=$(basename "$target" .rs)
    if [ ! -f "fuzz/dicts/$name.dict" ]; then
        echo "FAIL: fuzz target '$name' has no fuzz/dicts/$name.dict; CI passes -dict= for every target" >&2
        status=1
    fi
done

count=0
for dict in fuzz/dicts/*.dict; do
    [ -e "$dict" ] || continue
    count=$((count + 1))
    check_file "$dict" || status=1
done

if [ "$count" -eq 0 ]; then
    echo "FAIL: no dictionaries found in fuzz/dicts (run from the repository root)" >&2
    status=1
fi

[ "$status" -eq 0 ] && echo "check-dicts: ok ($count dictionaries)"
exit "$status"
