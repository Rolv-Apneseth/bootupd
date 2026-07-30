use std::{path::PathBuf, sync::Mutex};

use anyhow::{Context, Result};
use bootc_internal_mount::tempmount::MountGuard;
use cap_std::{
    ambient_authority,
    fs::{Dir, PermissionsExt},
};
use cap_std_ext::dirext::CapStdExtDirExt;
use chrono::Utc;
use fn_error_context::context;
use zbus::fdo;

use crate::{
    backend::StateLockGuard,
    bootupd::list_dev_current_root,
    efi::{mount_esp_shared, ESP_FS_TYPE, ESP_MOUNT_FLAGS},
    filetree,
    freezethaw::fsfreeze_thaw_cycle,
    model::{EspSyncState, SavedState},
};

pub(crate) const DBUS_NAME: &str = "org.coreos.Bootupd1";
pub(crate) const DBUS_OBJ_PATH: &str = "/org/coreos/Bootupd";

/// Discover all available ESPs
#[context("discovering ESP devices")]
fn discover_esps() -> Result<Option<Vec<PathBuf>>> {
    let root_device = list_dev_current_root()?;
    let esps = root_device
        .find_colocated_esps()
        .context("Finding co-located ESPs")?
        .map(|devices| {
            devices
                .into_iter()
                .map(|d| PathBuf::from(d.path()))
                .collect::<Vec<PathBuf>>()
        });

    Ok(esps)
}

/// Sync a mounted primary ESP to all other given ESP devices
#[context("syncing devices")]
fn sync_devices(
    primary_dev: &PathBuf,
    primary_dir: &Dir,
    primary_tree: &filetree::FileTree,
    all_devices: &[PathBuf],
) -> Result<()> {
    for device in all_devices.iter().filter(|p| *p != primary_dev) {
        let mount = mount_esp_shared(device.to_str().expect("should be UTF-8"))
            .with_context(|| format!("Mounting {device:?} for sync"))?;

        let device_tree = filetree::FileTree::new_from_dir(&mount.fd, None)?;
        let diff = device_tree.diff(primary_tree)?;

        filetree::apply_diff(&primary_dir, &mount.fd, &diff, None)
            .with_context(|| format!("Syncing to {device:?}"))?;
        fsfreeze_thaw_cycle(mount.fd.reopen_as_ownedfd()?)?;
    }

    Ok(())
}

/// Detect and repair incomplete ESP syncs from a previous cycle.
///
/// This compares the marker files created when syncing ESPs, and if
/// they differ, sync from the ESP with the newest marker to all others.
///
/// No-op if no markers exist or all match.
#[context("healing failed ESP sync")]
fn heal_esp_sync(all_devices: &[PathBuf]) -> Result<()> {
    if all_devices.len() == 1 {
        return Ok(());
    }

    // Read sync marker from each ESP
    let markers = all_devices
        .iter()
        .map(|device| {
            let mount = mount_esp_shared(device.to_str().expect("should be UTF-8"))
                .with_context(|| format!("Mounting device to check sync state: {device:?}"))?;

            let state: Option<EspSyncState> =
                match mount.fd.open_optional(EspSyncState::FILENAME)? {
                    Some(f) => {
                        let reader = std::io::BufReader::new(f);
                        serde_json::from_reader(reader)
                            .with_context(|| format!("Parsing sync marker on {device:?}"))?
                    }
                    None => None,
                };

            Ok((device, state))
        })
        .collect::<Result<Vec<(&PathBuf, Option<EspSyncState>)>>>()
        .context("Collecting sync markers in device ESPs")?;

    // If there are no markers at all, no sync is required
    if markers.iter().all(|(_, s)| s.is_none()) {
        return Ok(());
    }

    // Find device with the newest sync state
    let (primary_dev, newest_sync_state) = markers
        .iter()
        .filter_map(|(dev, s)| s.as_ref().map(|s| (*dev, s)))
        .max_by_key(|(_, s)| s.timestamp)
        .expect("there should be at least 1 ESP sync state at this stage");

    // Check if healing is required
    let needs_sync = markers
        .iter()
        .any(|(_, o)| o.as_ref().map(|s| s.timestamp) != Some(newest_sync_state.timestamp));
    if !needs_sync {
        log::debug!("All ESPs are in sync");
        return Ok(());
    }

    log::warn!("ESP sync inconsistency detected, healing from {primary_dev:?}");

    // Mount primary and sync to other devices
    let primary_mount = mount_esp_shared(primary_dev.to_str().expect("should be UTF-8"))
        .with_context(|| format!("Mounting primary device for heal: {primary_dev:?}"))?;
    let primary_tree = filetree::FileTree::new_from_dir(&primary_mount.fd, None)?;
    sync_devices(&primary_dev, &primary_mount.fd, &primary_tree, &all_devices)?;

    Ok(())
}

