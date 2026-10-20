#!/usr/bin/env bash
# How long a code change takes to a finished build of a game made by `ofeliaengine new`,
# with its `editor` feature, per cargo profile (ADR 0107 §1, E1-5c; the plan in
# docs/research/rebuild-restart.md). Results go to docs/perf/history.md.
#
#   CARGO_TARGET_DIR=C:/cargo-target/<name> bash tools/measure-rebuild.sh <game dir> [profiles...]
#
# Per profile (default: release dev): the first build, a build with nothing changed, 10
# changes to a function's body and 5 to a comment, each timed to the end of `cargo build`.
# It edits the game's `src/main.rs` (the quick start's `Quat::from_rotation_y(dt)`, and a
# `// measure:` line it adds): run it on a game made for measuring, never on your own. Keep
# it out of a check run: both use every core.
set -u
GAME="$1"
shift
PROFILES="${*:-release dev}"
cd "$GAME" || exit 2
ms() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }
build() {
    local t
    t=$(date +%s%N)
    if ! cargo build -q --profile "$1" --features editor 2> build-errors.txt; then
        echo "FAILED"
        tail -5 build-errors.txt
        return
    fi
    ms "$t"
}
grep -q "^// measure:" src/main.rs || echo "// measure: start" >> src/main.rs
for p in $PROFILES; do
    echo "== $p"
    echo "first: $(build "$p") ms"
    echo "nothing changed: $(build "$p") ms"
    for i in 1 2 3 4 5 6 7 8 9 10; do
        if [ $((i % 2)) -eq 1 ]; then
            sed -i 's/Quat::from_rotation_y(dt)/Quat::from_rotation_y(dt * 1.5)/' src/main.rs
        else
            sed -i 's/Quat::from_rotation_y(dt \* 1.5)/Quat::from_rotation_y(dt)/' src/main.rs
        fi
        echo "body edit $i: $(build "$p") ms"
    done
    for i in 1 2 3 4 5; do
        sed -i "s|^// measure: .*|// measure: comment $i|" src/main.rs
        echo "comment edit $i: $(build "$p") ms"
    done
    dir="$p"
    [ "$p" = dev ] && dir=debug
    ls -la "${CARGO_TARGET_DIR:-target}/$dir/"*.exe "${CARGO_TARGET_DIR:-target}/$dir/"*.pdb 2>/dev/null | awk '{print $5, $NF}'
done
