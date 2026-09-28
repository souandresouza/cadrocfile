//! Drive discovery and mounting, backed by UDisks2 over D-Bus.
//!
//! UDisks2 rather than plain `mount`: it mounts as the invoking user through
//! polkit, so the common cases need no password at all and none of this needs
//! to run as root. The whole device tree arrives in one `GetManagedObjects`
//! round-trip, which is what makes the sidebar populate instantly.
//!
//! NTFS gets extra attention. Volumes left dirty by Windows Fast Startup or
//! hibernation are refused by ntfs-3g, and UDisks2 surfaces that as an opaque
//! error; [`classify_mount_error`] turns it into something the UI can offer a
//! real fix for. See [`repair_and_mount`] and [`force_mount`].
use crate::tr;

mod health;
mod proxy;

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, Once},
};

use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use proxy::{DriveProxyBlocking, EncryptedProxyBlocking, FilesystemProxyBlocking};

const UDISKS_SERVICE: &str = "org.freedesktop.UDisks2";
const IFACE_BLOCK: &str = "org.freedesktop.UDisks2.Block";
const IFACE_FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const IFACE_PARTITION: &str = "org.freedesktop.UDisks2.Partition";
const IFACE_DRIVE: &str = "org.freedesktop.UDisks2.Drive";

/// Partition type IDs that hold no user data and only clutter a sidebar.
///
/// GPT GUIDs and MBR type bytes both appear in `Partition.Type`, so both forms
/// are listed.
const HIDDEN_PARTITION_TYPES: &[&str] = &[
    "c12a7328-f81f-11d2-ba4b-00a0c93ec93b", // EFI System
    "e3c9e316-0b5c-4db8-817d-f92df00215ae", // Microsoft Reserved
    "de94bba4-06d1-4d40-a16a-bfd50179d6ac", // Windows Recovery Environment
    "21686148-6449-6e6f-744e-656564454649", // BIOS boot
    "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f", // Linux swap
    "bc13c2ff-59e6-4262-a352-b275fd6f7172", // Extended boot (XBOOTLDR)
    "0x27",                                 // MBR: Windows recovery
    "0xef",                                 // MBR: EFI system
    "0x82",                                 // MBR: Linux swap
];

/// Filesystems Windows uses, for the "Windows" sidebar grouping.
const WINDOWS_FILESYSTEMS: &[&str] = &["ntfs", "ntfs3", "exfat", "refs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VolumeCategory {
    /// USB sticks, external SSDs, SD cards.
    Removable,
    /// Internal NTFS/exFAT partitions — the other half of a dual boot.
    Windows,
    /// Internal Linux data partitions that aren't the running root.
    Internal,
    Optical,
}

impl VolumeCategory {
    pub fn section_title(self) -> &'static str {
        match self {
            VolumeCategory::Removable => "Removable Devices",
            VolumeCategory::Windows => "Windows",
            VolumeCategory::Internal => "On This Computer",
            VolumeCategory::Optical => "Discs",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Volume {
    /// UDisks2 object path of the block device — the handle for every operation.
    pub object_path: String,
    pub drive_path: Option<String>,
    pub device: PathBuf,
    /// Filesystem label, or a generated name when the volume has none.
    pub label: String,
    pub uuid: String,
    pub fstype: String,
    pub size: u64,
    pub mount_point: Option<PathBuf>,
    /// A mount point UDisks2 still reports that the kernel can no longer serve.
    ///
    /// Set when a drive was disconnected while something was using it: the
    /// entry survives in the mount table with nothing behind it, so the volume
    /// looks mounted and cannot be mounted again until it is cleared.
    pub stale_mount: Option<PathBuf>,
    pub read_only: bool,
    pub category: VolumeCategory,
    /// Vendor + model of the physical drive, for the tooltip.
    pub drive_name: String,
    pub ejectable: bool,
    pub can_power_off: bool,
    /// True for a LUKS container that must be unlocked before it can be mounted.
    pub is_encrypted: bool,
}

impl Volume {
    pub fn is_mounted(&self) -> bool {
        self.mount_point.is_some()
    }

    pub fn is_ntfs(&self) -> bool {
        matches!(self.fstype.as_str(), "ntfs" | "ntfs3")
    }

    /// Symbolic icon name matching the volume's role.
    pub fn icon_name(&self) -> &'static str {
        match self.category {
            VolumeCategory::Optical => "media-optical-symbolic",
            VolumeCategory::Removable => "drive-removable-media-symbolic",
            VolumeCategory::Windows => "drive-harddisk-symbolic",
            VolumeCategory::Internal => "drive-harddisk-symbolic",
        }
    }

    pub fn size_label(&self) -> String {
        humansize::format_size(self.size, humansize::DECIMAL)
    }
}

/// What went wrong mounting, in terms the UI can act on.
#[derive(Debug, Clone)]
pub enum MountError {
    /// NTFS refused the mount because Windows left the volume dirty or
    /// hibernated. Recoverable — offer read-only, repair, or force.
    NtfsUnclean { message: String, hibernated: bool },
    /// polkit denied or the user dismissed the prompt.
    NotAuthorized(String),
    AlreadyMounted(PathBuf),
    /// The LUKS passphrase was rejected. Nothing was changed on the volume;
    /// the user can simply try again.
    WrongPassphrase,
    Other(String),
}

impl MountError {
    pub fn message(&self) -> String {
        match self {
            MountError::NtfsUnclean { message, .. } => message.clone(),
            MountError::NotAuthorized(m) => m.clone(),
            MountError::AlreadyMounted(p) => format!("Already mounted at {}", p.display()),
            MountError::WrongPassphrase => "That passphrase did not unlock the volume.".to_string(),
            MountError::Other(m) => m.clone(),
        }
    }
}

/// Interface maps come keyed by `OwnedInterfaceName`, which cannot be indexed
/// with a `&str`; look the interface up by name instead.
fn iface<'a>(
    ifaces: &'a HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>,
    name: &str,
) -> Option<&'a HashMap<String, OwnedValue>> {
    ifaces.iter().find(|(key, _)| key.as_str() == name).map(|(_, props)| props)
}

