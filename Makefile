# clove — developer targets.
#
# Tier 1 (router-free) is `make test` and runs anywhere, including CI.
# Tier 2 (live router) is `make test-live` + `make sam-stress` and needs a
# local I2P router exposing SAMv3 — bring one up with `make router-up`
# (rootless podman quadlet). See docs/LIVE-TESTING.md.

# SAM control port; override to point at an existing router.
SAM_PORT ?= 7656
# Concurrency for the R2 stress harness: make sam-stress N=128
N ?= 32
# Seconds router-wait polls for the SAM port before giving up.
WAIT ?= 180

QUADLET_DIR := $(HOME)/.config/containers/systemd

.PHONY: test smoke test-live sam-stress router-up router-down router-wait \
        fmt lint man-lint install uninstall

# Install layout. Override on the command line, e.g.
#   make install PREFIX=/usr DESTDIR=$(CURDIR)/pkg
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
MANDIR ?= $(PREFIX)/share/man
DESTDIR ?=

## Tier 1: unit + engine tests over the mock network. No router needed.
test:
	cargo test --workspace

## Tier 1, end to end: drive the real binaries as an operator does. Catches
## whole-process faults (deadlocks, startup regressions) that unit tests
## cannot see. No router needed.
smoke:
	cargo build --workspace
	./ci/smoke.sh

## Tier 2: the router-gated tests (#[ignore]d, keyed on CLOVE_SAM_PORT).
## Waits for SAM first so a cold router doesn't produce spurious failures.
test-live: router-wait
	CLOVE_SAM_PORT=$(SAM_PORT) cargo test --workspace -- --ignored --nocapture

## R2 stress harness: N concurrent streams on one session (docs/PROTOCOL §2.6).
sam-stress:
	CLOVE_SAM_PORT=$(SAM_PORT) cargo run --release -p i2pnet --bin sam-stress -- $(N)

## Install and start the i2pd quadlet (rootless). Give it a few minutes on a
## cold start to reseed and build tunnels before running the live targets.
router-up:
	mkdir -p $(QUADLET_DIR)
	cp contrib/podman/i2pd.container $(QUADLET_DIR)/
	systemctl --user daemon-reload
	systemctl --user start i2pd
	@echo "i2pd starting; a cold router needs a few minutes for tunnels."

## Stop the router and remove the quadlet unit (the data volume is kept;
## `podman volume rm clove-i2pd-data` to wipe netDb and keys).
router-down:
	-systemctl --user stop i2pd
	-rm -f $(QUADLET_DIR)/i2pd.container
	systemctl --user daemon-reload

## Block until the SAM port answers (or WAIT seconds elapse). Port-open does
## not prove tunnels are built — the live tests tolerate early connect churn.
router-wait:
	@echo "waiting up to $(WAIT)s for SAM on 127.0.0.1:$(SAM_PORT)…"
	@for i in $$(seq 1 $(WAIT)); do \
		if timeout 1 bash -c '</dev/tcp/127.0.0.1/$(SAM_PORT)' 2>/dev/null; then \
			echo "SAM is answering."; exit 0; \
		fi; sleep 1; \
	done; \
	echo "SAM did not come up on 127.0.0.1:$(SAM_PORT) within $(WAIT)s" >&2; exit 1

## Check the manuals parse and follow mdoc conventions. Unresolved cross-page
## references are expected until the pages are installed, so they are filtered.
man-lint:
	@fail=0; for page in man/*; do \
		out=$$(mandoc -T lint -W warning "$$page" 2>&1 \
			| grep -v 'referenced manual not found'); \
		if [ -n "$$out" ]; then echo "$$out"; fail=1; fi; \
	done; \
	[ $$fail -eq 0 ] && echo "man-lint: ok"

## Install the binaries and manuals. Release build; strip nothing, so a
## crash report from a user still carries symbols.
install:
	cargo build --workspace --release
	install -d $(DESTDIR)$(BINDIR)
	install -m 0755 target/release/cloved $(DESTDIR)$(BINDIR)/cloved
	install -m 0755 target/release/clove $(DESTDIR)$(BINDIR)/clove
	install -d $(DESTDIR)$(MANDIR)/man1 $(DESTDIR)$(MANDIR)/man5 \
		$(DESTDIR)$(MANDIR)/man7 $(DESTDIR)$(MANDIR)/man8
	install -m 0644 man/clove.1 $(DESTDIR)$(MANDIR)/man1/clove.1
	install -m 0644 man/clove.conf.5 $(DESTDIR)$(MANDIR)/man5/clove.conf.5
	install -m 0644 man/clove-api.7 $(DESTDIR)$(MANDIR)/man7/clove-api.7
	install -m 0644 man/cloved.8 $(DESTDIR)$(MANDIR)/man8/cloved.8

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/cloved $(DESTDIR)$(BINDIR)/clove
	rm -f $(DESTDIR)$(MANDIR)/man1/clove.1 \
		$(DESTDIR)$(MANDIR)/man5/clove.conf.5 \
		$(DESTDIR)$(MANDIR)/man7/clove-api.7 \
		$(DESTDIR)$(MANDIR)/man8/cloved.8

## CI-parity convenience.
fmt:
	cargo fmt --all --check

lint:
	cargo clippy --workspace --all-targets -- -D warnings
