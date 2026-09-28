//! Network storage: connecting to a server, and what is connected now.
//!
//! Everything here goes through gvfs, the same daemon the rest of the desktop
//! uses. That matters for one specific reason: gvfs bridges its mounts into the
//! filesystem under `/run/user/<uid>/gvfs`, so a share that gvfs has mounted is
//! an ordinary directory with an ordinary path. A file manager built on paths
//! gets remote browsing, copying and archiving for free, and every other
//! application on the machine sees the same mount.
//!
//! The catch is that each protocol lives in a separate gvfs package, so SMB can
//! be missing on a machine where SFTP works perfectly. [`Scheme::is_available`]
//! checks for the backend rather than discovering it through a failed mount,
//! which is the difference between "install gvfs-smb" and "Operation not
//! supported".

use std::path::PathBuf;

use gtk::{gio, prelude::*};
use serde::{Deserialize, Serialize};

/// Protocols worth offering. Each maps to one gvfs backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scheme {
    Smb,
    Sftp,
    Ftp,
    Ftps,
    Dav,
    Davs,
    Nfs,
    Afp,
}

impl Scheme {
    pub const ALL: [Scheme; 8] = [
        Scheme::Smb,
        Scheme::Sftp,
        Scheme::Ftps,
        Scheme::Ftp,
        Scheme::Davs,
        Scheme::Dav,
        Scheme::Nfs,
        Scheme::Afp,
    ];

    /// The URI scheme, which is also the gvfs backend name.
    pub fn uri_scheme(self) -> &'static str {
        match self {
            Scheme::Smb => "smb",
            Scheme::Sftp => "sftp",
            Scheme::Ftp => "ftp",
            Scheme::Ftps => "ftps",
            Scheme::Dav => "dav",
            Scheme::Davs => "davs",
            Scheme::Nfs => "nfs",
            Scheme::Afp => "afp",
        }
    }

    /// How to name it to somebody who is not thinking in URI schemes.
    pub fn label(self) -> &'static str {
        match self {
            Scheme::Smb => "Windows share (SMB)",
            Scheme::Sftp => "SSH (SFTP)",
            Scheme::Ftp => "FTP",
            Scheme::Ftps => "FTP over TLS",
            Scheme::Dav => "WebDAV",
            Scheme::Davs => "WebDAV over HTTPS",
            Scheme::Nfs => "NFS",
            Scheme::Afp => "Apple filing (AFP)",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Smb => 445,
            Scheme::Sftp => 22,
            Scheme::Ftp | Scheme::Ftps => 21,
            Scheme::Dav => 80,
            Scheme::Davs => 443,
            Scheme::Nfs => 2049,
            Scheme::Afp => 548,
        }
    }

    /// Whether a share name is part of the address.
    ///
    /// SMB addresses a share, not a directory, so `smb://host` alone is a
    /// browse request rather than a mount; the others take a path.
    pub fn needs_share(self) -> bool {
        matches!(self, Scheme::Smb | Scheme::Afp | Scheme::Nfs)
    }

    /// Where gvfs keeps its backend descriptions. A backend that isn't
    /// installed has no `.mount` file and cannot be used, however valid the URI.
    fn mount_file(self) -> PathBuf {
        // One file per scheme, named after it: several schemes can share an
        // executable — `ftp.mount` and `ftps.mount` both run `gvfsd-ftp` — but
        // each still gets its own declaration, so the scheme name is the right
        // thing to look for.
        PathBuf::from("/usr/share/gvfs/mounts").join(format!("{}.mount", self.uri_scheme()))
    }

    pub fn is_available(self) -> bool {
        self.mount_file().exists()
    }

    /// What to install to make this protocol work, named per distribution
    /// family because the package is spelled differently in each.
    pub fn package_hint(self) -> &'static str {
        match self {
            Scheme::Smb => "gvfs-smb (Arch), gvfs-backends (Debian/Ubuntu), gvfs-smb (Fedora)",
            Scheme::Nfs => "gvfs-nfs (Arch), gvfs-backends (Debian/Ubuntu), gvfs-nfs (Fedora)",
            Scheme::Afp => "gvfs-afp (Arch), gvfs-backends (Debian/Ubuntu), gvfs-afp (Fedora)",
            Scheme::Dav | Scheme::Davs => {
                "gvfs-dav (Arch), gvfs-backends (Debian/Ubuntu), gvfs-fuse (Fedora)"
            }
            _ => "gvfs (Arch), gvfs-backends (Debian/Ubuntu), gvfs (Fedora)",
        }
    }
}

