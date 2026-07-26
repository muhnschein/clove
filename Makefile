# clove — developer targets.
#
# Tier 1 (router-free) is `make test` and runs anywhere, including CI.
# Tier 2 (live router) is `make test-live` + `make sam-stress` and needs a
# local I2P router exposing SAMv3 — bring one up with `make router-up`
# (rootless podman quadlet). See docs/LIVE-TESTING.md.

# Which router the live targets talk to: i2pd, java, or emissary.
# All three can run at once — they publish SAM on different host ports — so
# the interop matrix is a loop over this variable, not a teardown dance.
#   make test-live ROUTER=java
ROUTER ?= i2pd
# Every router in the SCOPE §6 matrix, in priority order.
ROUTERS := i2pd java emissary

# Per-router host SAM port and quadlet unit. Inside every container SAM is on
# 7656; only the published host port differs.
SAM_PORT_i2pd     := 7656
SAM_PORT_java     := 7666
SAM_PORT_emissary := 7676
QUADLET_i2pd      := i2pd.container
QUADLET_java      := i2p-java.container
QUADLET_emissary  := emissary.container
UNIT_i2pd         := i2pd
UNIT_java         := i2p-java
UNIT_emissary     := emissary

# SAM control port. Derived from ROUTER; override to point at a router you
# brought up some other way (a distro package, a remote-forwarded port).
SAM_PORT ?= $(SAM_PORT_$(ROUTER))
QUADLET := $(QUADLET_$(ROUTER))
UNIT := $(UNIT_$(ROUTER))

# Concurrency for the R2 stress harness: make sam-stress N=128
N ?= 32
# Seconds router-wait polls for the SAM port before giving up.
WAIT ?= 180

QUADLET_DIR := $(HOME)/.config/containers/systemd

.PHONY: test smoke chaos test-live sam-stress matrix routers report router-ready \
        router-up router-down router-wait router-build router-sam-enable \
        fmt lint man-lint fuzz install uninstall

# Fail early and clearly on a typo'd ROUTER, rather than in the middle of a
# systemctl invocation with an empty unit name.
check-router:
	@case " $(ROUTERS) " in \
		*" $(ROUTER) "*) ;; \
		*) echo "unknown ROUTER=$(ROUTER); one of: $(ROUTERS)" >&2; exit 2 ;; \
	esac
.PHONY: check-router

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

## Crash resilience: SIGKILL storms during state writes, torn temporaries, and
## a state directory that stops being writable. No router needed. Runs the
## unwritable-directory case only when unprivileged (root ignores permissions).
chaos:
	cargo build --workspace
	./ci/chaos.sh

## Tier 2: the router-gated tests (#[ignore]d, keyed on CLOVE_SAM_PORT).
## Waits for SAM first so a cold router doesn't produce spurious failures.
##   make test-live ROUTER=java
test-live: router-wait
	@echo "== live tests against $(ROUTER) (SAM 127.0.0.1:$(SAM_PORT)) =="
	CLOVE_SAM_PORT=$(SAM_PORT) cargo test --workspace -- --ignored --nocapture

## The interop matrix (SCOPE §6): the live tier against every router in turn.
## Keeps going after a failure and reports which routers passed at the end —
## a router that is down should not hide the results of the two that are up.
## Record the outcome in docs/LIVE-TESTING.md §6.3.
matrix:
	@fail=""; pass=""; \
	for r in $(ROUTERS); do \
		echo; echo "################ $$r ################"; \
		if $(MAKE) --no-print-directory test-live ROUTER=$$r; then \
			pass="$$pass $$r"; \
		else \
			fail="$$fail $$r"; \
		fi; \
	done; \
	echo; echo "== matrix summary =="; \
	echo "passed:$${pass:- none}"; \
	echo "failed:$${fail:- none}"; \
	[ -z "$$fail" ]

## Everything that applies on this machine, into one report file to hand over.
## Adds router versions, netDb counts and container logs, so a failure arrives
## already distinguishable from a cold router.
##   make report              # test whatever routers are already up
##   make report ARGS=--up    # bring them up first
ARGS ?=
report:
	./ci/live-report.sh $(ARGS)

## What each router is called and where its SAM lands.
routers:
	@printf '%-10s %-18s %s\n' ROUTER UNIT "SAM (host)"; \
	for r in $(ROUTERS); do \
		$(MAKE) --no-print-directory router-line ROUTER=$$r; \
	done
router-line:
	@printf '%-10s %-18s 127.0.0.1:%s\n' '$(ROUTER)' '$(UNIT)' '$(SAM_PORT)'
.PHONY: router-line

