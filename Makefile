.PHONY: check test clippy clippy-session fmt fmt-check build check-all run clean install-portal-config install-session uninstall-session

PREFIX ?= /usr/local
WAYLAND_SESSIONS ?= /usr/share/wayland-sessions
PORTALS_DIR ?= /usr/share/xdg-desktop-portal

check:
	cargo check --locked

fmt-check:
	cargo fmt --check

build:
	cargo build --release --locked

check-all: fmt-check clippy clippy-session test build

test:
	cargo test --locked

clippy:
	cargo clippy --locked --all-targets -- -D warnings

clippy-session:
	cargo clippy --locked --no-default-features --all-targets -- -D warnings

fmt:
	cargo fmt

run:
	cargo run --locked -- --nested --nested-mod alt --config config/config.kdl

install-portal-config:
	install -Dm644 config/evawm-portals.conf "$${XDG_CONFIG_HOME:-$(HOME)/.config}/xdg-desktop-portal/evawm-portals.conf"

install-session:
	install -Dm755 target/release/evawm "$(DESTDIR)$(PREFIX)/bin/evawm"
	install -Dm755 scripts/evawm-session "$(DESTDIR)$(PREFIX)/bin/evawm-session"
	install -Dm644 config/evawm.desktop "$(DESTDIR)$(WAYLAND_SESSIONS)/evawm.desktop"
	install -Dm644 config/evawm-portals.conf "$(DESTDIR)$(PORTALS_DIR)/evawm-portals.conf"

uninstall-session:
	rm -f "$(DESTDIR)$(PREFIX)/bin/evawm"
	rm -f "$(DESTDIR)$(PREFIX)/bin/evawm-session"
	rm -f "$(DESTDIR)$(WAYLAND_SESSIONS)/evawm.desktop"
	rm -f "$(DESTDIR)$(PORTALS_DIR)/evawm-portals.conf"

clean:
	cargo clean