/// A server address the user can save and come back to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub scheme: Scheme,
    pub host: String,
    /// `None` means the protocol's default, which is what should be sent —
    /// pinning 445 into every SMB URI is noise the user did not ask for.
    pub port: Option<u16>,
    /// Share name for SMB, path for everything else. May be empty.
    pub path: String,
    pub user: Option<String>,
    /// What to call it in the sidebar. Defaults to the host.
    pub label: Option<String>,
}

impl Server {
    /// The gvfs URI for this address.
    ///
    /// The username goes in the authority rather than being left for the
    /// password prompt, because gvfs keys its saved credentials on the full URI
    /// and a URI without a user gets a fresh prompt every session.
    pub fn uri(&self) -> String {
        let mut uri = format!("{}://", self.scheme.uri_scheme());
        if let Some(user) = self.user.as_deref().filter(|u| !u.is_empty()) {
            uri.push_str(&urlencoding::encode(user));
            uri.push('@');
        }
        uri.push_str(self.host.trim());
        if let Some(port) = self.port.filter(|p| *p != self.scheme.default_port()) {
            uri.push_str(&format!(":{port}"));
        }
        let path = self.path.trim().trim_start_matches('/');
        if !path.is_empty() {
            uri.push('/');
            uri.push_str(path);
        }
        uri
    }

    pub fn display_name(&self) -> String {
        if let Some(label) = self.label.as_deref().filter(|l| !l.trim().is_empty()) {
            return label.to_string();
        }
        let path = self.path.trim().trim_matches('/');
        if path.is_empty() {
            self.host.clone()
        } else {
            format!("{} on {}", path, self.host)
        }
    }

    pub fn icon(&self) -> &'static str {
        match self.scheme {
            Scheme::Smb | Scheme::Afp => "folder-remote-symbolic",
            Scheme::Sftp => "network-server-symbolic",
            _ => "network-workgroup-symbolic",
        }
    }
}

/// Reads an address the user typed, in either URI or `host/share` form.
///
/// People paste `smb://nas/media`, type `nas/media`, and copy
/// `\\NAS\media` out of Windows. All three mean the same thing and all three
/// are accepted, because rejecting two of them teaches nothing.
pub fn parse_address(input: &str, fallback: Scheme) -> Result<Server, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a server address".into());
    }

    // Windows UNC: \\host\share
    let normalised = if trimmed.starts_with("\\\\") {
        format!("smb://{}", trimmed.trim_start_matches('\\').replace('\\', "/"))
    } else {
        trimmed.to_string()
    };

    let (scheme, rest) = match normalised.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = Scheme::ALL
                .iter()
                .copied()
                .find(|s| s.uri_scheme() == scheme.to_ascii_lowercase())
                .ok_or_else(|| format!("{scheme}:// is not a protocol Cadrocfile can mount"))?;
            (scheme, rest)
        }
        None => (fallback, normalised.as_str()),
    };

    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (rest, ""),
    };

    let (user, hostport) = match authority.rsplit_once('@') {
        Some((user, hostport)) => (
            Some(urlencoding::decode(user).map(|u| u.into_owned()).unwrap_or_else(|_| user.into())),
            hostport,
        ),
        None => (None, authority),
    };

    // A bracketed IPv6 literal contains colons that are not a port separator.
    let (host, port) = if hostport.starts_with('[') {
        match hostport.split_once("]:") {
            Some((host, port)) => (format!("{host}]"), port.parse::<u16>().ok()),
            None => (hostport.to_string(), None),
        }
    } else {
        match hostport.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
                (host.to_string(), port.parse::<u16>().ok())
            }
            _ => (hostport.to_string(), None),
        }
    };

    if host.is_empty() {
        return Err("That address has no server name in it".into());
    }
    if scheme.needs_share() && path.trim_matches('/').is_empty() {
        return Err(format!(
            "{} needs a share name as well, like {}://{host}/media",
            scheme.label(),
            scheme.uri_scheme()
        ));
    }

    Ok(Server {
        scheme,
        host,
        port,
        path: path.trim_matches('/').to_string(),
        user: user.filter(|u| !u.is_empty()),
        label: None,
    })
}