fn connection() -> Result<zbus::blocking::Connection, String> {
    zbus::blocking::Connection::system().map_err(|e| format!("Cannot reach the system bus: {e}"))
}

/// Reads the whole UDisks2 object tree and builds the sidebar's volume list.
/// The whole UDisks2 device tree in one round-trip.
type ManagedObjects =
    HashMap<OwnedObjectPath, HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>>;

fn managed_objects(conn: &zbus::blocking::Connection) -> Result<ManagedObjects, String> {
    let manager = zbus::blocking::fdo::ObjectManagerProxy::builder(conn)
        .destination(UDISKS_SERVICE)
        .map_err(|e| e.to_string())?
        .path("/org/freedesktop/UDisks2")
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| format!("UDisks2 is not available: {e}"))?;

    manager
        .get_managed_objects()
        .map_err(|e| format!("Could not query UDisks2: {e}"))
}

pub fn list_volumes() -> Result<Vec<Volume>, String> {
    let conn = connection()?;
    let objects = managed_objects(&conn)?;

    // Drive interfaces are referenced by block devices, so index them first.
    let drives: HashMap<&OwnedObjectPath, &HashMap<String, OwnedValue>> = objects
        .iter()
        .filter_map(|(path, ifaces)| iface(ifaces, IFACE_DRIVE).map(|props| (path, props)))
        .collect();

    let mounted_root = running_root_device();
    let mut volumes = Vec::new();

    for (path, ifaces) in &objects {
        let Some(block) = iface(ifaces, IFACE_BLOCK) else { continue };
        let Some(volume) = build_volume(path, block, ifaces, &drives, mounted_root.as_deref()) else {
            continue;
        };
        volumes.push(volume);
    }

    // Group by section, then put mounted volumes first and order by label so the
    // list stays stable as drives come and go.
    volumes.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| b.is_mounted().cmp(&a.is_mounted()))
            .then_with(|| crate::fs::entry::natural_cmp(&a.label, &b.label))
    });

    Ok(volumes)
}

fn build_volume(
    path: &OwnedObjectPath,
    block: &HashMap<String, OwnedValue>,
    ifaces: &HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>,
    drives: &HashMap<&OwnedObjectPath, &HashMap<String, OwnedValue>>,
    root_device: Option<&Path>,
) -> Option<Volume> {
    // UDisks2 asks us to skip these outright (device-mapper internals, loop
    // backing files it considers uninteresting).
    if get_bool(block, "HintIgnore") {
        return None;
    }

    let device = PathBuf::from(get_bytestring(block, "Device")?);
    let fstype = get_string(block, "IdType");
    let is_encrypted = iface(ifaces, "org.freedesktop.UDisks2.Encrypted").is_some();

    // A block device with no filesystem and no LUKS header is a whole disk, an
    // extended partition, or unallocated space — nothing to open.
    if fstype.is_empty() && !is_encrypted {
        return None;
    }
    // The crypto container's cleartext device carries the real filesystem; the
    // container itself is listed separately, so don't list the payload twice.
    if get_object_path(block, "CryptoBackingDevice").is_some() {
        return None;
    }

    let partition = iface(ifaces, IFACE_PARTITION);
    let partition_type = partition.map(|p| get_string(p, tr!("Type"))).unwrap_or_default();
    if HIDDEN_PARTITION_TYPES.contains(&partition_type.to_lowercase().as_str()) {
        return None;
    }

    let reported_mount = iface(ifaces, IFACE_FILESYSTEM)
        .and_then(|fsp| first_bytestring_list(fsp, "MountPoints"))
        .map(PathBuf::from);

    // What UDisks2 reports and what the kernel can actually serve are two
    // different things after an unclean disconnect, so the mount point is only
    // believed once it answers. A dead one is kept separately: it is the thing
    // standing between the user and a working drive, and it has to be cleared
    // rather than ignored.
    let (mount_point, stale_mount) = match &reported_mount {
        None => (None, None),
        Some(path) => match health::health(path) {
            health::Health::Live => (reported_mount.clone(), None),
            health::Health::Dead => (None, Some(path.clone())),
            // UDisks2 says mounted, the kernel has no such mount. Nothing to
            // clean up — it simply is not mounted.
            health::Health::Absent => (None, None),
        },
    };

    // Hide the filesystem the running system booted from: navigating "/" is
    // what the Home shortcut and path bar are for, and offering to unmount it
    // would be actively hostile.
    if reported_mount.as_deref() == Some(Path::new("/")) || root_device == Some(device.as_path()) {
        return None;
    }
    if reported_mount.as_deref().is_some_and(|m| m.starts_with("/boot")) {
        return None;
    }

    let drive_props = get_object_path(block, "Drive")
        .and_then(|dp| drives.iter().find(|(p, _)| p.as_str() == dp).map(|(_, v)| *v));

    let connection_bus = drive_props.map(|d| get_string(d, "ConnectionBus")).unwrap_or_default();
    let removable = drive_props.is_some_and(|d| get_bool(d, "Removable"));
    let optical = drive_props.is_some_and(|d| get_bool(d, "Optical"));
    let hint_system = get_bool(block, "HintSystem");

    let category = if optical {
        VolumeCategory::Optical
    } else if removable || connection_bus == "usb" || connection_bus == "sdio" || !hint_system {
        VolumeCategory::Removable
    } else if WINDOWS_FILESYSTEMS.contains(&fstype.to_lowercase().as_str()) {
        VolumeCategory::Windows
    } else {
        VolumeCategory::Internal
    };

    let drive_name = drive_props
        .map(|d| {
            let vendor = get_string(d, "Vendor");
            let model = get_string(d, "Model");
            format!("{vendor} {model}").trim().to_string()
        })
        .unwrap_or_default();

    let label = {
        let id_label = get_string(block, "IdLabel");
        if !id_label.is_empty() {
            id_label
        } else if let Some(name) = partition.map(|p| get_string(p, tr!("Name"))).filter(|n| !n.is_empty()) {
            name
        } else if !drive_name.is_empty() {
            // Fall back to the hardware name, then the raw node, so a label-less
            // partition still reads as something recognisable.
            format!("{drive_name} ({})", device.display())
        } else {
            device.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        }
    };

    Some(Volume {
        object_path: path.as_str().to_string(),
        drive_path: get_object_path(block, "Drive"),
        device,
        label,
        uuid: get_string(block, "IdUUID"),
        fstype,
        size: get_u64(block, tr!("Size")),
        mount_point,
        stale_mount,
        read_only: get_bool(block, "ReadOnly"),
        category,
        drive_name,
        ejectable: drive_props.is_some_and(|d| get_bool(d, "Ejectable")),
        can_power_off: drive_props.is_some_and(|d| get_bool(d, "CanPowerOff")),
        is_encrypted,
    })
}

