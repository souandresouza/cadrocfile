#!/bin/sh
# Cadrocfile installer.
#
# Downloads a prebuilt binary when this machine can run one, and builds from
# source when it can't — or when you ask it to. Installs runtime dependencies
# with whatever package manager the distro actually uses.
#
#   curl -fsSL .../install.sh | sh
#   sh install.sh --from-source --prefix /usr/local
#   sh install.sh --uninstall
#
# POSIX sh on purpose: /bin/sh is dash on Debian and busybox on Alpine, and an
# installer that needs bash is an installer that fails on the systems most
# likely to need it.
set -eu

REPO="souandresouza/cadrocfile"
BRANCH="master"
APP="cadrocfile"
DESKTOP="dev.cadrocfile.Files.desktop"
APP_ICON="dev.cadrocfile.Files"

# Minimums the source actually requires — see Cargo.toml's feature flags.
MIN_GTK="4.12"
MIN_ADW="1.5"

MODE="auto"
PREFIX=""
INSTALL_DEPS="yes"
ASSUME_YES="no"
VERSION="latest"

# ── output ──────────────────────────────────────────────────────────────────
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    B=$(printf '\033[1m'); R=$(printf '\033[0m')
    G=$(printf '\033[32m'); Y=$(printf '\033[33m'); E=$(printf '\033[31m')
else
    B=""; R=""; G=""; Y=""; E=""
fi
say()  { printf '%s==>%s %s\n' "$B" "$R" "$*"; }
ok()   { printf '  %s✓%s %s\n' "$G" "$R" "$*"; }
warn() { printf '  %s!%s %s\n' "$Y" "$R" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$E" "$R" "$*" >&2; exit 1; }

usage() {
    cat <<'USAGE'
Usage: install.sh [options]

  --from-source     Build with cargo instead of downloading a binary
  --binary          Download the prebuilt binary; fail rather than build
  --prefix DIR      Install under DIR (default: /usr/local as root, else ~/.local)
  --version TAG     Install a specific release tag (default: latest)
  --no-deps         Don't touch the package manager
  --yes, -y         Don't prompt
  --uninstall       Remove an existing install
  --help, -h        This
USAGE
}

# ── arguments ───────────────────────────────────────────────────────────────
UNINSTALL="no"
while [ $# -gt 0 ]; do
    case "$1" in
        --from-source) MODE="source" ;;
        --binary)      MODE="binary" ;;
        --prefix)      PREFIX="${2:?--prefix needs a directory}"; shift ;;
        --prefix=*)    PREFIX="${1#*=}" ;;
        --version)     VERSION="${2:?--version needs a tag}"; shift ;;
        --version=*)   VERSION="${1#*=}" ;;
        --no-deps)     INSTALL_DEPS="no" ;;
        -y|--yes)      ASSUME_YES="yes" ;;
        --uninstall)   UNINSTALL="yes" ;;
        -h|--help)     usage; exit 0 ;;
        *)             die "unknown option: $1 (try --help)" ;;
    esac
    shift
done

have() { command -v "$1" >/dev/null 2>&1; }

# One scratch directory for the whole run, in a variable rather than returned
# from a function.
#
# `tmp=$(scratch)` would run the function in a subshell: the directory would be
# created, the subshell would exit, and the EXIT trap registered inside it would
# delete the directory before the caller ever wrote to it. Assigning $WORK
# directly keeps both the variable and the trap in the shell that actually uses
# them.
WORK=""
ensure_scratch() {
    [ -n "$WORK" ] && return 0
    WORK=$(mktemp -d) || die "could not create a temporary directory"
    trap 'rm -rf "$WORK"' EXIT INT TERM
}

# Returns success when $1 >= $2, comparing as dotted version numbers.
version_ge() {
    [ "$1" = "$2" ] && return 0
    [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" = "$2" ]
}

confirm() {
    [ "$ASSUME_YES" = "yes" ] && return 0
    # Piping the script into sh leaves stdin consumed, so read from the
    # terminal directly; if there isn't one, take silence as yes.
    [ -r /dev/tty ] || return 0
    printf '  %s [Y/n] ' "$1"
    read -r reply </dev/tty || return 0
    case "$reply" in [Nn]*) return 1 ;; *) return 0 ;; esac
}

