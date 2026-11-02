//! Phones, cameras and other portable devices exposed by gvfs.
//!
//! MTP phones are not block devices, so UDisks2 never reports them — they show
//! up as `GVolume`s from gvfs's MTP backend, with an `mtp://` activation root.
//! After mounting, gvfs bridges the device into `/run/user/<uid>/gvfs` just
//! like a network share, so browsing, copying and archiving work on ordinary
//! paths.
//!
//! Each device family lives in its own gvfs package (`gvfs-mtp`, `gvfs-afc`,
//! `gvfs-gphoto2`). A phone can therefore be plugged in and still be invisible
//! to the whole desktop; the README and installer name what to install when
//! that happens.
use std::path::PathBuf;

use gtk::{gio, prelude::*};

/// A class of portable device with its own gvfs backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Android phones and other MTP players.
    Mtp,
    /// iPhones and iPads through libimobiledevice.
    Afc,
    /// Cameras in PTP mode.
    Gphoto2,
}

impl Kind {
    fn from_scheme(scheme: &str) -> Option<Self> {
        match scheme.to_ascii_lowercase().as_str() {
            "mtp" => Some(Kind::Mtp),
            "afc" => Some(Kind::Afc),
            "gphoto2" => Some(Kind::Gphoto2),
            _ => None,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Kind::Mtp | Kind::Afc => "phone-symbolic",
            Kind::Gphoto2 => "camera-photo-symbolic",
        }
    }

    /// What to do when the device is plugged in but the mount fails.
    pub fn hint(self) -> &'static str {
        match self {
            Kind::Mtp => {
                "Unlock the phone and choose “File transfer” (or “MTP”) in its USB \
                 notification, then try again."
            }
            Kind::Afc => {
                "Unlock the phone and tap “Trust” when it asks whether to trust this \
                 computer, then try again."
            }
            Kind::Gphoto2 => {
                "Set the camera to PTP mode — sometimes called “PC control” or “MTP” — \
                 then try again."
            }
        }
    }
}

/// A phone, camera or media player that gvfs can mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portable {
    pub name: String,
    /// The gvfs URI, e.g. `mtp://motorola_moto_g34_5G_ZF524XS49X/`.
    pub uri: String,
    /// Where gvfs bridged it into the filesystem, when it is mounted.
    pub path: Option<PathBuf>,
    pub kind: Kind,
}

impl Portable {
    pub fn icon(&self) -> &'static str {
        self.kind.icon()
    }

    pub fn hint(&self) -> &'static str {
        self.kind.hint()
    }

    pub fn is_connected(&self) -> bool {
        self.path.is_some()
    }
}

/// Every portable device gvfs knows about, mounted or not.
///
/// Mounted devices are included too: the row's state changes, but it must not
/// disappear and reappear as it is connected, which would move every row below
/// it while the user is aiming at one.
pub fn list() -> Vec<Portable> {
    let monitor = gio::VolumeMonitor::get();
    let mounts = monitor.mounts();

    let mut devices: Vec<Portable> = monitor
        .volumes()
        .into_iter()
        .filter_map(|volume| {
            let root = volume.activation_root()?;
            let uri = root.uri().to_string();
            let kind = root.uri_scheme().and_then(|s| Kind::from_scheme(&s))?;
            let path = mounts
                .iter()
                .find(|mount| mount.root().uri() == uri)
                .and_then(|mount| mount.root().path());

            Some(Portable {
                name: volume.name().to_string(),
                uri,
                path,
                kind,
            })
        })
        .collect();

    // A mount whose volume vanished from the monitor — which the MTP proxy
    // does on some gvfs versions — is still usable, so fall back to the mount
    // itself rather than losing the device from the sidebar.
    for mount in &mounts {
        let root = mount.root();
        let uri = root.uri().to_string();
        let Some(kind) = root.uri_scheme().and_then(|s| Kind::from_scheme(&s)) else {
            continue;
        };
        if devices.iter().any(|device| device.uri == uri) {
            continue;
        }
        devices.push(Portable {
            name: mount.name().to_string(),
            uri,
            path: root.path(),
            kind,
        });
    }

    devices.sort_by_key(|device| device.name.to_lowercase());
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_device_schemes_are_treated_as_portable() {
        assert_eq!(Kind::from_scheme("mtp"), Some(Kind::Mtp));
        assert_eq!(Kind::from_scheme("MTP"), Some(Kind::Mtp));
        assert_eq!(Kind::from_scheme("afc"), Some(Kind::Afc));
        assert_eq!(Kind::from_scheme("gphoto2"), Some(Kind::Gphoto2));
        assert_eq!(Kind::from_scheme("smb"), None);
        assert_eq!(Kind::from_scheme("file"), None);
    }

    /// A phone must never be mistaken for a network share, and vice versa:
    /// both paths are ordinary directories once mounted, so only the URI tells
    /// them apart.
    #[test]
    fn each_device_family_has_its_own_icon_and_advice() {
        for kind in [Kind::Mtp, Kind::Afc, Kind::Gphoto2] {
            assert!(!kind.icon().is_empty());
            assert!(!kind.hint().is_empty());
        }
        assert_eq!(Kind::Mtp.icon(), "phone-symbolic");
        assert_eq!(Kind::Gphoto2.icon(), "camera-photo-symbolic");
    }
}