/// The block device backing `/`, so it can be excluded from the list.
fn running_root_device() -> Option<PathBuf> {
    let text = std::fs::read_to_string("/proc/mounts").ok()?;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let source = parts.next()?;
        let target = parts.next()?;
        if target == "/" && source.starts_with("/dev/") {
            return std::fs::canonicalize(source).ok();
        }
    }
    None
}

// ── D-Bus value helpers ────────────────────────────────────────────────────
// `GetManagedObjects` hands back everything as `OwnedValue`, so each property
// needs a typed read with a sane fallback rather than a panic on shape drift.

fn get_string(props: &HashMap<String, OwnedValue>, key: &str) -> String {
    props
        .get(key)
        .and_then(|v| <&str>::try_from(v).ok())
        .unwrap_or_default()
        .to_string()
}

fn get_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    props.get(key).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

fn get_u64(props: &HashMap<String, OwnedValue>, key: &str) -> u64 {
    props.get(key).and_then(|v| u64::try_from(v).ok()).unwrap_or(0)
}

fn get_object_path(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let value = props.get(key)?;
    let path = OwnedObjectPath::try_from(value.try_clone().ok()?).ok()?;
    // UDisks2 uses "/" as its null object path.
    (path.as_str() != "/").then(|| path.as_str().to_string())
}

/// UDisks2 encodes paths as NUL-terminated byte arrays rather than strings, so
/// they survive filenames that aren't valid UTF-8.
fn get_bytestring(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let value = props.get(key)?;
    let bytes: Vec<u8> = Vec::<u8>::try_from(value.try_clone().ok()?).ok()?;
    decode_bytestring(&bytes)
}

fn first_bytestring_list(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let value = props.get(key)?;
    let list: Vec<Vec<u8>> = Vec::<Vec<u8>>::try_from(value.try_clone().ok()?).ok()?;
    list.first().and_then(|b| decode_bytestring(b))
}

fn decode_bytestring(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    if end == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

// ── Mount operations ───────────────────────────────────────────────────────

fn encrypted_proxy(
    conn: &zbus::blocking::Connection,
    object_path: &str,
) -> Result<EncryptedProxyBlocking<'static>, String> {
    EncryptedProxyBlocking::builder(conn)
        .destination(UDISKS_SERVICE)
        .map_err(|e| e.to_string())?
        .path(object_path.to_string())
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())
}

fn filesystem_proxy(
    conn: &zbus::blocking::Connection,
    object_path: &str,
) -> Result<FilesystemProxyBlocking<'static>, String> {
    FilesystemProxyBlocking::builder(conn)
        .destination(UDISKS_SERVICE)
        .map_err(|e| e.to_string())?
        .path(object_path.to_string())
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())
}