/// A share gvfs currently has mounted.
#[derive(Debug, Clone)]
pub struct Mounted {
    pub name: String,
    pub uri: String,
    /// Where it appears in the filesystem. `None` when gvfs is running without
    /// its FUSE bridge, in which case the share exists but no path reaches it.
    pub path: Option<PathBuf>,
    pub icon: String,
}

/// Every network share mounted in this session.
///
/// Local disks are filtered out: they arrive through [`crate::drives`], which
/// knows about capacity and ejection, and listing them twice would be worse
/// than not listing them here at all.
pub fn mounted() -> Vec<Mounted> {
    let monitor = gio::VolumeMonitor::get();
    let mut shares: Vec<Mounted> = monitor
        .mounts()
        .into_iter()
        .filter_map(|mount| {
            let root = mount.root();
            let uri = root.uri().to_string();
            let scheme = root.uri_scheme().map(|s| s.to_string()).unwrap_or_default();
            if !is_network_scheme(&scheme) {
                return None;
            }
            Some(Mounted {
                name: mount.name().to_string(),
                uri,
                path: root.path(),
                icon: "folder-remote-symbolic".to_string(),
            })
        })
        .collect();
    shares.sort_by_key(|a| a.name.to_lowercase());
    shares
}

fn is_network_scheme(scheme: &str) -> bool {
    matches!(
        scheme,
        "smb" | "sftp" | "ssh" | "ftp" | "ftps" | "dav" | "davs" | "nfs" | "afp" | "google-drive"
    )
}

/// Mounts `uri`, prompting for credentials through `operation`.
///
/// The `GMountOperation` is the whole reason this goes through GIO rather than
/// calling `mount.cifs`: it is what turns "authentication required" into a
/// dialog, remembers the answer in the keyring when asked to, and handles the
/// re-ask on a wrong password. Doing that by hand would mean reimplementing the
/// desktop's credential handling badly.
pub async fn mount(uri: &str, operation: &gio::MountOperation) -> Result<PathBuf, String> {
    let file = gio::File::for_uri(uri);

    match file
        .mount_enclosing_volume_future(gio::MountMountFlags::NONE, Some(operation))
        .await
    {
        Ok(()) => {}
        Err(e) if is_already_mounted(&e) => {}
        Err(e) => return Err(mount_error(uri, &e)),
    }

    // The mount succeeded, but this app browses paths, so a mount with no path
    // is not usable and saying nothing about it would look like a hang.
    file.path().ok_or_else(|| {
        format!(
            "{uri} is mounted, but it has no path on this system. That means gvfs is \
             running without its FUSE bridge — installing gvfs-fuse fixes it."
        )
    })
}

fn is_already_mounted(error: &glib::Error) -> bool {
    error.matches(gio::IOErrorEnum::AlreadyMounted)
}

/// gvfs reports the interesting failures through error domains rather than
/// messages, so they can be answered specifically instead of echoed.
fn mount_error(uri: &str, error: &glib::Error) -> String {
    if error.matches(gio::IOErrorEnum::Cancelled) {
        return "Connection cancelled.".to_string();
    }
    if error.matches(gio::IOErrorEnum::NotSupported) {
        let scheme = uri.split("://").next().unwrap_or("that protocol");
        return format!(
            "No gvfs backend is installed for {scheme}://, so this address cannot be mounted."
        );
    }
    if error.matches(gio::IOErrorEnum::PermissionDenied) {
        return "The server rejected those credentials.".to_string();
    }
    if error.matches(gio::IOErrorEnum::HostNotFound) || error.matches(gio::IOErrorEnum::HostUnreachable)
    {
        return "That server could not be reached. Check the name and that you are on the \
                same network."
            .to_string();
    }
    if error.matches(gio::IOErrorEnum::TimedOut) {
        return "The server did not answer in time.".to_string();
    }
    error.message().to_string()
}