/// Convenience extension trait for converting to [`fdo::Error`]
trait FdoResultExt<T> {
    fn to_fdo_err(self) -> fdo::Result<T>;
}
impl<T> FdoResultExt<T> for Result<T> {
    fn to_fdo_err(self) -> fdo::Result<T> {
        self.map_err(|e| fdo::Error::Failed(format!("Internal error: {e:#}")))
    }
}

#[derive(Debug)]
struct State {
    _guard: MountGuard,
    _tempdir: tempfile::TempDir,
    _lock: StateLockGuard,
    mountpoint: PathBuf,
    primary_device: PathBuf,
    initial_tree: filetree::FileTree,
    all_devices: Vec<PathBuf>,
    synced: bool,
}

impl Drop for State {
    // Best-effort sync and umount on drop
    fn drop(&mut self) {
        if let Err(e) = self.sync_all_esps() {
            log::error!("Failed to sync ESPs on drop: {e:#}")
        }
    }
}

impl State {
    /// Sync primary mounted ESP to all other ESPs
    fn sync_all_esps(&mut self) -> Result<()> {
        if self.synced {
            return Ok(());
        }

        if self.all_devices.len() <= 1 {
            log::info!("Only one ESP device - skipping sync");
            return Ok(());
        }

        log::info!("Syncing all ESPs. devices={:?}", self.all_devices);

        let primary_dir = Dir::open_ambient_dir(&self.mountpoint, ambient_authority())?;
        let primary_tree = filetree::FileTree::new_from_dir(&primary_dir, None)?;

        // Skip if primary ESP is unchanged
        let diff = self.initial_tree.diff(&primary_tree)?;
        if diff.is_empty() {
            log::info!("No changes detected on primary ESP. Skipping sync.");
            self.synced = true;
            return Ok(());
        }

        // Sync state to be written to each ESP
        let sync_state = EspSyncState {
            version: EspSyncState::CURRENT_VERSION,
            timestamp: Utc::now(),
        };
        let sync_state_serialized =
            serde_json::to_vec_pretty(&sync_state).context("Serializing sync state")?;
        let sync_state_permissions = cap_std::fs::Permissions::from_mode(0o644);

        // Write sync state to primary device first - this will be written to each
        // ESP while syncing below.
        primary_dir
            .atomic_write_with_perms(
                EspSyncState::FILENAME,
                &sync_state_serialized,
                sync_state_permissions.clone(),
            )
            .context("Writing sync marker to primary ESP")?;
        fsfreeze_thaw_cycle(primary_dir.reopen_as_ownedfd()?)?;

        // Rebuild the primary tree so it includes the sync marker
        let primary_tree = filetree::FileTree::new_from_dir(&primary_dir, None)?;

        sync_devices(
            &self.primary_device,
            &primary_dir,
            &primary_tree,
            &self.all_devices,
        )?;

        self.synced = true;
        Ok(())
    }
}

#[derive(Debug, Default)]
pub(crate) struct BootupdDbus {
    state: Mutex<Option<State>>,
}

impl BootupdDbus {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_mounted(&self) -> bool {
        self.state
            .lock()
            .expect("mutex should not be poisoned")
            .is_some()
    }
}