/// Mounts the volume through UDisks2.
///
/// For NTFS this makes two attempts. UDisks2 prefers the in-kernel `ntfs3`
/// driver whenever it appears in `/proc/filesystems`, and `ntfs3` refuses any
/// volume Windows left dirty — which, with Fast Startup enabled, is most of
/// them. Its error says only "wrong fs type, bad option, bad superblock […] or
/// other error", which is why this looks like an unfixable failure in other
/// file managers.
///
/// Asking for `fstype=ntfs` routes the mount through `mount.ntfs`, i.e.
/// `ntfs-3g`, which mounts those same volumes read-write without complaint and
/// without a password. So the fallback is tried automatically, before any
/// dialog is put in front of the user.
/// The `fstype` values to try, in order. An empty string means "let UDisks2
/// choose", which is the in-kernel driver.
///
/// The FUSE driver reads volumes Windows left dirty; the in-kernel `ntfs3`
/// refuses them outright but is markedly faster on the ones it accepts. So the
/// kernel driver leads unless this particular volume has already proved it
/// needs FUSE.
///
/// # It has to be spelled `ntfs`, not `ntfs-3g`
///
/// This is the opposite of what it looks like, and getting it wrong disabled
/// the fallback completely. UDisks2 validates the requested type against its
/// well-known list, `/proc/filesystems` and `/etc/filesystems`, and `ntfs-3g`
/// is in none of them — the kernel registers `ntfs3`, and `ntfs-3g` is the name
/// of a *helper binary*, not a filesystem. Asking for it fails before anything
/// is attempted:
///
/// ```text
/// OptionNotPermitted: Requested filesystem type `ntfs-3g' is neither
/// well-known nor in /proc/filesystems nor in /etc/filesystems
/// ```
///
/// `ntfs` is well-known to UDisks2, and the mount it performs runs
/// `/sbin/mount.ntfs`, which on every distribution that ships ntfs-3g is a
/// symlink to it. The volume comes back as `fuseblk` — FUSE, as intended.
fn mount_order(is_ntfs: bool, prefer_fuse: bool) -> &'static [&'static str] {
    if is_ntfs && prefer_fuse {
        &["ntfs", ""]
    } else {
        &["", "ntfs"]
    }
}

/// Where a mount ended up, and which driver got it there.
///
/// `used_fuse` is fed back into the config so the next mount of the same volume
/// can skip straight to the driver that worked.
pub struct Mounted {
    pub path: PathBuf,
    pub used_fuse: bool,
    /// True when an earlier driver refused the volume and a later one took it.
    ///
    /// The refusal is a failed UDisks2 job, and anything on the desktop that
    /// watches those — udiskie, for one — announces it as an error even though
    /// the mount went on to succeed. The UI uses this to say what happened, so
    /// a notification the user did not ask for has an explanation next to it.
    pub recovered: bool,
}

/// Mounts `volume`, trying the FUSE driver first when asked.
///
/// The order matters for more than speed. Every mount attempt — including a
/// failed one — is a UDisks2 job, and other things on the desktop listen to
/// those: `udiskie` pops a "Job failed" notification for the first attempt even
/// when the retry immediately succeeds, so a working mount still looks broken.
/// Getting it right first time is the only way to keep that quiet.
///
/// `prefer_fuse` is remembered per volume by the caller, so a drive that needed
/// `ntfs-3g` once goes straight there afterwards and the in-kernel driver stays
/// the fast default for volumes that can use it.
pub fn mount(volume: &Volume, prefer_fuse: bool) -> Result<Mounted, MountError> {
    // A dead mount left by an unclean disconnect occupies the mount point and
    // makes every mount attempt fail. Clearing it first is what turns "this
    // drive will not mount any more" back into an ordinary mount.
    if volume.stale_mount.is_some() {
        clear_stale_mount(volume)?;
    }

    let order = mount_order(volume.is_ntfs(), prefer_fuse);

    let mut first_error: Option<MountError> = None;

    for (index, fstype) in order.iter().enumerate() {
        let options: Vec<(&str, &str)> =
            if fstype.is_empty() { Vec::new() } else { vec![("fstype", *fstype)] };

        match mount_with_options(volume, &options) {
            Ok(path) => {
                return Ok(Mounted {
                    path,
                    used_fuse: !fstype.is_empty(),
                    recovered: index > 0,
                });
            }
            Err(error) => {
                // Neither of these is a driver problem, and retrying with a
                // different one would only produce a second identical failure.
                if matches!(
                    error,
                    MountError::NotAuthorized(_) | MountError::AlreadyMounted(_)
                ) {
                    return Err(error);
                }
                // Only NTFS has a second driver worth trying.
                if !volume.is_ntfs() {
                    return Err(error);
                }
                if index == 0 {
                    first_error = Some(error);
                }
            }
        }
    }

    // Report the first failure: it describes the volume, where the fallback's
    // is usually just a repeat from a driver we merely guessed at.
    Err(first_error.unwrap_or_else(|| MountError::Other("Could not mount the volume".into())))
}

/// Releases a mount point the kernel is still holding but cannot serve.
///
/// Forced, because a polite unmount is refused while a process still holds a
/// file on the vanished device — and that process is precisely why the mount
/// went stale. Nothing can be lost by forcing: the device is already gone, so
/// there are no writes left to flush.
///
/// Clearing a mount the user did not make needs admin rights, which is the
/// normal case here: `~/load-ssd.sh` and anything else using `sudo mount`
/// creates a root-owned mount that this session may not touch unaided. polkit
/// prompts, and a refusal is reported as one.
fn clear_stale_mount(volume: &Volume) -> Result<(), MountError> {
    let Some(path) = &volume.stale_mount else { return Ok(()) };

    let conn = connection().map_err(MountError::Other)?;
    let proxy = filesystem_proxy(&conn, &volume.object_path).map_err(MountError::Other)?;

    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("force", Value::from(true));
    opts.insert("auth.no_user_interaction", Value::from(false));

    match proxy.unmount(opts) {
        Ok(()) => Ok(()),
        Err(err) => {
            let raw = err.to_string();
            // Already gone by the time we asked is exactly what we wanted.
            if raw.to_lowercase().contains("not mounted") {
                return Ok(());
            }
            match classify_mount_error(&raw, volume) {
                MountError::NotAuthorized(_) => Err(MountError::NotAuthorized(format!(
                    "{} is left over from a disconnect and has to be cleared before the \
                     drive can be mounted again. That needs administrator approval, because \
                     the mount was not made by this session.",
                    path.display()
                ))),
                other => Err(other),
            }
        }
    }
}

