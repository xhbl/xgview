#!/usr/bin/env bash
#
# Checks the language packs.
#
# English is the reference: it is embedded in the binary with `include_str!` and
# is what every other language falls through to. A key that is asked for but not
# defined is drawn as its own name, and a key defined in English but not in
# another pack is drawn in English inside that language - both look like bugs to
# whoever sees them, and neither fails a build.
#
# Three things are checked:
#
#   1. every key the code asks for is defined in English;
#   2. every English key is defined in each pack beside it;
#   3. no pack defines the same key twice.
#
# Usage: scripts/check-i18n.sh   (exits non-zero when something is missing)
set -u
cd "$(dirname "$0")/.."

EN=crates/monitor_i18n/langs/en.ftl
STATUS=0

keys_of() {
    grep -oE '^[a-z0-9-]+ *=' "$1" | sed -E 's/ *=$//' | sort -u
}

report() {
    echo "$1"
    sed 's/^/  /' "$2"
    STATUS=1
}

USED=$(mktemp)
EN_KEYS=$(mktemp)
MISSING=$(mktemp)
trap 'rm -f "$USED" "$EN_KEYS" "$MISSING"' EXIT

# 1. What the code asks for, against English. `this-key-does-not-exist` is a
#    deliberate fixture of monitor_i18n's own tests, not a key anyone draws.
grep -rhoE 'tr(_args)?\("[a-z0-9-]+"' crates --include=*.rs \
    | sed -E 's/.*"([a-z0-9-]+)".*/\1/' \
    | sort -u | grep -v '^this-key-does-not-exist$' > "$USED"
keys_of "$EN" > "$EN_KEYS"
comm -23 "$USED" "$EN_KEYS" > "$MISSING"
[ -s "$MISSING" ] && report "asked for by the code, missing from $EN:" "$MISSING"

# 2. Every English key, against each pack beside it.
for PACK in langs/*.ftl; do
    PACK_KEYS=$(mktemp)
    keys_of "$PACK" > "$PACK_KEYS"
    comm -23 "$EN_KEYS" "$PACK_KEYS" > "$MISSING"
    [ -s "$MISSING" ] && report "in $EN, missing from $PACK:" "$MISSING"
    rm -f "$PACK_KEYS"
done

# 3. A key written twice: the last one wins, silently.
for PACK in "$EN" langs/*.ftl; do
    DUPLICATES=$(keys_of "$PACK" | uniq -d)
    [ -n "$DUPLICATES" ] && report "defined twice in $PACK:" <(echo "$DUPLICATES")
done

[ "$STATUS" -eq 0 ] && echo "i18n: ok"
exit "$STATUS"