#[zbus::interface(name = "org.coreos.Bootupd1")]
#[allow(clippy::unused_async)]
impl BootupdDbus {
    // SAFETY: It's valid to return a regular String here since D-Bus strings are also
    //         guaranteed UTF-8 without a null terminator.
    async fn mount_esp(&self) -> fdo::Result<String> {
        let mut state_guard = self.state.lock().expect("mutex should not be poisoned");

        // Already mounted, so just return the existing path
        if let Some(state) = state_guard.as_mut() {
            return Ok(state
                .mountpoint
                .to_str()
                .expect("tempdir path should be UTF-8")
                .to_owned());
        }

        // Find all ESP devices
        let all_devices = discover_esps()
            .to_fdo_err()?
            .ok_or_else(|| fdo::Error::Failed("No ESP devices found".into()))?;
        let primary_device = all_devices[0].clone();

        log::info!("Discovered ESP devices: {:?}", all_devices);
        log::info!("Primary ESP: {:?}", primary_device);

        // Acquire bootupd state lock
        let sysroot = Dir::open_ambient_dir("/", ambient_authority())
            .with_context(|| {
                format!("opening ambient dir on primary device sysroot: {primary_device:?}")
            })
            .to_fdo_err()?;
        let lock = SavedState::acquire_write_lock("/".into(), sysroot)
            .context("acquiring bootupd state lock")
            .to_fdo_err()?;

        // Before proceeding, heal ESPs if needed
        heal_esp_sync(&all_devices).to_fdo_err()?;

        let tempdir = tempfile::tempdir()
            .context("creating temp dir")
            .to_fdo_err()?;
        let mountpoint = tempdir.path().to_owned();

        log::info!("Mounting ESP {:?} at {:?}", primary_device, mountpoint);

        let guard = MountGuard::mount(
            all_devices[0].to_str().expect("should be UTF-8"),
            mountpoint.clone(),
            ESP_FS_TYPE,
            ESP_MOUNT_FLAGS,
            // No restrictions (unlike internal bootupd mounts) so that non-root callers
            // (e.g. fwupd-refresh, which will be added to the D-Bus policy in the future) can
            // read the mounted ESP
            None,
        )
        .to_fdo_err()?;

        let mountpoint_str = mountpoint
            .to_str()
            .expect("tempdir path should be UTF-8")
            .to_owned();

        let primary_dir = Dir::open_ambient_dir(&mountpoint, ambient_authority())
            .with_context(|| format!("opening ambient dir at mount point {mountpoint_str}"))
            .to_fdo_err()?;
        let initial_tree = filetree::FileTree::new_from_dir(&primary_dir, None).to_fdo_err()?;

        let state = State {
            _guard: guard,
            _tempdir: tempdir,
            _lock: lock,
            mountpoint,
            primary_device,
            all_devices,
            synced: false,
            initial_tree,
        };
        *state_guard = Some(state);

        Ok(mountpoint_str)
    }

    async fn unmount_esp(&self) -> fdo::Result<()> {
        let mut state_guard = self.state.lock().expect("mutex should not be poisoned");
        let Some(state) = state_guard.as_mut() else {
            return Err(fdo::Error::Failed("No active mount".into()));
        };

        state.sync_all_esps().to_fdo_err()?;

        let primary_dir = Dir::open_ambient_dir(&state.mountpoint, ambient_authority())
            .and_then(|d| d.reopen_as_ownedfd())
            .context("opening owned fd on primary device mountpoint")
            .to_fdo_err()?;
        fsfreeze_thaw_cycle(primary_dir).to_fdo_err()?;

        *state_guard = None;
        Ok(())
    }

    #[zbus(property)]
    async fn mount_point(&self) -> String {
        self.state
            .lock()
            .expect("mutex should not be poisoned")
            .as_ref()
            .and_then(|s| s.mountpoint.to_str())
            .unwrap_or("")
            .to_owned()
    }
}