/// Mounts read-only — the safe answer when Windows is hibernated.
///
/// Forced through `ntfs-3g` for the same reason as [`mount`]: `ntfs3` declines
/// a dirty volume even for reading.
pub fn mount_read_only(volume: &Volume) -> Result<PathBuf, MountError> {
    if volume.stale_mount.is_some() {
        clear_stale_mount(volume)?;
    }
    if volume.is_ntfs() {
        // `ntfs`, not `ntfs-3g` — see [`mount_order`] for why the obvious
        // spelling is the one UDisks2 refuses.
        let via_ntfs3g = mount_with_options(volume, &[("fstype", "ntfs"), ("options", "ro")]);
        if via_ntfs3g.is_ok() {
            return via_ntfs3g;
        }
    }
    mount_with_options(volume, &[("options", "ro")])
}

fn mount_with_options(volume: &Volume, options: &[(&str, &str)]) -> Result<PathBuf, MountError> {
    if let Some(existing) = &volume.mount_point {
        return Err(MountError::AlreadyMounted(existing.clone()));
    }

    let conn = connection().map_err(MountError::Other)?;
    let proxy = filesystem_proxy(&conn, &volume.object_path).map_err(MountError::Other)?;

    let mut opts: HashMap<&str, Value> = HashMap::new();
    for (key, value) in options {
        opts.insert(key, Value::from(*value));
    }
    // Without this UDisks2 may pop its own authentication agent in a context
    // where our window can't parent it; the desktop's agent handles it better.
    opts.insert("auth.no_user_interaction", Value::from(false));

    match proxy.mount(opts) {
        Ok(path) => Ok(PathBuf::from(path)),
        Err(err) => Err(classify_mount_error(&err.to_string(), volume)),
    }
}

/// Unlocks a LUKS container and mounts the filesystem inside it.
///
/// Two steps, because they are two different objects: `Unlock` opens a
/// device-mapper mapping and hands back the *cleartext* device, and that is
/// what carries the filesystem to mount. Neither step writes to the LUKS
/// header, so a wrong passphrase costs nothing but an error.
///
/// The passphrase is moved in and dropped with this call; it is never stored,
/// logged, or written to the config.
pub fn unlock_and_mount(
    volume: &Volume,
    passphrase: &str,
    prefer_fuse: bool,
) -> Result<Mounted, MountError> {
    let conn = connection().map_err(MountError::Other)?;

    let cleartext = match unlock(&conn, volume, passphrase) {
        Ok(path) => path,
        // Already open from an earlier unlock, or unlocked by something else:
        // find the mapping that is already there rather than failing.
        Err(MountError::AlreadyMounted(_)) => cleartext_device_of(volume)
            .ok_or_else(|| MountError::Other("The volume is unlocked but its contents could not be found.".into()))?,
        Err(other) => return Err(other),
    };

    let mut opened = volume.clone();
    opened.object_path = cleartext;
    opened.is_encrypted = false;
    opened.mount_point = None;
    mount(&opened, prefer_fuse)
}

fn unlock(
    conn: &zbus::blocking::Connection,
    volume: &Volume,
    passphrase: &str,
) -> Result<String, MountError> {
    let proxy = encrypted_proxy(conn, &volume.object_path).map_err(MountError::Other)?;

    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("auth.no_user_interaction", Value::from(false));

    match proxy.unlock(passphrase, opts) {
        Ok(path) => Ok(path.as_str().to_string()),
        Err(err) => Err(classify_unlock_error(&err.to_string())),
    }
}

/// The cleartext device already backed by `volume`, if the container is open.
fn cleartext_device_of(volume: &Volume) -> Option<String> {
    let conn = connection().ok()?;
    let objects = managed_objects(&conn).ok()?;

    objects.iter().find_map(|(path, ifaces)| {
        let block = iface(ifaces, IFACE_BLOCK)?;
        let backing = get_object_path(block, "CryptoBackingDevice")?;
        (backing == volume.object_path).then(|| path.as_str().to_string())
    })
}

/// Distinguishes a wrong passphrase from every other reason an unlock fails,
/// because it is the only one the user can do anything about.
fn classify_unlock_error(raw: &str) -> MountError {
    let lower = raw.to_lowercase();

    if lower.contains("already unlocked") || lower.contains("already exists") {
        return MountError::AlreadyMounted(PathBuf::new());
    }
    if lower.contains("not authorized") || lower.contains("dismissed") {
        return MountError::NotAuthorized(
            "Authentication is required to unlock this drive.".to_string(),
        );
    }
    // cryptsetup reports a bad passphrase as "no key available with this
    // passphrase"; udisks wraps it, so match on the substance rather than the
    // exact wrapper.
    if lower.contains("no key available")
        || lower.contains("failed to activate")
        || lower.contains("incorrect passphrase")
        || lower.contains("wrong passphrase")
    {
        return MountError::WrongPassphrase;
    }
    MountError::Other(raw.to_string())
}