/// Disconnects a share. Never forced: a forced unmount discards writes that
/// have not reached the server yet, and losing a file is worse than a busy
/// error the user can act on.
pub async fn unmount(uri: &str) -> Result<(), String> {
    let file = gio::File::for_uri(uri);
    // Finding the mount is a local D-Bus lookup against the gvfs daemon, so it
    // is done synchronously; only the unmount itself can take real time.
    let mount = file
        .find_enclosing_mount(gio::Cancellable::NONE)
        .map_err(|_| "That share is not mounted any more.".to_string())?;

    mount
        .unmount_with_operation_future(gio::MountUnmountFlags::NONE, gio::MountOperation::NONE)
        .await
        .map_err(|e| {
            if e.matches(gio::IOErrorEnum::Busy) {
                "Something is still using that share. Close it and try again.".to_string()
            } else {
                e.message().to_string()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_accepted_in_every_form_people_actually_have_them() {
        // A full URI.
        let s = parse_address("smb://nas.local/media", Scheme::Smb).unwrap();
        assert_eq!(s.host, "nas.local");
        assert_eq!(s.path, "media");
        assert_eq!(s.scheme, Scheme::Smb);

        // Bare, using whatever the dialog has selected.
        let s = parse_address("nas.local/media", Scheme::Smb).unwrap();
        assert_eq!((s.host.as_str(), s.path.as_str()), ("nas.local", "media"));

        // Copied out of Windows.
        let s = parse_address(r"\\NAS\media", Scheme::Sftp).unwrap();
        assert_eq!(s.scheme, Scheme::Smb, "a UNC path is always SMB");
        assert_eq!((s.host.as_str(), s.path.as_str()), ("NAS", "media"));

        // User and port in the authority.
        let s = parse_address("sftp://ana@box.dev:2222/srv/data", Scheme::Sftp).unwrap();
        assert_eq!(s.user.as_deref(), Some("ana"));
        assert_eq!(s.port, Some(2222));
        assert_eq!(s.path, "srv/data");
    }

    #[test]
    fn an_ipv6_literal_is_not_split_on_its_colons() {
        let s = parse_address("sftp://[fe80::1]:2222/data", Scheme::Sftp).unwrap();
        assert_eq!(s.host, "[fe80::1]");
        assert_eq!(s.port, Some(2222));

        let s = parse_address("sftp://[fe80::1]/data", Scheme::Sftp).unwrap();
        assert_eq!(s.host, "[fe80::1]");
        assert_eq!(s.port, None);
    }

    #[test]
    fn addresses_that_cannot_work_are_refused_with_a_reason() {
        assert!(parse_address("", Scheme::Smb).is_err());
        assert!(parse_address("http://x.dev/f", Scheme::Smb).is_err(), "not a mountable protocol");
        // SMB mounts a share, so a bare host is an incomplete address.
        let err = parse_address("smb://nas", Scheme::Smb).unwrap_err();
        assert!(err.contains("share"), "{err}");
        // SFTP mounts a path, so a bare host is fine.
        assert!(parse_address("sftp://box.dev", Scheme::Sftp).is_ok());
    }

    #[test]
    fn a_uri_round_trips_through_parsing_unchanged() {
        for original in [
            "smb://nas.local/media",
            "sftp://ana@box.dev/srv/data",
            "sftp://ana@box.dev:2222/srv/data",
            "davs://files.example.org/remote.php/dav",
        ] {
            let parsed = parse_address(original, Scheme::Smb).unwrap();
            assert_eq!(parsed.uri(), original, "{original} did not survive a round trip");
        }
    }

    /// A default port in the URI is noise, and gvfs keys saved credentials on
    /// the exact URI string, so it must be spelled the same way every time.
    #[test]
    fn the_default_port_is_left_out_of_the_uri() {
        let mut server = parse_address("sftp://box.dev/data", Scheme::Sftp).unwrap();
        server.port = Some(22);
        assert_eq!(server.uri(), "sftp://box.dev/data");
        server.port = Some(2222);
        assert_eq!(server.uri(), "sftp://box.dev:2222/data");
    }

    /// gvfs declares one `.mount` file per scheme, so availability is a file
    /// existence check — and it has to look for the right filename.
    #[test]
    fn backend_lookup_uses_the_scheme_name() {
        for scheme in Scheme::ALL {
            let file = scheme.mount_file();
            assert_eq!(
                file.file_name().unwrap().to_string_lossy(),
                format!("{}.mount", scheme.uri_scheme()),
            );
            assert!(file.starts_with("/usr/share/gvfs/mounts"));
        }
    }

    #[test]
    fn a_name_is_produced_even_without_a_label() {
        let mut server = parse_address("smb://nas.local/media", Scheme::Smb).unwrap();
        assert_eq!(server.display_name(), "media on nas.local");
        server.label = Some("Living room NAS".into());
        assert_eq!(server.display_name(), "Living room NAS");
        // A blank label is not a label.
        server.label = Some("   ".into());
        assert_eq!(server.display_name(), "media on nas.local");
    }
}
