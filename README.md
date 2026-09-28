<div align="center">

<img src="docs/icon.png" width="120" alt="Cadrocfile">

# Cadrocfile

**A fast GTK4 file manager for Linux, written in Rust.**

Browse, copy, move, rename — plus the things that usually send you to a
terminal: NTFS drives Windows left dirty, LUKS volumes, any archive format,
and downloads.

</div>

<table>
<tr>
<td align="center">

![Cadrocfile dark mode](docs/screenshot_dark_mode.png)

Dark mode
</td>
<td align="center">

![Cadrocfile light mode](docs/screenshot_light_mode.png)

Light mode
</td>
</tr>
</table>

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/0znio/cadrocfile/main/install.sh | sh
```

Works out what your machine needs: installs dependencies with your own package
manager (apt, pacman, dnf, zypper, apk, xbps, emerge, eopkg), then downloads the
prebuilt binary — or builds from source if a binary won't run here.

> **Prebuilt binary needs** glibc ≥ 2.39, GTK ≥ 4.12, libadwaita ≥ 1.5 —
> Ubuntu 24.04+, Debian 13+, Fedora 40+, Arch, openSUSE Tumbleweed.
> Older, musl, or non-x86_64 builds from source instead.

The installer also pulls the optional pieces each feature needs: `gvfs` and
`gvfs-smb` for network shares, `rclone` and `fuse3` for cloud drives, `ntfs-3g`
for NTFS repair, and `pigz` for fast `.tar.gz`. Everything else still works
without them, and the app says which one is missing rather than failing quietly.

Options go after `-s --`, since the script is piped into `sh`:

```sh
curl -fsSL .../install.sh | sh -s -- --from-source --prefix /usr/local
```

| Option | Effect |
| --- | --- |
| `--from-source` | Build with cargo instead of downloading |
| `--prefix DIR` | Install under `DIR` (default `/usr/local`, or `~/.local` without root) |
| `--version TAG` | A specific release |
| `--no-deps` | Leave the package manager alone |
| `--uninstall` | Remove it again |

<details>
<summary><b>From source, or manual download</b></summary>

Needs Rust 1.92+ and dev headers for `gtk4`, `libadwaita`, `libarchive`.

```sh
git clone https://github.com/0znio/cadrocfile.git && cd cadrocfile
make && make test
make install                      # ~/.local, no root
sudo make install PREFIX=/usr/local   # system-wide
```

Or grab the tarball from the [latest release](https://github.com/0znio/cadrocfile/releases/latest):

```sh
curl -fsSLO https://github.com/0znio/cadrocfile/releases/latest/download/SHA256SUMS
curl -fsSLO https://github.com/0znio/cadrocfile/releases/latest/download/cadrocfile-0.4.1-x86_64-linux.tar.gz
sha256sum -c SHA256SUMS
tar -xzf cadrocfile-*-x86_64-linux.tar.gz && cd cadrocfile-*-x86_64-linux
install -Dm755 cadrocfile ~/.local/bin/cadrocfile
```

Runtime needs `gtk4`, `libadwaita`, `libarchive`, `udisks2` and a polkit agent.
Optional: `pigz` (threaded `.tar.gz`), `ntfs-3g` (dirty NTFS), `ntfsprogs`
(`ntfsfix`, repairs them properly).

</details>

## What it does

**Browsing** — tabs with their own history and scroll position · icon grid and
details list · breadcrumb that becomes an editable path · drag and drop ·
light/dark and a pick-your-own accent colour.

**Preview** — press `Space` on a file to see it without opening anything:
images, PDFs and office documents, text and code, and video or audio playing in
place. Arrow keys step through the folder; `Enter` opens the file.

**Thumbnails** — photos, video, PDF and office documents, the right way up. Uses
whatever thumbnailers your system has registered, falls back to `ffmpeg` and
`pdftoppm`, and shares everything through the freedesktop cache.

**Search** — by name, or by what's inside files with the **Contents** toggle.
Both walk the tree in the background and stream results as they're found.
Content search skips binaries and reads on several threads.

**Rename many at once** — select several and press `F2`: find and replace,
number them, or change case, with every new name previewed and clashes flagged
before anything moves. Swaps and renumbered series are handled safely.

**Disk usage** — which folders are eating the drive, largest first, with a bar
for each one's share. Click through to drill down. Measures space actually used,
stays on one disk, and counts hard links once — the way `du -x` does.

**Drives** — every mountable volume in the sidebar with a capacity ring, grouped
into Removable, Windows and internal. Mounts through UDisks2 as your own user:
no `sudo`, no fstab, no `/dev/sdX` that breaks on replug. LUKS volumes prompt for
a passphrase and unlock.

**NTFS that Windows left dirty** — with Fast Startup on, that's *every* normal
shutdown. Most file managers show you the driver error and stop. Cadrocfile offers
read-only, a proper repair, or a forced mount, and remembers which driver worked.

**Archives** — extract zip, 7z, rar/rar5, tar with any compressor, cab, iso, deb,
rpm and more, in-process through libarchive with real progress and cancel.
Create zip, tar.gz, tar.xz, tar.zst, 7z.

**Downloads** — `Ctrl+Shift+D` takes a URL, asks the server for the real name and
size, lets you rename and pick a folder, then fetches it across several
connections. Share links from Google Drive, pixeldrain, GitHub, Dropbox and
gofile are rewritten to the file they point at, and a reply that turns out to be
a login page or an ISP block is reported instead of being saved as your `.zip`.

**Network shares** — connect to SMB, SFTP, FTP, WebDAV or NFS from the sidebar.
Saved servers stay listed whether or not they're mounted; passwords go to your
login keyring through gvfs, never to Cadrocfile.

**Cloud drives** — Google Drive, Proton Drive, Icedrive, Dropbox, OneDrive and
Nextcloud, mounted as ordinary folders so copy, search and compress all work on
them. Each account is labelled with the name you give it, so several accounts of
one provider stay apart. Needs [rclone](https://rclone.org), which holds the
credentials — Cadrocfile never sees them.

**Deleting** — Trash by default and undoable. `Shift+Delete` is permanent;
`Ctrl+Shift+Delete` shreds, and tells you when your filesystem makes that
promise meaningless.

**Internationalization** — uses gettext for translations. Currently supports
pt-BR. Add more languages by creating `po/<lang>.po` files.

## Fast

Measured on a 446 MB / 36,700-file corpus, ext4 on NVMe, 8 threads.

| | Before | After |
| --- | --- | --- |
| Compress `.tar.xz` | 77.7 s | **16.6 s** |
| Compress `.tar.gz` | 6.29 s | **1.03 s** |
| Extract `.tar.xz` | 2.57 s | **1.70 s** |
| Shred 2000 × 8 KB | 9.31 s | **1.79 s** |
| List an 8,463-file folder | 613 ms | **62 ms** |
| Download 140 MB | 34.3 s | **9.1 s** |

Jobs use half your cores by default, so a long compress doesn't make the desktop
stutter. Settings → Performance overrides it.

<details>
<summary><b>Why</b></summary>

- **libarchive compresses on one core** unless told otherwise. liblzma and
  libzstd split input into independent blocks, so they just needed a thread
  count. Deflate has no threaded encoder, so `.tar.gz` pipes through `pigz`.
- **Extraction read every archive twice** — once to list, once to extract, and
  listing a `.tar.gz` means decompressing all of it. Entries now land in a
  staging directory and are moved into place with one `rename`.
- **Shredding is bound by `fsync` latency, not bandwidth.** Files shred on
  several threads that sit blocked in `fsync` costing almost no CPU, while the
  device coalesces their commits.
- **Listing** asked gio for `standard::content-type`, which costs a mime lookup
  per file and an open-and-read for anything it can't name from the filename.
  Cadrocfile derives it itself, memoised on the name's suffix.
- **Downloads** split across connections when the server sends `Accept-Ranges`;
  byte-identical output, falling back to one stream when it won't.
- **Deleting was left alone deliberately** — parallel `unlink` measured
  completely flat on ext4, because the journal serialises it.

`CADROCFILE_TRACE=1` reports scan times and main-loop stalls.
`CADROCFILE_SNAPSHOT=<path>` renders the window to a PNG and exits.

</details>

## Shortcuts

`Ctrl+?` lists them all in the app.

| Key | Action |
| --- | --- |
| `Ctrl+T` / `Ctrl+W` / `Ctrl+Tab` | New tab / close / cycle |
| `Alt+←` `Alt+→` `Alt+↑` | Back / forward / up |
| `Ctrl+L` / `Ctrl+F` / `Ctrl+H` | Edit path / search / hidden files |
| `Space` | Preview (arrows step through, `Enter` opens) |
| `F2` · `Delete` · `Shift+Delete` · `Ctrl+Shift+Delete` | Rename (several at once too) · trash · delete · shred |
| `Ctrl+E` / `Ctrl+Shift+E` | Extract here / compress |
| `Ctrl+Shift+D` | Download from a URL |
| `Ctrl+D` / `Ctrl+Shift+V` / `Alt+Return` / `F9` | Favourite / grid↔list / properties / sidebar |

Selection keys act on the file list, so text boxes keep their own.

<details>
<summary><b>How dirty NTFS is handled</b></summary>

| Option | What it does | Risk |
| --- | --- | --- |
| **Repair and mount** (default) | `ntfsfix -d` clears the dirty flag and schedules Windows' chkdsk | Low; needs admin |
| **Open read-only** | Browse and copy files off | None |
| **Force read-write** | Mounts with `remove_hiberfile`, discarding the hibernation image | Files untouched, but a suspended Windows session can't resume |

The in-kernel `ntfs3` driver rejects dirty volumes, and it's what you get by
default — `blkid` reports them as `ntfs` and the kernel resolves that name
straight to `ntfs3`. So retrying with `fstype=ntfs` changes nothing; Cadrocfile asks
for `ntfs-3g`, which UDisks2 routes through the FUSE helper instead.

Which driver worked is remembered per volume UUID. Every mount attempt is a
UDisks2 job that other desktop components watch — `udiskie` reports a failed
first attempt even when the retry succeeds — so getting it right first time is
the only way to keep that quiet.

</details>

<details>
<summary><b>What shredding can and can't promise</b></summary>

Overwriting before unlinking works on a filesystem that overwrites in place
(ext4 without data journalling, XFS, vfat, NTFS) on magnetic media. It is *not*
a guarantee on copy-on-write filesystems or flash, so Cadrocfile detects btrfs,
ZFS, bcachefs, overlayfs, F2FS, tmpfs, network mounts and SSDs and says exactly
why the guarantee doesn't hold before you commit.

Files are overwritten with random data, finish with a zero pass, `fsync`ed after
each, truncated, renamed, then unlinked. Symlinks are unlinked, not followed.

</details>

<details>
<summary><b>Source layout</b></summary>

```
src/
  app.rs        application, command line, stylesheet, debug hooks
  config.rs     settings, persisted atomically to ~/.config/cadrocfile
  i18n.rs       gettext internationalisation
  fs/           entry model, scanning, copy/move/delete, trash, shredding,
                recent files, parallel downloads, the shared thread budget
  archive/      libarchive extraction and bsdtar creation
  drives/       UDisks2 over D-Bus, NTFS recovery, LUKS unlocking
  ui/           window and tabs, actions, sidebar, path bar, views, dialogs
```

Long operations run on worker threads and report through an async channel.
Conflicts hand the UI a one-shot reply channel and block the worker on it, so the
decision stays synchronous with the copy loop without the worker touching a
widget.

</details>

## Known gaps

No split view · each network protocol needs its own gvfs backend installed, and
the connect dialog says which · video plays in the preview only when GStreamer
has the codec (`gst-plugins-good`, `gst-libav`), otherwise it shows a still frame
· not yet a desktop file-chooser backend, so browsers still use the GTK save
dialog.

## Licence

MIT
