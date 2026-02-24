PREFIX ?= /usr
DESTDIR ?=
LIBEXECDIR ?= $(PREFIX)/libexec
DATADIR ?= $(PREFIX)/share
SYSTEMD_USER_DIR ?= $(LIBEXECDIR)/systemd/user

BINARY = xdg-desktop-portal-generic
CARGO ?= cargo

.PHONY: all build install uninstall clean

all: build

build:
	$(CARGO) build --release

install: build
	install -Dm755 target/release/$(BINARY) $(DESTDIR)$(LIBEXECDIR)/$(BINARY)
	install -Dm644 data/generic.portal $(DESTDIR)$(DATADIR)/xdg-desktop-portal/portals/generic.portal
	install -Dm644 data/org.freedesktop.impl.portal.desktop.generic.service $(DESTDIR)$(DATADIR)/dbus-1/services/org.freedesktop.impl.portal.desktop.generic.service
	install -Dm644 data/xdg-desktop-portal-generic.service $(DESTDIR)$(SYSTEMD_USER_DIR)/xdg-desktop-portal-generic.service

uninstall:
	rm -f $(DESTDIR)$(LIBEXECDIR)/$(BINARY)
	rm -f $(DESTDIR)$(DATADIR)/xdg-desktop-portal/portals/generic.portal
	rm -f $(DESTDIR)$(DATADIR)/dbus-1/services/org.freedesktop.impl.portal.desktop.generic.service
	rm -f $(DESTDIR)$(SYSTEMD_USER_DIR)/xdg-desktop-portal-generic.service

clean:
	$(CARGO) clean