# ── privilege ───────────────────────────────────────────────────────────────
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if have sudo; then SUDO="sudo"
    elif have doas; then SUDO="doas"
    fi
fi
as_root() {
    if [ "$(id -u)" -eq 0 ]; then "$@"
    elif [ -n "$SUDO" ]; then $SUDO "$@"
    else die "need root for: $* (install sudo, or run as root)"
    fi
}

# Default prefix: system-wide only when we can actually write there.
if [ -z "$PREFIX" ]; then
    if [ "$(id -u)" -eq 0 ] || [ -n "$SUDO" ]; then
        PREFIX="/usr/local"
    else
        PREFIX="$HOME/.local"
    fi
fi
# Whether writing to the prefix needs elevation is a question about
# permissions, not about paths: `--prefix /opt/x` on a writable /opt, or a
# prefix under /tmp, need no root at all. Walk up to the nearest directory that
# exists and ask the filesystem.
needs_root_for() {
    dir="$1"
    while [ ! -d "$dir" ] && [ "$dir" != "/" ] && [ "$dir" != "." ]; do
        dir=$(dirname "$dir")
    done
    [ -w "$dir" ] && return 1 || return 0
}
if needs_root_for "$PREFIX"; then NEEDS_ROOT="yes"; else NEEDS_ROOT="no"; fi
maybe_root() { if [ "$NEEDS_ROOT" = "yes" ]; then as_root "$@"; else "$@"; fi; }

# ── install paths ───────────────────────────────────────────────────────────
# Defined before the uninstall branch below, which removes the very files these
# name; leaving them further down meant `--uninstall` aborted on an unbound
# variable under `set -u`.
BINDIR="$PREFIX/bin"
APPDIR="$PREFIX/share/applications"
ICONDIR="$PREFIX/share/icons/hicolor"
ICON_SIZES="16 24 32 48 64 128 256 512"

# ── uninstall ───────────────────────────────────────────────────────────────
if [ "$UNINSTALL" = "yes" ]; then
    say "Removing $APP from $PREFIX"
    maybe_root rm -f "$PREFIX/bin/$APP" "$PREFIX/share/applications/$DESKTOP"
    for size in $ICON_SIZES; do
        maybe_root rm -f "$ICONDIR/${size}x${size}/apps/$APP_ICON.png"
    done
    maybe_root update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
    maybe_root gtk-update-icon-cache -qtf "$ICONDIR" 2>/dev/null || true
    ok "removed (config in ~/.config/cadrocfile was left alone)"
    exit 0
fi

# ── distro ──────────────────────────────────────────────────────────────────
DISTRO="unknown"; DISTRO_LIKE=""
if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
    DISTRO="${ID:-unknown}"
    DISTRO_LIKE="${ID_LIKE:-}"
fi

# Collapses derivatives onto the family whose package manager they use, so
# Mint, Pop!_OS, EndeavourOS, Rocky and the rest need no special cases.
family() {
    for id in $DISTRO $DISTRO_LIKE; do
        case "$id" in
            debian|ubuntu)                     echo debian; return ;;
            arch|archlinux|manjaro)            echo arch;   return ;;
            fedora|rhel|centos|"rhel fedora")  echo fedora; return ;;
            opensuse*|suse|sles)               echo suse;   return ;;
            alpine)                            echo alpine; return ;;
            void)                              echo void;   return ;;
            gentoo)                            echo gentoo; return ;;
            nixos)                             echo nixos;  return ;;
            solus)                             echo solus;  return ;;
        esac
    done
    # Fall back to whichever manager is actually present — covers derivatives
    # that set neither ID nor ID_LIKE to anything we know.
    if   have pacman;  then echo arch
    elif have apt-get; then echo debian
    elif have dnf || have yum; then echo fedora
    elif have zypper;  then echo suse
    elif have apk;     then echo alpine
    elif have xbps-install; then echo void
    elif have emerge;  then echo gentoo
    elif have eopkg;   then echo solus
    else echo unknown
    fi
}
FAMILY=$(family)

