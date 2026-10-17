#!/usr/bin/env bash
# Journalled CLI verbs share one unmount and one failure path (#545).
# The kernel and installed-CLI suites check the volumes they leave; this
# guard checks that a new verb cannot grow a second unmount error path.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! awk '
    /^fn / {
        name = $2
        sub(/\(.*/, "", name)
    }
    /^[[:space:]]*\/\// { next }
    /\.unmount\(/ {
        unmounts++
        if (name != "release_clean") {
            print "FAIL  " name " unmounts outside release_clean" > "/dev/stderr"
            fails++
        }
    }
    /release_clean\(fs, path\)/ { releases[name]++ }
    END {
        if (unmounts != 1) {
            print "FAIL  the CLI must have one unmount site, in release_clean" > "/dev/stderr"
            fails++
        }
        split("write mkdir remove", verbs, " ")
        for (i = 1; i <= 3; i++) {
            if (!releases[verbs[i]]) {
                print "FAIL  " verbs[i] " does not use release_clean" > "/dev/stderr"
                fails++
            }
        }
        exit (fails > 0)
    }
' "$REPO/src/cli/xfs/fs.rs"; then
    exit 1
fi

echo 'cli-shared-unmount: all checks passed'
