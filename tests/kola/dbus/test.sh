#!/bin/bash
## kola:
##   distros: fcos
##   tags: "platform-independent"
##   # D-Bus ESP mount only relevant on EFI systems
##   architectures: "x86_64 aarch64"
##   description: Verify the bootupd D-Bus ESP mount interface works

set -xeuo pipefail

# shellcheck disable=SC1091
. "${KOLA_EXT_DATA}/libtest.sh"

# Error when unmounting without mount
if busctl call org.coreos.Bootupd1 /org/coreos/Bootupd org.coreos.Bootupd1 UnmountEsp; then
    fatal "Calling UnmountEsp before MountEsp should be an error"
fi
ok "UnmountEsp fails when called before MountEsp"

# Request ESP mount
mountpoint=$(busctl call --json=short org.coreos.Bootupd1 /org/coreos/Bootupd org.coreos.Bootupd1 MountEsp | jq -r '.data[0]')
if [ -z "${mountpoint}" ]; then
    fatal "MountEsp returned an empty path"
fi
if [ ! -d "${mountpoint}" ]; then
    fatal "MountEsp did not return a valid directory"
fi
if ! findmnt -n -o FSTYPE "${mountpoint}" | grep -q vfat; then
    fatal "MountEsp mounted a filesystem that is not vfat"
fi
ok "ESP mounted successfully"

# Mountpoint property matches returned mount point
prop=$(busctl get-property --json=short org.coreos.Bootupd1 /org/coreos/Bootupd org.coreos.Bootupd1 MountPoint | jq -r '.data')
if [ "${prop}" != "${mountpoint}" ]; then
    fatal "MountPoint property '${prop}' did not match given mount point '${mountpoint}'"
fi
ok "MountPoint property matches the given mount point"

# Idempotent second mount
mountpoint2=$(busctl call --json=short org.coreos.Bootupd1 /org/coreos/Bootupd org.coreos.Bootupd1 MountEsp | jq -r '.data[0]')
if [ "${mountpoint2}" != "${mountpoint}" ]; then
    fatal "Second MountEsp call returned a different path: '${mountpoint2}' instead of '${mountpoint}'"
fi
ok "MountEsp is idempotent"

# Write a file
if ! echo "bootupd-dbus-test" > "${mountpoint}/bootupd-dbus-test"; then
    fatal "Failed to write to the mounted ESP"
fi
ok "Wrote to the mounted ESP successfully"

# Unmount ESP
if ! busctl call org.coreos.Bootupd1 /org/coreos/Bootupd org.coreos.Bootupd1 UnmountEsp; then
    fatal "UnmountEsp failed"
fi
if findmnt "${mountpoint}" > /dev/null 2>&1; then
    fatal "ESP still mounted after calling UnmountEsp"
fi
ok "Unmounted ESP successfully"

dev=$(lsblk -J -o PATH,PARTTYPE -l \
    | jq -r '.blockdevices[] | select(.parttype == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b") | .path')
tmp_mount=$(mktemp -d)
mount "${dev}" "${tmp_mount}"
assert_file_has_content_literal "${tmp_mount}/bootupd-dbus-test" "bootupd-dbus-test"
umount "${tmp_mount}"
ok "File persisted after unmount"