# ── dependencies ────────────────────────────────────────────────────────────
# Runtime first, then the extras needed only to compile.
# Required: without these Cadrocfile does not start. Installed as one transaction.
runtime_packages() {
    case "$FAMILY" in
        arch)   echo "gtk4 libadwaita libarchive udisks2" ;;
        debian) echo "libgtk-4-1 libadwaita-1-0 libarchive13t64 udisks2" ;;
        fedora) echo "gtk4 libadwaita libarchive udisks2" ;;
        suse)   echo "gtk4 libadwaita-1-0 libarchive13 udisks2" ;;
        alpine) echo "gtk4.0 libadwaita libarchive udisks2" ;;
        void)   echo "gtk4 libadwaita libarchive udisks2" ;;
        gentoo) echo "gui-libs/gtk gui-libs/libadwaita app-arch/libarchive sys-fs/udisks" ;;
        solus)  echo "libgtk-4 libadwaita libarchive udisks2" ;;
        *)      echo "" ;;
    esac
}

# Optional: each unlocks a feature, and Cadrocfile runs without any of them.
# Package names drift between releases of the same distro, so these are
# allowed to fail one at a time — kept apart from the required list because a
# single wrong name in a shared transaction makes the package manager refuse
# the lot, GTK included.
#
#   ntfs-3g, pigz          NTFS repair, fast .tar.gz
#   gvfs + backends        network shares (SMB, NFS, ...)
#   rclone, fuse3          cloud drives
#   ffmpeg, poppler        video and PDF thumbnails
#   gstreamer good, libav  video and audio playing in the preview
optional_packages() {
    case "$FAMILY" in
        arch)   echo "ntfs-3g pigz gvfs gvfs-smb gvfs-nfs rclone fuse3 ffmpeg poppler gst-plugins-good gst-libav" ;;
        debian) echo "ntfs-3g pigz gvfs gvfs-backends gvfs-fuse rclone fuse3 ffmpeg poppler-utils gstreamer1.0-plugins-good gstreamer1.0-libav" ;;
        fedora) echo "ntfs-3g pigz gvfs gvfs-smb gvfs-nfs gvfs-fuse rclone fuse3 ffmpeg-free poppler-utils gstreamer1-plugins-good gstreamer1-plugin-libav" ;;
        suse)   echo "ntfs-3g pigz gvfs gvfs-backend-samba gvfs-fuse rclone fuse3 ffmpeg poppler-tools gstreamer-plugins-good gstreamer-plugins-libav" ;;
        alpine) echo "ntfs-3g pigz gvfs gvfs-smb rclone fuse3 ffmpeg poppler-utils gst-plugins-good gst-libav" ;;
        void)   echo "ntfs-3g pigz gvfs gvfs-smb rclone fuse3 ffmpeg poppler gst-plugins-good1 gst-libav" ;;
        gentoo) echo "sys-fs/ntfs3g app-arch/pigz gnome-base/gvfs net-misc/rclone sys-fs/fuse media-video/ffmpeg app-text/poppler media-libs/gst-plugins-good media-plugins/gst-plugins-libav" ;;
        solus)  echo "ntfs-3g pigz gvfs rclone fuse3 ffmpeg poppler gstreamer-1.0-plugins-good gstreamer-1.0-libav" ;;
        *)      echo "" ;;
    esac
}
build_packages() {
    case "$FAMILY" in
        arch)   echo "base-devel pkgconf" ;;
        debian) echo "build-essential pkg-config libgtk-4-dev libadwaita-1-dev libarchive-dev" ;;
        fedora) echo "gcc pkgconf-pkg-config gtk4-devel libadwaita-devel libarchive-devel" ;;
        suse)   echo "gcc pkg-config gtk4-devel libadwaita-devel libarchive-devel" ;;
        alpine) echo "build-base pkgconf gtk4.0-dev libadwaita-dev libarchive-dev" ;;
        void)   echo "base-devel pkg-config gtk4-devel libadwaita-devel libarchive-devel" ;;
        solus)  echo "-c system.devel libgtk-4-devel libadwaita-devel libarchive-devel" ;;
        *)      echo "" ;;
    esac
}

