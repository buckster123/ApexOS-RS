#!/usr/bin/env bash
# Shared confinement for the root USB helpers (usb-mount / usb-umount / usb-prep).
#
# Finding 7: after a policy escape, agentd could replace workspace/media or
# media/APEX-* with a symlink; mkdir -p / mount / umount / mountpoint then
# followed it and operated on an arbitrary host path.
#
# Laws:
#   1. MEDIA_ROOT is a real directory (lstat), not a symlink, uid 0.
#   2. The mountpoint is one safe APEX-* component under MEDIA_ROOT.
#   3. An unmounted mountpoint is a real directory, not a symlink, uid 0.
#   4. realpath(mountpoint) equals the constructed MEDIA_ROOT/label.
#   5. umount only if /proc/self/mountinfo has that exact TARGET, and the
#      SOURCE is the expected block device (or its LABEL matches).
# Never mkdir -p through a path we have not lstat'd. Full openat2-relative
# ops are finding 8 — this slice fails closed on any symlink hop.
#
# Sourced by the helpers. Installed next to them in /usr/local/lib/apexos/.

# Kernel device name (sdb1, nvme0n1p1, mmcblk0p1). No slashes, no `..`.
safe_kernel_dev() {
  [[ "${1:-}" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || return 1
  [[ "$1" != *..* ]] || return 1
  return 0
}

# Filesystem label → one path component. Mirrors valid_exo_label.
safe_exo_label() {
  local raw="${1:-}"
  [[ "$raw" == APEX-* ]] || return 1
  local s
  s="$(printf '%s' "$raw" | tr -cd 'A-Za-z0-9._-')"
  [[ -n "$s" && "$s" == "$raw" && "$s" != *..* && "$s" != APEX- ]] || return 1
  [[ "${#s}" -le 64 ]] || return 1
  printf '%s' "$s"
}

usb_workspace() {
  if [[ -n "${APEXOS_USB_WORKSPACE:-}" ]]; then
    printf '%s' "${APEXOS_USB_WORKSPACE%/}"
    return 0
  fi
  # shellcheck disable=SC1091
  local w
  w="$(. /etc/agentd/env 2>/dev/null; echo "${AGENTD_WORKSPACE:-/var/lib/agentd/workspace}")"
  printf '%s' "${w%/}"
}

usb_media_root() {
  printf '%s' "$(usb_workspace)/media"
}

# lstat: refuse if the path is a symlink. `[ -d ]` follows — check -L first.
assert_not_symlink() {
  local p="${1:?}" what="${2:-path}"
  if [[ -L "$p" ]]; then
    echo "usb-confine: $what is a symlink: $p" >&2
    return 1
  fi
  return 0
}

# Real directory, not a symlink, owned by root. Fail closed if stat is missing.
assert_root_owned_dir() {
  local p="${1:?}" what="${2:-path}"
  assert_not_symlink "$p" "$what" || return 1
  if [[ ! -d "$p" ]]; then
    echo "usb-confine: $what is not a directory: $p" >&2
    return 1
  fi
  command -v stat >/dev/null 2>&1 || { echo "usb-confine: stat(1) required" >&2; return 1; }
  local u
  u="$(stat -c %u "$p" 2>/dev/null || true)"
  if [[ "$u" != 0 ]]; then
    echo "usb-confine: $what is not root-owned (uid=${u:-?}): $p" >&2
    return 1
  fi
  return 0
}

# String confine: $1 must be strictly under $2/ (prefix match on the
# constructed path, before any resolution).
assert_under() {
  local path="${1:?}" root="${2:?}"
  case "$path" in
    "${root}/"*) return 0 ;;
    *) echo "usb-confine: $path is outside $root" >&2; return 1 ;;
  esac
}

# No symlink hop between MEDIA_ROOT and the mountpoint.
assert_canonical_mnt() {
  local mnt="${1:?}" expected="${2:?}"
  command -v realpath >/dev/null 2>&1 || { echo "usb-confine: realpath(1) required" >&2; return 1; }
  local got
  got="$(realpath -e "$mnt" 2>/dev/null || true)"
  if [[ -z "$got" || "$got" != "$expected" ]]; then
    echo "usb-confine: canonical path '$got' != '$expected'" >&2
    return 1
  fi
  return 0
}

# Exact TARGET lookup in mountinfo (kernel path, no symlink follow).
# Prints SOURCE on success.
mountinfo_source_for() {
  local want="${1:?}"
  [[ -r /proc/self/mountinfo ]] || return 1
  local id parent majmin root target rest src
  while read -r id parent majmin root target rest; do
    [[ "$target" == "$want" ]] || continue
    src="${rest#* - }"
    src="${src#* }"
    src="${src%% *}"
    [[ -n "$src" ]] || return 1
    printf '%s' "$src"
    return 0
  done < /proc/self/mountinfo
  return 1
}

