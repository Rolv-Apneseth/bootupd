# D-Bus ESP Mount API for bootupd

Context: https://github.com/coreos/fedora-coreos-tracker/issues/1623
See also: https://github.com/coreos/fedora-coreos-tracker/issues/1623#issuecomment-2115934941

fwupd cannot update firmware on FCOS because the ESP is not mounted and
udisks2 (which fwupd uses for ESP discovery/mounting) is not shipped.
Adding udisks2 is not viable (pulls in Python transitively). bootupd
already knows how to find and mount the ESP, so it should expose that
capability over D-Bus for fwupd and other consumers.

## Interface

**Bus name:** `org.coreos.Bootupd1`
**Object path:** `/org/coreos/Bootupd`
**Interface:** `org.coreos.Bootupd1`

### Methods

#### `MountEsp(target: s) → s`

Caller passes an empty directory path. bootupd discovers the ESP
device (`list_dev_current_root` → `find_colocated_esps`), mounts it at
`target`, adds the caller's D-Bus unique name as a lease holder, and
returns the mount path.

Semantics when the ESP is already mounted:
- **Same caller, same path:** idempotent — already has a lease, return
  the path.
- **Different caller, same path:** add a second lease, return the path.
- **Any caller, different path:** error — the ESP is already mounted
  elsewhere.

#### `UnmountEsp()`

Remove the caller's lease. If no leases remain, sync the primary ESP
to all other ESPs (with timestamp bumping), then unmount. Error if the
caller has no lease.

### Properties

#### `MountPoint: s` (read-only)

Current mount path, or empty string if not mounted. Lets callers check
state without side effects.

### Lease tracking

Callers are tracked by D-Bus unique name (`:1.42` etc.), extracted via
`#[zbus(header)]` on each method. Stored as `HashSet<String>`.

`NameOwnerChanged` monitoring for crash cleanup is deferred for v1. If
a caller crashes without calling `UnmountEsp`, its lease persists until
the service exits via idle timeout. A subsequent `MountEsp` call from
the same or different caller will still work.

### Internal state

```rust
struct State {
    guard: MountGuard,        // from bootc-internal-mount, synchronous unmount on drop
    mountpoint: PathBuf,
    primary_device: PathBuf,
    all_devices: Vec<PathBuf>,
    leases: HashSet<String>,
    synced: bool,             // prevents double-sync (explicit + Drop)
}
```

`Mutex<Option<State>>` — `None` = not mounted, `Some` = mounted.

### ESP sync on unmount

When all leases are released:
1. Bump root mtime on primary ESP via `rustix::fs::utimensat` with
   `UTIME_NOW`
2. For each other ESP: mount at tmpdir via `TempMount`, compute diff
   via `filetree::FileTree` / `apply_diff`, bump its root mtime to
   match, unmount
