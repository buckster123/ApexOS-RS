#!/usr/bin/env bash
# Local drill for deploy/usb/usb-confine.sh — no root, no real stick.
# Exercises label/dev sanitising, symlink refusal, root-owner check,
# canonical-path match, and mountinfo exact-TARGET lookup.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=usb-confine.sh
. "$HERE/usb-confine.sh"

PASS=0
FAIL=0
say() { printf '%s\n' "$*"; }
check() { # $1=desc $2=actual $3=expected
  if [ "$2" = "$3" ]; then PASS=$((PASS+1)); say "  ✓ $1"
  else FAIL=$((FAIL+1)); say "  ✗ $1: got [$2] want [$3]"; fi
}
ok() { if "$@"; then echo yes; else echo no; fi; }

ROOT=/tmp/apexos-usb-confine-drill.$$
trap 'rm -rf "$ROOT"' EXIT
mkdir -p "$ROOT/workspace/media" "$ROOT/elsewhere"

say "=== labels + kernel names ==="
check "good label"    "$(safe_exo_label APEX-config || echo FAIL)" "APEX-config"
check "good dotted"   "$(safe_exo_label APEX-work_2024.1 || echo FAIL)" "APEX-work_2024.1"
check "no prefix"     "$(ok safe_exo_label config)" no
check "empty name"    "$(ok safe_exo_label APEX-)" no
check "slash"         "$(ok safe_exo_label APEX-a/b)" no
check "dotdot"        "$(ok safe_exo_label APEX-../etc)" no
check "space"         "$(ok safe_exo_label 'APEX-a b')" no
check "bang stripped" "$(ok safe_exo_label 'APEX-foo!')" no
check "dev sdb1"      "$(ok safe_kernel_dev sdb1)" yes
check "dev nvme"      "$(ok safe_kernel_dev nvme0n1p1)" yes
check "dev slash"     "$(ok safe_kernel_dev '../sda')" no
check "dev empty"     "$(ok safe_kernel_dev '')" no

say "=== symlink refusal ==="
ln -s /etc "$ROOT/workspace/media/APEX-evil"
check "mountpoint symlink" "$(ok assert_not_symlink "$ROOT/workspace/media/APEX-evil" mp)" no
check "real dir ok"        "$(ok assert_not_symlink "$ROOT/workspace/media" media)" yes
ln -s /etc "$ROOT/media-link"
check "media root symlink" "$(ok assert_not_symlink "$ROOT/media-link" media)" no

say "=== under + canonical ==="
check "under media" "$(ok assert_under "$ROOT/workspace/media/APEX-x" "$ROOT/workspace/media")" yes
check "not under"   "$(ok assert_under /etc/passwd "$ROOT/workspace/media")" no
# realpath of a real dir equals itself
exp="$(realpath -e "$ROOT/workspace/media")"
mkdir -p "$ROOT/workspace/media/APEX-ok"
check "canonical match" "$(ok assert_canonical_mnt "$ROOT/workspace/media/APEX-ok" "$exp/APEX-ok")" yes
# hop: APEX-hop → elsewhere
ln -s "$ROOT/elsewhere" "$ROOT/workspace/media/APEX-hop"
check "canonical hop" "$(ok assert_canonical_mnt "$ROOT/workspace/media/APEX-hop" "$exp/APEX-hop")" no

say "=== root-owned dir ==="
# /usr is a real root-owned directory on any normal box; the drill dir is not.
check "usr is root dir" "$(ok assert_root_owned_dir /usr usr)" yes
check "drill dir not root" "$(ok assert_root_owned_dir "$ROOT/workspace/media" media)" no
check "symlink not root dir" "$(ok assert_root_owned_dir "$ROOT/media-link" media)" no

say "=== mountinfo exact TARGET ==="
# `/` is always a mount. A path that is not a mountpoint must miss.
root_src="$(mountinfo_source_for / || true)"
check "slash has source" "$([ -n "$root_src" ] && echo yes || echo no)" yes
check "drill path absent" "$(ok mountinfo_source_for "$ROOT/workspace/media/APEX-ok")" no

say "=== umount refuses a symlink even if the target is a mount ==="
# APEX-evil → /etc. / is mounted; following the link must not count as "our" mount.
APEXOS_USB_WORKSPACE="$ROOT/workspace"
export APEXOS_USB_WORKSPACE
check "umount symlink" "$(ok assert_umount_ok "$ROOT/workspace/media/APEX-evil")" no
check "umount missing" "$(assert_umount_ok "$ROOT/workspace/media/APEX-ok"; echo $?)" "2"

say "=== helpers refuse a planted mountpoint (end-to-end script) ==="
# usb-umount --label against a symlink under a user-owned media root: fails
# closed on either the symlink or the non-root media root. Never reaches umount.
set +e
"$HERE/usb-umount" --label APEX-evil >"$ROOT/umount.log" 2>&1
u_rc=$?
set -e
check "usb-umount symlink exits nonzero" "$([ "$u_rc" -ne 0 ] && echo yes || echo no)" yes
if grep -q 'unmounted' "$ROOT/umount.log"; then
  FAIL=$((FAIL+1)); say "  ✗ usb-umount must not umount through a symlink"
else
  PASS=$((PASS+1)); say "  ✓ usb-umount did not umount"
fi

# Replacing media/ itself with a symlink to / must not umount the rootfs.
rm -rf "$ROOT/workspace/media"
ln -s / "$ROOT/workspace/media"
set +e
"$HERE/usb-umount" --label APEX-config >"$ROOT/umount-media.log" 2>&1
mroot_rc=$?
set -e
check "usb-umount media→/ exits nonzero" "$([ "$mroot_rc" -ne 0 ] && echo yes || echo no)" yes
if grep -q 'unmounted' "$ROOT/umount-media.log"; then
  FAIL=$((FAIL+1)); say "  ✗ usb-umount must not umount through a media symlink"
else
  PASS=$((PASS+1)); say "  ✓ usb-umount did not umount via media symlink"
fi

# A legacy/hostile state path `media/../../etc` must be reconstructed, not used.
mkdir -p "$ROOT/run"
printf '%s\n' "$ROOT/workspace/media/../../etc" > "$ROOT/run/sdb1.mnt"
set +e
APEXOS_USB_RUNDIR="$ROOT/run" "$HERE/usb-umount" sdb1 >"$ROOT/state.log" 2>&1
st_rc=$?
set -e
check "hostile state exits nonzero" "$([ "$st_rc" -ne 0 ] && echo yes || echo no)" yes
if grep -q 'unmounted' "$ROOT/state.log"; then
  FAIL=$((FAIL+1)); say "  ✗ usb-umount must not honour a traversal state path"
else
  PASS=$((PASS+1)); say "  ✓ usb-umount ignored traversal state path"
fi

# usb-mount with a garbage device name must die before touching the block layer.
set +e
"$HERE/usb-mount" '../sda' >"$ROOT/mount.log" 2>&1
m_rc=$?
set -e
check "usb-mount ../sda exits nonzero" "$([ "$m_rc" -ne 0 ] && echo yes || echo no)" yes

say "=== bash -n ==="
for f in usb-confine.sh usb-mount usb-umount usb-prep usb-prep-drain usb-eject-drain apexos-workspace-init; do
  if bash -n "$HERE/$f"; then PASS=$((PASS+1)); say "  ✓ syntax $f"
  else FAIL=$((FAIL+1)); say "  ✗ syntax $f"; fi
done

say ""
say "drill: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