# SOURCE matches the expected block device (path, realpath, or by-uuid/by-label).
source_is_dev() {
  local src="${1:?}" dev="${2:?}"
  [[ "$src" == "$dev" ]] && return 0
  local rsrc rdev
  rdev="$(realpath -e "$dev" 2>/dev/null || true)"
  [[ -n "$rdev" && "$src" == "$rdev" ]] && return 0
  rsrc="$(realpath -e "$src" 2>/dev/null || true)"
  [[ -n "$rsrc" && -n "$rdev" && "$rsrc" == "$rdev" ]] && return 0
  return 1
}

# Reclaim / create MEDIA_ROOT as a root-owned real directory + sentinel so
# agentd cannot rmdir it (parent workspace is agentd-writable).
ensure_media_root() {
  local media="${1:?}"
  assert_not_symlink "$media" "media root" || return 1
  if [[ ! -e "$media" ]]; then
    local parent
    parent="$(dirname "$media")"
    assert_not_symlink "$parent" "workspace" || return 1
    [[ -d "$parent" ]] || { echo "usb-confine: workspace missing: $parent" >&2; return 1; }
    mkdir -m 0755 "$media" || return 1
  fi
  # Migration: old installs created media as agentd-owned.
  if [[ -d "$media" && ! -L "$media" ]]; then
    chown root:root "$media" 2>/dev/null || true
    chmod 0755 "$media" 2>/dev/null || true
  fi
  assert_root_owned_dir "$media" "media root" || return 1
  local sentinel="${media}/.apexos-media"
  if [[ -L "$sentinel" ]]; then
    echo "usb-confine: refusing symlink sentinel $sentinel" >&2
    return 1
  fi
  if [[ ! -e "$sentinel" ]]; then
    : > "$sentinel" || return 1
  fi
  chown root:root "$sentinel" 2>/dev/null || true
  chmod 0444 "$sentinel" 2>/dev/null || true
  return 0
}

# Create or reclaim one mountpoint under a verified media root. Prints MNT.
prepare_mountpoint() {
  local media="${1:?}" label="${2:?}"
  local mnt="${media}/${label}"
  assert_under "$mnt" "$media" || return 1
  assert_not_symlink "$mnt" "mountpoint" || return 1
  if [[ ! -e "$mnt" ]]; then
    mkdir -m 0755 "$mnt" || return 1
    chown root:root "$mnt" 2>/dev/null || true
  elif [[ -d "$mnt" ]] && ! mountinfo_source_for "$mnt" >/dev/null; then
    chown root:root "$mnt" 2>/dev/null || true
    chmod 0755 "$mnt" 2>/dev/null || true
  fi
  # After FAT mount the covering inode is uid=agentd — only require root
  # ownership when this path is not already our mount.
  if ! mountinfo_source_for "$mnt" >/dev/null; then
    assert_root_owned_dir "$mnt" "mountpoint" || return 1
  else
    assert_not_symlink "$mnt" "mountpoint" || return 1
    [[ -d "$mnt" ]] || { echo "usb-confine: mountpoint is not a directory: $mnt" >&2; return 1; }
  fi
  local expected
  expected="$(realpath -e "$media")/${label}"
  assert_canonical_mnt "$mnt" "$expected" || return 1
  printf '%s' "$mnt"
}

# Ready to umount: not a symlink, exact TARGET in mountinfo, SOURCE ok.
# Prints SOURCE. Returns 1 if the path is hostile; 2 if not mounted.
assert_umount_ok() {
  local mnt="${1:?}" expected_dev="${2:-}" expected_label="${3:-}"
  assert_not_symlink "$mnt" "umount target" || return 1
  local src
  src="$(mountinfo_source_for "$mnt" || true)"
  if [[ -z "$src" ]]; then
    return 2
  fi
  if [[ -n "$expected_dev" ]]; then
    source_is_dev "$src" "$expected_dev" || {
      echo "usb-confine: mount at $mnt is $src, not $expected_dev" >&2
      return 1
    }
  fi
  if [[ -n "$expected_label" ]]; then
    local have
    have="$(blkid -s LABEL -o value "$src" 2>/dev/null || true)"
    if [[ -n "$have" && "$have" != "$expected_label" ]]; then
      echo "usb-confine: mount at $mnt has label '$have', not '$expected_label'" >&2
      return 1
    fi
  fi
  printf '%s' "$src"
  return 0
}

# Remove an empty leftover mountpoint — never rm -f, never follow a symlink.
rmdir_mountpoint() {
  local mnt="${1:?}"
  [[ -L "$mnt" ]] && return 0
  [[ -d "$mnt" ]] || return 0
  local u
  u="$(stat -c %u "$mnt" 2>/dev/null || true)"
  [[ "$u" == 0 ]] || return 0
  rmdir "$mnt" 2>/dev/null || true
}
