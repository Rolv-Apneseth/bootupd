#!/bin/bash
## kola:
##   platforms: qemu
##   minMemory: 4096
##   additionalDisks: ["10G"]
##   architectures: "x86_64"
##   description: Verify D-Bus ESP mount syncs across multiple ESPs in a RAID1 mirror

set -xeuo pipefail

# shellcheck disable=SC1091
. "${KOLA_EXT_DATA}/libtest.sh"

# Request ESP mount
mountpoint=$(busctl call --json=short org.coreos.Bootupd1 /org/coreos/Bootupd org.coreos.Bootupd1 MountEsp \
    | jq -r '.data[0]')
if [ -z "${mountpoint}" ]; then
    fatal "MountEsp returned an empty path"
fi
if [ ! -d "${mountpoint}" ]; then
    fatal "MountEsp did not return a valid directory"
fi
ok "ESP mounted successfully"

if ! findmnt -n -o FSTYPE "${mountpoint}" | grep -q vfat; then
    fatal "Mount not visible to test process"
fi

# Write test file to primary ESP
test_content="dbus-raid-sync-test-$(date)"
if ! echo "${test_content}" > "${mountpoint}/dbus-raid-test"; then
    fatal "Failed to write to mounted ESP"
fi
ok "Wrote test file to mounted ESP"

# Unmount, triggering a sync to secondary ESP
if ! busctl call org.coreos.Bootupd1 /org/coreos/Bootupd org.coreos.Bootupd1 UnmountEsp; then
    fatal "UnmountEsp failed"
fi
ok "Unmounted ESP successfully"

# Verify file was synced and sync markers exist
esp_devices=$(lsblk -J -o PATH,PARTTYPE -l \
    | jq -r '.blockdevices[] | select(.parttype == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b") | .path')
timestamps=""
for dev in ${esp_devices}; do
    tmp_mount=$(mktemp -d)
    mount "${dev}" "${tmp_mount}"
    if ! grep -q "${test_content}" "${tmp_mount}/dbus-raid-test"; then
        fatal "Test file not synced to ESP device ${dev}"
    fi
    if [ ! -f "${tmp_mount}/.bootupd-esp-sync.json" ]; then
        fatal "Sync marker not found on ESP device ${dev}"
    fi

    ts=$(jq -r '.timestamp' "${tmp_mount}/.bootupd-esp-sync.json")
    timestamps="${timestamps} ${ts}"

    umount "${tmp_mount}"
done

# For additional logs
for dev in ${esp_devices}; do
    tmp_mount=$(mktemp -d)
    mount "${dev}" "${tmp_mount}"
    echo "Contents of ${dev}:"
    ls -la "${tmp_mount}/"
    umount "${tmp_mount}"
done

# Verify timestamps on sync markers match
unique_timestamps=$(echo "${timestamps# }" | tr ' ' '\n' | sort -u | wc -l)
if [ "${unique_timestamps}" -ne 1 ]; then
    fatal "More than 1 unique timestamp in sync markers (failed sync)"
fi
ok "Sync markers have matching timestamps"

# Check that mounting + unmounting without making changes does not update the
# sync marker
timestamp=$(echo "${timestamps}" | cut -d' ' -f2)
if ! busctl call --json=short org.coreos.Bootupd1 /org/coreos/Bootupd org.coreos.Bootupd1 MountEsp \
    | jq -r '.data[0]'; then
    fatal "MountEsp failed"
fi
if ! busctl call org.coreos.Bootupd1 /org/coreos/Bootupd org.coreos.Bootupd1 UnmountEsp; then
    fatal "UnmountEsp failed"
fi

for dev in ${esp_devices}; do
    tmp_mount=$(mktemp -d)
    mount "${dev}" "${tmp_mount}"
    ts=$(jq -r '.timestamp' "${tmp_mount}/.bootupd-esp-sync.json")
    if [ "${ts}" != "${timestamp}" ]; then
        fatal "Timestamp was updated on ESP device ${dev}"
    fi
    umount "${tmp_mount}"
done
ok "Sync marker timestamps not updated when no change made"