install_packages() {
    [ $# -gt 0 ] || return 0
    case "$FAMILY" in
        arch)   as_root pacman -S --needed --noconfirm "$@" ;;
        debian) if [ -z "${APT_UPDATED:-}" ]; then
                    as_root env DEBIAN_FRONTEND=noninteractive apt-get update -qq
                    APT_UPDATED=1
                fi
                as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$@" ;;
        fedora) if have dnf; then as_root dnf install -y "$@"; else as_root yum install -y "$@"; fi ;;
        suse)   as_root zypper --non-interactive install "$@" ;;
        alpine) as_root apk add --no-cache "$@" ;;
        void)   as_root xbps-install -Sy "$@" ;;
        gentoo) as_root emerge --noreplace "$@" ;;
        solus)  as_root eopkg install -y "$@" ;;
        *)      return 1 ;;
    esac
}

# Debian renamed libarchive13 to libarchive13t64 in the 64-bit-time_t
# transition, so try the modern name and fall back rather than hardcoding one.
install_deps() {
    [ "$INSTALL_DEPS" = "yes" ] || { warn "skipping dependencies (--no-deps)"; return 0; }

    if [ "$FAMILY" = "nixos" ]; then
        warn "NixOS installs declaratively; add cadrocfile to your configuration"
        warn "or run: nix-shell -p gtk4 libadwaita libarchive cargo"
        return 0
    fi
    if [ "$FAMILY" = "unknown" ]; then
        warn "unrecognised distro — install these yourself:"
        warn "  gtk4 (>= $MIN_GTK), libadwaita (>= $MIN_ADW), libarchive, udisks2"
        warn "  optional: ntfs-3g (NTFS repair), pigz (fast .tar.gz)"
        warn "  optional: gvfs + gvfs-smb (network shares), rclone + fuse3 (cloud drives)"
        warn "  optional: ffmpeg, poppler (video/PDF thumbnails), gstreamer good + libav plugins (media preview)"
        return 0
    fi

    pkgs=$(runtime_packages)
    [ "$MODE" = "source" ] && pkgs="$pkgs $(build_packages)"

    say "Installing dependencies with $FAMILY's package manager"
    if ! install_packages $pkgs; then
        if [ "$FAMILY" = "debian" ]; then
            warn "retrying with the pre-time_t libarchive name"
            pkgs=$(echo "$pkgs" | sed 's/libarchive13t64/libarchive13/')
            install_packages $pkgs || warn "some required packages failed; continuing"
        else
            warn "some required packages failed to install; continuing"
        fi
    fi

    # All at once first, which is fast when every name is right. If the
    # package manager refuses the batch, one at a time, so one unknown name
    # costs only its own feature.
    opts=$(optional_packages)
    if [ -n "$opts" ]; then
        say "Installing optional extras"
        if ! install_packages $opts; then
            warn "retrying optional packages one at a time"
            for p in $opts; do
                install_packages "$p" >/dev/null 2>&1 || warn "  optional package not available here: $p"
            done
        fi
    fi
    ok "dependencies done"
}

# ── can this machine run the prebuilt binary? ───────────────────────────────
fetch() {
    if have curl; then curl -fsSL "$1" -o "$2"
    elif have wget; then wget -qO "$2" "$1"
    else die "need curl or wget"
    fi
}
fetch_stdout() {
    if have curl; then curl -fsSL "$1"
    elif have wget; then wget -qO- "$1"
    else die "need curl or wget"
    fi
}

binary_is_usable() {
    [ "$(uname -s)" = "Linux" ] || { warn "prebuilt binaries are Linux-only"; return 1; }
    [ "$(uname -m)" = "x86_64" ] || { warn "no prebuilt binary for $(uname -m)"; return 1; }

    # musl cannot run a glibc binary. Detect musl positively rather than trying
    # to recognise every way glibc spells its own name — `ldd --version` says
    # "GNU libc" on Arch and Fedora but "Ubuntu GLIBC" on Debian derivatives,
    # so grepping for "glibc" wrongly rejects half the distros that work.
    if ldd --version 2>&1 | head -1 | grep -qi musl; then
        warn "musl libc — the prebuilt binary is built against glibc"
        return 1
    fi
    for loader in /lib/ld-musl-*.so.1; do
        [ -e "$loader" ] || continue
        warn "musl libc — the prebuilt binary is built against glibc"
        return 1
    done

    # `getconf GNU_LIBC_VERSION` prints "glibc 2.44" and is the same everywhere;
    # ldd's banner is only the fallback.
    host_glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | grep -oE '[0-9]+\.[0-9]+' | head -1)
    [ -n "$host_glibc" ] || \
        host_glibc=$(ldd --version 2>&1 | head -1 | grep -oE '[0-9]+\.[0-9]+' | tail -1)
    if [ -n "$host_glibc" ] && ! version_ge "$host_glibc" "2.39"; then
        warn "glibc $host_glibc is older than the 2.39 the binary needs"
        return 1
    fi
    return 0
}

