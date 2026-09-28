# Contributing to Cadrocfile

Thanks for your interest in contributing! This document explains how to set up
your development environment and what we expect from contributions.

## Development setup

### Prerequisites

- [Rust](https://rustup.rs/) 1.92 or later
- GTK 4.12+, libadwaita 1.5+, libarchive development headers
- `pkg-config`, `gettext`

### Install dependencies

**Debian/Ubuntu:**
```sh
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev \
  libarchive-dev gettext
```

**Fedora:**
```sh
sudo dnf install gcc pkgconf-pkg-config gtk4-devel libadwaita-devel \
  libarchive-devel gettext-devel
```

**Arch:**
```sh
sudo pacman -S base-devel pkgconf gtk4 libadwaita libarchive gettext
```

### Build and test

```sh
cargo build
cargo test
```

Or use the Makefile:

```sh
make          # build
make test     # run tests
make install  # install to ~/.local
```

## Project structure

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

## Code style

- Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Use `cargo fmt` before committing
- Run `cargo clippy` and fix warnings
- Keep functions small and focused
- Document public APIs with doc comments

## Translations

Cadrocfile uses gettext for internationalization. To add a language:

1. Create `po/<lang>.po` from the template (`po/cadrocfile.pot`)
2. Fill in the translations
3. The build system compiles `.po` to `.mo` automatically

## Reporting bugs

When filing a bug, please include:

- Your distribution and version
- GTK/libadwaita versions (`pkg-config --modversion gtk4 libadwaita`)
- Steps to reproduce
- What you expected vs. what happened
- Output with `CADROCFILE_TRACE=1` if relevant

## Pull requests

1. Fork the repository and create a feature branch
2. Make your changes with clear, focused commits
3. Add tests for new functionality
4. Update documentation (README, doc comments) as needed
5. Ensure `cargo test` passes and `cargo clippy` is clean
6. Open a PR with a clear description of the change

## License

By contributing, you agree that your contributions will be licensed under the
MIT License.