/// Shown when the mount needs authorisation and nothing can ask for it.
const NO_AGENT_MESSAGE: &str = "This drive needs your password to mount, but no \
authentication agent is running, so nothing can ask for it.\n\n\
Desktop environments start one automatically; a bare window manager usually \
does not. Start one and try again — on Hyprland:\n\n\
    systemctl --user start hyprpolkitagent\n\n\
Add it to your session's autostart to make it permanent.";

/// Whether a polkit authentication agent is registered for this session.
///
/// polkit has no API to ask this directly, so the agents themselves are looked
/// for: they are long-lived processes with recognisable names, and every
/// desktop ships one of them. Used only to choose the wording of an error, so a
/// wrong guess costs nothing worse than a less helpful message.
fn authentication_agent_running() -> bool {
    const AGENTS: &[&str] = &[
        "polkit-gnome-authentication-agent",
        "polkit-kde-authentication-agent",
        "polkit-mate-authentication-agent",
        "hyprpolkitagent",
        "lxpolkit",
        "lxqt-policykit-agent",
        "xfce-polkit",
        "polkit-dumb-agent",
        "soteria",
    ];

    let Ok(entries) = fs::read_dir("/proc") else { return false };
    for entry in entries.filter_map(Result::ok) {
        let Ok(comm) = fs::read_to_string(entry.path().join("comm")) else { continue };
        let comm = comm.trim();
        if AGENTS.iter().any(|agent| agent.starts_with(comm) || comm.starts_with(agent)) {
            return true;
        }
    }
    false
}

/// Maps a raw UDisks2 error string onto an actionable [`MountError`].
fn classify_mount_error(raw: &str, volume: &Volume) -> MountError {
    let lower = raw.to_lowercase();

    // `NotAuthorizedCanObtain` is polkit saying "no, *but* asking would have
    // worked". Mounting an internal partition needs the
    // `filesystem-mount-system` action, whose policy is `auth_admin`, so this is
    // the normal answer — and if no authentication agent is registered for the
    // session there is nothing to show the prompt, and the mount fails
    // instantly with no way for the user to say yes. That is a very different
    // problem from a flat refusal, so it gets a different message.
    if lower.contains("notauthorizedcanobtain") {
        return MountError::NotAuthorized(if authentication_agent_running() {
            "Authentication was not completed.".to_string()
        } else {
            NO_AGENT_MESSAGE.to_string()
        });
    }
    if lower.contains("not authorized") || lower.contains("dismissed") {
        return MountError::NotAuthorized(if authentication_agent_running() {
            "Authentication is required to mount this drive.".to_string()
        } else {
            NO_AGENT_MESSAGE.to_string()
        });
    }

    // UDisks2 refusing the *request* is our mistake, not a dirty volume, and
    // must not be dressed up as one: offering to repair a filesystem when the
    // real problem is an unsupported option sends the user chasing a fault that
    // does not exist.
    if lower.contains("optionnotpermitted") || lower.contains("neither well-known") {
        return MountError::Other(format!(
            "UDisks2 refused the mount options Cadrocfile asked for. This is a bug in \
             Cadrocfile, not a problem with the drive.\n\n{raw}"
        ));
    }

    if volume.is_ntfs() || lower.contains("ntfs") {
        // Hibernation needs a stronger warning, because clearing it discards a
        // saved Windows session rather than just a dirty bit.
        let hibernated = lower.contains("hibernat") || lower.contains("hiberfil");

        // Everything else is treated as recoverable. Matching on wording was a
        // mistake: the message that actually reaches us from the ntfs3 driver
        // is the generic "wrong fs type, bad option, bad superblock […]", which
        // contains none of the words you would look for. On an NTFS volume a
        // failed mount is nearly always Windows leaving it unclean, and every
        // option we offer in response is either safe or explicitly confirmed —
        // so offering them beats a dead end, even in the rare other case.
        return MountError::NtfsUnclean { message: raw.to_string(), hibernated };
    }

    MountError::Other(raw.to_string())
}

pub fn unmount(volume: &Volume) -> Result<(), String> {
    let conn = connection()?;
    let proxy = filesystem_proxy(&conn, &volume.object_path)?;
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("auth.no_user_interaction", Value::from(false));
    proxy.unmount(opts).map_err(|e| clean_dbus_error(&e.to_string()))
}

/// Unmounts, then tells the drive to power down so it's safe to unplug.
pub fn eject(volume: &Volume) -> Result<(), String> {
    if volume.is_mounted() {
        unmount(volume)?;
    } else if volume.stale_mount.is_some() {
        // The drive is not usable but the kernel still holds it, so powering
        // down without clearing that would leave the entry behind for the next
        // time it is plugged in — which is how the problem accumulates.
        clear_stale_mount(volume).map_err(|e| e.message())?;
    }
    let Some(drive_path) = &volume.drive_path else {
        return Ok(());
    };

    let conn = connection()?;
    let proxy = DriveProxyBlocking::builder(&conn)
        .destination(UDISKS_SERVICE)
        .map_err(|e| e.to_string())?
        .path(drive_path.clone())
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;

    let opts: HashMap<&str, Value> = HashMap::new();
    if volume.ejectable {
        proxy.eject(opts.clone()).map_err(|e| clean_dbus_error(&e.to_string()))?;
    }
    if volume.can_power_off {
        // Not every enclosure supports this; a failure here still leaves the
        // drive unmounted and safe to remove, so it isn't worth reporting.
        let _ = proxy.power_off(opts);
    }
    Ok(())
}

