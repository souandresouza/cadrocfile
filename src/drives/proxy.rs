//! Hand-written zbus proxies for the UDisks2 interfaces this app calls.
//!
//! Only the methods actually used are declared: properties are read in bulk
//! from `GetManagedObjects` instead, which avoids a round-trip per property per
//! device when the sidebar refreshes.

use std::collections::HashMap;

use zbus::zvariant::Value;

#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Filesystem",
    default_service = "org.freedesktop.UDisks2"
)]
pub trait Filesystem {
    /// Returns the resulting mount point.
    fn mount(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<String>;

    fn unmount(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Drive",
    default_service = "org.freedesktop.UDisks2"
)]
pub trait Drive {
    fn eject(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;

    /// Cuts power to the drive so it can be physically removed.
    fn power_off(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Encrypted",
    default_service = "org.freedesktop.UDisks2"
)]
pub trait Encrypted {
    /// Opens the container and returns the object path of the cleartext device.
    ///
    /// This is the only method declared from this interface, deliberately.
    /// `Encrypted` also carries `Format` and `ChangePassphrase`, which rewrite
    /// the LUKS header and would destroy the volume if called by mistake; not
    /// declaring them means they cannot be called at all. `Unlock` itself only
    /// establishes a device-mapper mapping — it never writes to the disk.
    fn unlock(
        &self,
        passphrase: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    /// Tears the mapping down again. Also non-destructive.
    fn lock(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}