## Can this router do the one thing the live tests need — carry a stream from
## one of its own destinations to another? Two sessions, one dial, bounded.
##
## Worth four minutes before spending twenty on a test that fails the same way.
## A fresh router says "unfinished": its netDb is too thin to resolve a leaseSet
## yet, and the answer is to wait, not to debug clove.
##
## This was 90s, which could not work: sam-stress retries a warming-up leaseSet
## on its own budget, and at ~26s per attempt (dial timeout plus backoff) 90s
## bought three tries against a router that wanted nine. Every observed run
## failed here and skipped the whole matrix behind it. The budget now drives the
## retry loop instead of racing it (see sam-stress's `warmup_deadline`), so this
## number means what it says — but it still has to be large enough for a real
## router, and 90 was not.
READY_DEADLINE ?= 240
router-ready: check-router router-wait
	@echo "== $(ROUTER): one stream between two of its own destinations =="
	CLOVE_SAM_PORT=$(SAM_PORT) CLOVE_STRESS_DEADLINE=$(READY_DEADLINE) 		cargo run --release -p i2pnet --bin sam-stress -- 1

## R2 stress harness: N concurrent streams on one session (docs/PROTOCOL §2.6).
sam-stress:
	CLOVE_SAM_PORT=$(SAM_PORT) cargo run --release -p i2pnet --bin sam-stress -- $(N)

## Build the router image, for routers that have no published one. Only
## emissary needs this; it is a no-op for the others.
router-build: check-router
	@if [ "$(ROUTER)" = emissary ]; then \
		podman build -t localhost/clove-emissary:latest \
			-f contrib/podman/Containerfile.emissary contrib/podman; \
	else \
		echo "$(ROUTER) uses a published image; nothing to build."; \
	fi

## Install and start a router quadlet (rootless). Give it a few minutes on a
## cold start to reseed and build tunnels before running the live targets.
##   make router-up ROUTER=emissary
router-up: check-router
	mkdir -p $(QUADLET_DIR)
	cp contrib/podman/$(QUADLET) $(QUADLET_DIR)/
	systemctl --user daemon-reload
	systemctl --user start $(UNIT)
	@echo "$(ROUTER) starting; a cold router needs a few minutes for tunnels."
	@if [ "$(ROUTER)" != i2pd ]; then \
		echo "$(ROUTER) does not expose SAM until you run:"; \
		echo "    make router-sam-enable ROUTER=$(ROUTER)"; \
	fi

## Switch on the SAM bridge for routers that do not ship it reachable (Java
## I2P has it disabled; emissary binds it inside the container). Run once the
## router has booted; idempotent. i2pd needs nothing.
router-sam-enable: check-router
	./contrib/podman/enable-sam.sh $(ROUTER)

## Stop the router and remove its quadlet unit (the data volume is kept; e.g.
## `podman volume rm clove-i2pd-data` to wipe netDb and keys).
router-down: check-router
	-systemctl --user stop $(UNIT)
	-rm -f $(QUADLET_DIR)/$(QUADLET)
	systemctl --user daemon-reload

## Block until the SAM port answers (or WAIT seconds elapse). Port-open does
## not prove tunnels are built — the live tests tolerate early connect churn.
router-wait: check-router
	@echo "waiting up to $(WAIT)s for $(ROUTER) SAM on 127.0.0.1:$(SAM_PORT)…"
	@for i in $$(seq 1 $(WAIT)); do \
		if timeout 1 bash -c '</dev/tcp/127.0.0.1/$(SAM_PORT)' 2>/dev/null; then \
			echo "SAM is answering."; exit 0; \
		fi; sleep 1; \
	done; \
	echo "$(ROUTER) SAM did not come up on 127.0.0.1:$(SAM_PORT) within $(WAIT)s" >&2; \
	if [ "$(ROUTER)" != i2pd ]; then \
		echo "  (did you run: make router-sam-enable ROUTER=$(ROUTER) ?)" >&2; \
	fi; exit 1

## Deep, coverage-guided fuzzing (nightly toolchain + cargo-fuzz). The
## every-push parser coverage is `make test`; see fuzz/README.md.
##   make fuzz TARGET=metainfo SECS=600
TARGET ?= bencode
SECS ?= 60
fuzz:
	cargo +nightly fuzz run $(TARGET) -- -max_total_time=$(SECS)

## Check the manuals parse and follow mdoc conventions. Unresolved cross-page
## references are expected until the pages are installed, so they are filtered.
man-lint:
	@if ! command -v mandoc >/dev/null 2>&1; then \
		echo "man-lint: SKIP (mandoc is not installed; apt install mandoc)"; \
		exit 0; \
	fi; \
	fail=0; for page in man/*; do \
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