/// Strips the D-Bus error-name prefix that would otherwise leak into the UI.
fn clean_dbus_error(raw: &str) -> String {
    raw.rsplit_once(": ").map(|(_, msg)| msg.to_string()).unwrap_or_else(|| raw.to_string())
}

// ── NTFS recovery ──────────────────────────────────────────────────────────

/// True when the tooling needed by [`repair_and_mount`] is installed.
pub fn ntfsfix_available() -> bool {
    which("ntfsfix").is_some()
}

fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join(binary)).find(|c| c.is_file())
    })
}

/// Clears the NTFS dirty flag with `ntfsfix`, then retries the normal mount.
///
/// This is the non-destructive repair: it resets the volume's dirty bit and
/// schedules Windows' own chkdsk for the next boot. It does not touch a
/// hibernation image — [`force_mount`] is for that.
pub fn repair_and_mount(volume: &Volume) -> Result<Mounted, MountError> {
    let ntfsfix = which("ntfsfix")
        .ok_or_else(|| MountError::Other(
            "ntfsfix is not installed. On Arch it comes from the ntfsprogs package."
                .to_string(),
        ))?;

    let output = Command::new("pkexec")
        .arg(ntfsfix)
        .arg("-d")
        .arg(&volume.device)
        .output()
        .map_err(|e| MountError::Other(format!("Could not run ntfsfix: {e}")))?;

    if !output.status.success() {
        // pkexec exits 126/127 when the user cancels or authentication fails.
        if matches!(output.status.code(), Some(126) | Some(127)) {
            return Err(MountError::NotAuthorized(
                "Administrator access is required to repair this volume.".to_string(),
            ));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        return Err(MountError::Other(format!("ntfsfix could not repair the volume: {detail}")));
    }

    // Re-read the volume so the retry sees UDisks2's current mount state.
    let refreshed = refresh(volume).unwrap_or_else(|| volume.clone());
    mount(&refreshed, false)
}

/// Mounts NTFS read-write by discarding the hibernation image.
///
/// Destructive to the *saved Windows session only* — files on the volume are
/// untouched, but an in-progress Windows session cannot be resumed afterwards.
/// The UI must state that plainly before calling this.
pub fn force_mount(volume: &Volume) -> Result<PathBuf, MountError> {
    let mount_point = default_mount_point(volume);

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    // `remove_hiberfile` is ntfs-3g-specific and is exactly what UDisks2 refuses
    // to pass through, which is why this path shells out under pkexec instead.
    let options = format!("remove_hiberfile,uid={uid},gid={gid},umask=022,windows_names");

    let output = Command::new("pkexec")
        .arg("mount")
        .arg("--mkdir")
        .args(["-t", "ntfs-3g"])
        .args(["-o", &options])
        .arg(&volume.device)
        .arg(&mount_point)
        .output()
        .map_err(|e| MountError::Other(format!("Could not run mount: {e}")))?;

    if output.status.success() {
        return Ok(mount_point);
    }
    if matches!(output.status.code(), Some(126) | Some(127)) {
        return Err(MountError::NotAuthorized(
            "Administrator access is required to force this mount.".to_string(),
        ));
    }
    Err(MountError::Other(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// Where a forced mount should land: the same `/media/$USER/<label>` layout
/// UDisks2 uses, so both paths look identical to the rest of the app.
fn default_mount_point(volume: &Volume) -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    let name = if volume.label.is_empty() {
        volume.device.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "volume".into())
    } else {
        // Slashes in a label would silently create nested directories.
        volume.label.replace('/', "_")
    };
    PathBuf::from("/media").join(user).join(name)
}

/// Unmounts a volume that was mounted outside UDisks2 (i.e. by [`force_mount`]).
pub fn unmount_privileged(mount_point: &Path) -> Result<(), String> {
    let output = Command::new("pkexec")
        .arg("umount")
        .arg(mount_point)
        .output()
        .map_err(|e| format!("Could not run umount: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
}

/// Re-reads a single volume's current state after an operation changed it.
pub fn refresh(volume: &Volume) -> Option<Volume> {
    list_volumes()
        .ok()?
        .into_iter()
        .find(|v| v.object_path == volume.object_path)
}

/// Subscribers notified when the device tree changes.
///
/// One watcher thread serves every window: the D-Bus connection and its two
/// signal streams are process-wide resources, and spawning a fresh set per
/// window would leak a thread each time one closed.
static WATCHERS: Mutex<Vec<async_channel::Sender<()>>> = Mutex::new(Vec::new());
static WATCH_STARTED: Once = Once::new();

/// Returns a channel that yields a message whenever drives change.
///
/// The channel is bounded to one slot and written with `try_send`, so a burst
/// of hotplug events collapses into a single pending refresh instead of a
/// backlog the UI has to chew through.
pub fn subscribe_changes() -> async_channel::Receiver<()> {
    let (tx, rx) = async_channel::bounded(1);
    if let Ok(mut watchers) = WATCHERS.lock() {
        watchers.push(tx);
    }
    WATCH_STARTED.call_once(|| {
        if let Err(err) = start_watch_thread() {
            eprintln!("cadrocfile: drive hotplug detection unavailable: {err}");
        }
    });
    rx
}

fn broadcast() {
    let Ok(mut watchers) = WATCHERS.lock() else { return };
    // Drop channels whose window has closed, then nudge the rest.
    watchers.retain(|tx| !tx.is_closed());
    for tx in watchers.iter() {
        let _ = tx.try_send(());
    }
}

/// Spawns the thread that turns UDisks2 signals into [`broadcast`] calls.
fn start_watch_thread() -> Result<(), String> {
    let conn = connection()?;

    std::thread::Builder::new()
        .name("cadrocfile-udisks-watch".into())
        .spawn(move || {
            let manager = zbus::blocking::fdo::ObjectManagerProxy::builder(&conn)
                .destination(UDISKS_SERVICE)
                .and_then(|b| b.path("/org/freedesktop/UDisks2"))
                .and_then(|b| b.build());
            let Ok(manager) = manager else { return };

            let added = manager.receive_interfaces_added().ok();
            let removed = manager.receive_interfaces_removed().ok();

            // Mounting does not add or remove an interface, it only changes
            // Filesystem.MountPoints — so property changes have to be watched
            // as well, or a drive mounted from another app never shows up.
            let properties = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.freedesktop.DBus.Properties")
                .and_then(|b| b.member("PropertiesChanged"))
                .and_then(|b| b.sender(UDISKS_SERVICE))
                .map(|b| b.build())
                .ok()
                .and_then(|rule| {
                    zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(16)).ok()
                });

            std::thread::scope(|scope| {
                if let Some(stream) = added {
                    scope.spawn(move || {
                        for _ in stream {
                            broadcast();
                        }
                    });
                }
                if let Some(stream) = removed {
                    scope.spawn(move || {
                        for _ in stream {
                            broadcast();
                        }
                    });
                }
                if let Some(stream) = properties {
                    scope.spawn(move || {
                        for _ in stream {
                            broadcast();
                        }
                    });
                }
            });
        })
        .map_err(|e| e.to_string())?;

    Ok(())
}



#[cfg(test)]
mod error_tests {
    use super::*;

    fn ntfs_volume() -> Volume {
        Volume {
            object_path: "/org/freedesktop/UDisks2/block_devices/sda1".into(),
            drive_path: None,
            device: PathBuf::from("/dev/sda1"),
            label: "SSD-Store".into(),
            uuid: "1234".into(),
            fstype: "ntfs".into(),
            size: 0,
            mount_point: None,
            stale_mount: None,
            read_only: false,
            category: VolumeCategory::Removable,
            drive_name: String::new(),
            ejectable: true,
            can_power_off: true,
            is_encrypted: false,
        }
    }

    /// The generic kernel refusal really is a dirty volume, and every option
    /// the recovery dialog offers applies to it.
    #[test]
    fn a_kernel_refusal_on_ntfs_is_treated_as_a_dirty_volume() {
        let raw = "Error mounting /dev/sda1: wrong fs type, bad option, bad superblock \
                   on /dev/sda1, missing codepage or helper program, or other error";
        assert!(matches!(
            classify_mount_error(raw, &ntfs_volume()),
            MountError::NtfsUnclean { hibernated: false, .. }
        ));
    }

    /// A rejected *option* is not a dirty volume, and must not open a repair
    /// dialog that cannot possibly help.
    #[test]
    fn a_rejected_option_is_not_mistaken_for_a_dirty_volume() {
        let raw = "GDBus.Error:org.freedesktop.UDisks2.Error.OptionNotPermitted: Requested \
                   filesystem type `ntfs-3g' is neither well-known nor in /proc/filesystems \
                   nor in /etc/filesystems";
        match classify_mount_error(raw, &ntfs_volume()) {
            MountError::Other(message) => {
                assert!(message.contains("bug in Cadrocfile"), "{message}");
            }
            other => panic!("expected a plain error, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod mount_order_tests {
    use super::mount_order;

    /// The attempt order is the whole point: getting it right first time is
    /// what stops a failed UDisks2 job appearing, which other desktop
    /// components report as an error even when the retry succeeds.
    #[test]
    fn a_remembered_volume_tries_the_fuse_driver_first() {
        assert_eq!(mount_order(true, true), ["ntfs", ""], "remembered NTFS goes to FUSE first");
        assert_eq!(mount_order(true, false), ["", "ntfs"], "unknown NTFS tries the kernel first");
        assert_eq!(mount_order(false, true), ["", "ntfs"], "non-NTFS never prefers FUSE");
        assert_eq!(mount_order(false, false), ["", "ntfs"]);
    }

    /// Both drivers are always reachable, whichever way round they are tried —
    /// a volume must never be left with only one option.
    #[test]
    fn every_order_still_offers_both_drivers() {
        for is_ntfs in [true, false] {
            for prefer in [true, false] {
                let order = mount_order(is_ntfs, prefer);
                assert_eq!(order.len(), 2);
                assert!(order.contains(&""), "the kernel driver must stay reachable");
                assert!(order.contains(&"ntfs"), "the FUSE driver must stay reachable");
            }
        }
    }

    /// Pins the spelling, because the wrong one is the plausible-looking one.
    ///
    /// Naming the helper binary rather than the filesystem makes UDisks2 reject
    /// the request outright with `OptionNotPermitted`, which meant every dirty
    /// NTFS volume failed both attempts and had to be mounted from a terminal.
    /// The mistake is invisible without a real drive, so it is fixed here.
    #[test]
    fn the_fallback_names_a_filesystem_udisks_will_accept() {
        for is_ntfs in [true, false] {
            for prefer in [true, false] {
                assert!(
                    !mount_order(is_ntfs, prefer).contains(&"ntfs-3g"),
                    "`ntfs-3g` is a mount helper, not a filesystem UDisks2 accepts",
                );
            }
        }
    }
}