3. Set `synced = true` (so `Drop` doesn't repeat the sync)
4. Drop `MountGuard` (synchronous unmount of primary)

`Drop` on `State` calls `sync_all_esps()` as a safety net for abnormal
exits; the `synced` flag makes it a no-op after explicit sync.

### Dependencies used

- **`bootc-internal-mount`**: `MountGuard` (primary mount, syscall-
  based, synchronous unmount), `TempMount` (sync targets, auto tempdir)
- **`bootc-internal-blockdev`**: `list_dev_current_root`,
  `find_colocated_esps` (ESP device discovery)
- **`bootc-internal-utils`**: `CommandRunExt` (`run_inherited` etc.)
- **`filetree`** (in-tree): `FileTree::new_from_dir`,
  `relative_diff_to`, `apply_diff` for ESP sync
- **`zbus`** 5.17: `#[interface]` macro, `blocking::connection::Builder`,
  `Connection::monitor_activity()`, `fdo::Error`/`fdo::Result`

## Tasks

### 1. Create `src/dbus.rs` ✓

D-Bus interface with `MountEsp`, `UnmountEsp`, `MountPoint` property,
lease tracking, ESP sync with timestamp bumping.

### 2. Add `dbus` subcommand to `src/cli/bootupd.rs`

New `DVerb::Dbus` variant. The handler:

- Builds a `zbus::blocking::connection::Builder::system()` connection
- Requests bus name `org.coreos.Bootupd1`
- Serves the `BootupdDbus` interface at `/org/coreos/Bootupd`
- Shares `Mutex<Option<State>>` via `Arc` between the interface struct
  and the idle loop (since `serve_at` moves the interface into the
  object server)
- Uses `Connection::monitor_activity()` with
  `wait_timeout(Duration::from_secs(30))` to implement idle exit —
  only exits if no active leases
- On exit, `State` is dropped, which syncs and unmounts

### 3. Register `mod dbus` in `src/main.rs`

Add `mod dbus;` to the module list.

### 4. D-Bus bus policy config

**File:** `dbus-1/system.d/org.coreos.Bootupd1.conf`

XML policy allowing:
- Root to own the bus name and call methods
- Default policy to introspect only

### 5. D-Bus service file for bus activation

**File:** `dbus-1/system-services/org.coreos.Bootupd1.service`

```ini
[D-BUS Service]
Name=org.coreos.Bootupd1
Exec=/usr/libexec/bootupd dbus
User=root
SystemdService=bootupd-dbus.service
```

When a client calls a method on `org.coreos.Bootupd1` and no process
owns the name, dbus-daemon starts the service automatically.

### 6. systemd unit for D-Bus activation

**File:** `systemd/bootupd-dbus.service`

```ini
[Unit]
Description=Bootupd D-Bus service (ESP management)

[Service]
Type=dbus
BusName=org.coreos.Bootupd1
ExecStart=/usr/libexec/bootupd dbus
PrivateNetwork=yes
ProtectHome=yes
KillMode=mixed
```

Key difference from `bootloader-update.service`: NO `MountFlags=slave`.
Mounts made by this service must be visible in the global namespace so
that fwupd (running in a separate service) can access the ESP.

### 7. Update Makefile

Add install targets for the D-Bus config and service files alongside the
existing `install-systemd-unit` target.

### 8. ESP heal on boot

If a previous D-Bus service crashed mid-sync, ESPs may be inconsistent
(one has newer data than the others). This is detected by comparing
root directory mtimes across ESPs.

This does NOT belong in the D-Bus module — it should run during the
normal boot path, before any ESP operations. The right place is in the
existing `bootupctl update` flow, which runs via
`bootloader-update.service` on every boot:

- **Location:** `src/bootupd.rs`, in `prep_before_update()` or at the
  start of `client_run_update()`
- **Logic:** If multiple ESPs exist, mount each read-only, compare root
  mtimes. If they differ, mount the newest one read-write, sync to all
  others via `FileTree`/`apply_diff`, bump all timestamps to match.
- **Single-ESP systems:** no-op (the common case).

This ensures the D-Bus service can always assume ESPs are consistent
when it starts.

## Design decisions

- **Caller-specified mount point:** Per cgwalters' suggestion, the
  caller provides an empty directory to mount to. This decouples bootupd
  from assumptions about mount paths and leaves the door open for FUSE
  in the future.
- **FUSE for RAID (future):** In multi-ESP RAID setups, `MountEsp`
  could present a FUSE filesystem that transparently mirrors writes to
  all ESPs. The D-Bus API won't need to change — the caller just sees
  a directory. For v1, plain mount + sync-on-unmount with timestamp
  tracking.
- **Global namespace mount:** The D-Bus service omits `MountFlags=slave`
  so the ESP mount is visible to other services (fwupd). The FCOS
  objection was to *persistent* fstab mounts, not transient on-demand
  mounts.
- **Blocking API:** Uses `zbus::blocking`. The `#[interface]` macro
  generates async signatures internally but the blocking connection
  builder handles this transparently. No tokio/async runtime needed.
- **zbus features:** Default features (`async-io` + `blocking-api`) are
  required; `async-io` is the I/O backend and cannot be dropped.
- **No `Efi` struct reuse:** The D-Bus module mounts to caller-specified
  paths and will support FUSE in the future, so it doesn't fit the
  `Efi` struct's well-known-path model. The `&mut self` refactor still
  stands on its own merits as a cleanup.
