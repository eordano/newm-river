#!/usr/bin/env bash
# newm-river's live VM tests (nix/vm), one VM at a time: builds
# legacyPackages.x86_64-linux.vm-report.<test> (never fails, so the screenshots survive a
# failure), prints every case's status, and exits 1 on a failure xfail.nix does not expect.
# Run from the repository on a KVM host:
#   newm/e2e/vm.sh                       # vm-input vm-lock vm-session vm-reattach vm-browser
#   newm/e2e/vm.sh vm-lock               # one of them
# OUT (default /tmp/newm-river-vm) gets <test>/results.txt and <test>/shots/*.png.
# The strict variants are `nix build .#checks.x86_64-linux.vm-<test>` (fail on any failure).
set -uo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
out=${OUT:-/tmp/newm-river-vm}
tests=("$@")
[ ${#tests[@]} -eq 0 ] && tests=(vm-input vm-lock vm-session vm-reattach vm-browser)
mkdir -p "$out"
rc=0
for t in "${tests[@]}"; do
    echo "== $t"
    rm -rf "${out:?}/$t"
    if ! res=$(nix build -L --no-link --print-out-paths \
        "$root#legacyPackages.x86_64-linux.vm-report.$t"); then
        echo "fail  $t: the VM test did not finish" | tee -a "$out/errors.txt"
        rc=1
        continue
    fi
    mkdir -p "$out/$t"
    cp -r "$res"/. "$out/$t"/
    chmod -R u+w "$out/$t"
    cat "$res/results.txt"
    grep -q '^fail' "$res/results.txt" && rc=1
done
echo "== summary"
cat "$out"/*/results.txt 2>/dev/null | awk '{print $1}' | sort | uniq -c
grep -h '^xpass' "$out"/*/results.txt 2>/dev/null | sed 's|^|passes now, drop it from nix/vm/xfail.nix: |'
exit $rc
