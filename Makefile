PREFIX ?= $(HOME)/.local
BINDIR  = $(DESTDIR)$(PREFIX)/bin
APPDIR  = $(DESTDIR)$(PREFIX)/share/applications
ICONDIR = $(DESTDIR)$(PREFIX)/share/icons/hicolor
ICON_SIZES = 16 24 32 48 64 128 256 512

APP = cadrocfile
DESKTOP = dev.cadrocfile.Files.desktop
APP_ICON = dev.cadrocfile.Files

.PHONY: all build install uninstall test clean run

all: build

build:
	cargo build --release

test:
	cargo test

# Installs to ~/.local by default, which needs no root. Use
# `sudo make install PREFIX=/usr/local` for a system-wide install.
# Deliberately does not depend on `build`. A system install runs under sudo, and
# building as root leaves root-owned artefacts in ./target and writes to
# /root/.cargo — so the binary is built as you, and only the copying is
# privileged. Run `make` first.
install:
	@test -x target/release/$(APP) || { \
	  echo "target/release/$(APP) is missing — run 'make' first (as your user)."; \
	  exit 1; \
	}
	install -Dm755 target/release/$(APP) $(BINDIR)/$(APP)
	@# `Exec=cadrocfile` needs the binary on PATH, and a desktop session's PATH is
	@# not the shell's — `~/.local/bin` is missing from it on many setups, so the
	@# launcher silently fails while a terminal works. Bake in the real
	@# path, and add TryExec so launchers hide a broken entry instead of
	@# offering one that does nothing.
	install -d $(APPDIR)
	sed -e 's|^Exec=$(APP)|Exec=$(PREFIX)/bin/$(APP)|' \
	    -e 's|^Icon=|TryExec=$(PREFIX)/bin/$(APP)\nIcon=|' \
	    data/$(DESKTOP) > $(APPDIR)/$(DESKTOP)
	chmod 644 $(APPDIR)/$(DESKTOP)
	for s in $(ICON_SIZES); do \
	  install -Dm644 data/icons/hicolor/$${s}x$${s}/apps/$(APP_ICON).png \
	    $(ICONDIR)/$${s}x$${s}/apps/$(APP_ICON).png; \
	done
	-update-desktop-database $(APPDIR) 2>/dev/null
	-gtk-update-icon-cache -qtf $(ICONDIR) 2>/dev/null
	@echo "Installed to $(BINDIR)/$(APP)"
	@case ":$$PATH:" in *":$(PREFIX)/bin:"*) ;; \
	  *) echo "Note: $(PREFIX)/bin is not on your PATH.";; esac

uninstall:
	rm -f $(BINDIR)/$(APP) $(APPDIR)/$(DESKTOP)
	@for s in $(ICON_SIZES); do \
	  rm -f $(ICONDIR)/$${s}x$${s}/apps/$(APP_ICON).png; \
	done
	-update-desktop-database $(APPDIR) 2>/dev/null
	-gtk-update-icon-cache -qtf $(ICONDIR) 2>/dev/null

run:
	cargo run --release

clean:
	cargo clean