gtk_is_new_enough() {
    have pkg-config || return 0          # can't tell; let it try
    gtk=$(pkg-config --modversion gtk4 2>/dev/null) || return 0
    adw=$(pkg-config --modversion libadwaita-1 2>/dev/null) || return 0
    version_ge "$gtk" "$MIN_GTK" && version_ge "$adw" "$MIN_ADW"
}

# ── install paths ───────────────────────────────────────────────────────────
place() {   # place <binary> <desktop-file> <icon-root>
    maybe_root install -Dm755 "$1" "$BINDIR/$APP"

    # `Exec=cadrocfile` needs the binary on PATH, and a desktop session's PATH is
    # not the shell's: `~/.local/bin` is absent from it on many setups, so the
    # launcher silently does nothing while running it in a terminal works. Bake
    # in the real path, and add TryExec so a launcher hides a broken entry
    # rather than offering one that fails.
    maybe_root install -d "$APPDIR"
    ensure_scratch
    sed -e "s|^Exec=$APP|Exec=$BINDIR/$APP|" \
        -e "s|^Icon=|TryExec=$BINDIR/$APP\\nIcon=|" \
        "$2" > "$WORK/$DESKTOP"
    maybe_root install -m644 "$WORK/$DESKTOP" "$APPDIR/$DESKTOP"

    # Without the icon the desktop entry falls back to a generic glyph, so the
    # app looks unfinished in every launcher and task switcher.
    if [ -n "${3:-}" ] && [ -d "$3" ]; then
        for size in $ICON_SIZES; do
            src="$3/hicolor/${size}x${size}/apps/$APP_ICON.png"
            [ -f "$src" ] || continue
            maybe_root install -Dm644 "$src" \
                "$ICONDIR/${size}x${size}/apps/$APP_ICON.png"
        done
        maybe_root gtk-update-icon-cache -qtf "$ICONDIR" 2>/dev/null || true
    fi

    # Translations: install compiled .mo files for each available language.
    if [ -d "$3" ]; then
        for mo in "$3"/po/*.mo; do
            [ -f "$mo" ] || continue
            lang=$(basename "$mo" .mo)
            maybe_root install -Dm644 "$mo" \
                "$PREFIX/share/locale/$lang/LC_MESSAGES/$APP.mo"
        done
    fi

    maybe_root update-desktop-database "$APPDIR" 2>/dev/null || true
}

# Returns non-zero rather than exiting, so `auto` can fall back to a source
# build when there is no release yet or the download fails. The one exception
# is a checksum mismatch, which is loud and fatal on purpose.
install_binary() {
    say "Downloading the prebuilt binary"
    ensure_scratch; tmp="$WORK"

    if [ "$VERSION" = "latest" ]; then
        base="https://github.com/$REPO/releases/latest/download"
    else
        base="https://github.com/$REPO/releases/download/$VERSION"
    fi

    if ! fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
        warn "no published release found at $base"
        return 1
    fi
    name=$(awk '{print $2}' "$tmp/SHA256SUMS" | grep 'x86_64-linux\.tar\.gz$' | head -1)
    if [ -z "$name" ]; then
        warn "that release has no x86_64 Linux tarball"
        return 1
    fi
    if ! fetch "$base/$name" "$tmp/$name"; then
        warn "download failed: $base/$name"
        return 1
    fi

    say "Verifying checksum"
    if have sha256sum;   then ( cd "$tmp" && grep " $name\$" SHA256SUMS | sha256sum -c - >/dev/null )
    elif have shasum;    then ( cd "$tmp" && grep " $name\$" SHA256SUMS | shasum -a 256 -c - >/dev/null )
    else warn "no sha256sum; cannot verify the download"; fi \
        || die "checksum mismatch — refusing to install"
    ok "checksum verified"

    tar -C "$tmp" -xzf "$tmp/$name" || { warn "could not unpack the tarball"; return 1; }
    dir="$tmp/${name%.tar.gz}"
    [ -x "$dir/$APP" ] || { warn "archive did not contain $APP"; return 1; }

    # Check it can actually run here before replacing anything already
    # installed. `ldd` answers the real question — is every shared library
    # present — without executing a binary we have only just downloaded.
    if have ldd; then
        missing=$(ldd "$dir/$APP" 2>/dev/null | grep 'not found' || true)
        if [ -n "$missing" ]; then
            warn "the binary needs libraries this system does not have:"
            printf '%s\n' "$missing" | sed 's/^/      /' >&2
            return 1
        fi
    fi
    if ! "$dir/$APP" --version >/dev/null 2>&1; then
        warn "the downloaded binary does not run on this system"
        return 1
    fi
    place "$dir/$APP" "$dir/share/applications/$DESKTOP" "$dir/share/icons"
}

install_source() {
    have cargo || die "cargo not found — install Rust from https://rustup.rs, or drop --from-source"
    gtk_is_new_enough || warn "GTK/libadwaita look older than $MIN_GTK/$MIN_ADW; the build may fail"

    say "Building from source (this takes a few minutes)"
    ensure_scratch; tmp="$WORK"

    if [ -f "./Cargo.toml" ] && [ -d "./src" ]; then
        src="."                                   # running inside a checkout
    else
        say "Fetching the source"
        if have git; then
            git clone --depth 1 "https://github.com/$REPO.git" "$tmp/src" >/dev/null 2>&1 \
                || die "git clone failed"
        else
            fetch "https://github.com/$REPO/archive/refs/heads/$BRANCH.tar.gz" "$tmp/src.tar.gz" \
                || die "source download failed"
            mkdir -p "$tmp/src" && tar -C "$tmp/src" --strip-components=1 -xzf "$tmp/src.tar.gz"
        fi
        src="$tmp/src"
    fi

    ( cd "$src" && cargo build --release --locked ) || die "build failed"
    place "$src/target/release/$APP" "$src/data/$DESKTOP" "$src/data/icons"
}

# ── go ──────────────────────────────────────────────────────────────────────
say "Cadrocfile installer"
printf '  distro: %s (%s family)   prefix: %s\n' "$DISTRO" "$FAMILY" "$PREFIX"

install_deps

case "$MODE" in
    source) install_source ;;
    binary) binary_is_usable || die "this system can't run the prebuilt binary; use --from-source"
            install_binary || die "binary install failed" ;;
    auto)
        if binary_is_usable && install_binary; then
            :
        else
            warn "falling back to building from source"
            if [ "$INSTALL_DEPS" = "yes" ] && [ "$FAMILY" != "unknown" ] && [ "$FAMILY" != "nixos" ]; then
                install_packages $(build_packages) || warn "build dependencies incomplete"
            fi
            install_source
        fi
        ;;
esac

ok "installed $BINDIR/$APP"

case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *) warn "$BINDIR is not on your PATH — add it to your shell profile:"
       warn "  export PATH=\"\$PATH:$BINDIR\"" ;;
esac

have pigz    || warn "pigz not installed — .tar.gz creation stays single-threaded"
have ntfsfix || warn "ntfs-3g not installed — NTFS repair will be unavailable"
have rclone  || warn "rclone not installed — cloud drives will be unavailable"
have ffmpeg  || warn "ffmpeg not installed — videos will have no thumbnails"
have pdftoppm || warn "poppler not installed — PDFs will have no thumbnails"
# Cloud drives are FUSE mounts, so the unmount helper is as required as rclone.
have fusermount3 || have fusermount || \
    warn "fuse3 not installed — cloud drives cannot be mounted"
[ -f /usr/share/gvfs/mounts/smb.mount ] || \
    warn "gvfs-smb not installed — Windows network shares will be unavailable"

printf '\n  Run it with: %scadrocfile%s [path]\n\n' "$B" "$R"
