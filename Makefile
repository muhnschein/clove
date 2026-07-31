# clove — developer targets.
#
# Everything here is router-free and runs anywhere, including CI: `make test`
# for the unit and engine suites, `make smoke` for the binaries end to end,
# `make chaos` for crash resilience, `make fuzz` for the parsers.

.PHONY: test smoke chaos fmt lint man-lint doc-lint fuzz fuzz-all fuzz-seed \
        install uninstall

# Install layout. Override on the command line, e.g.
#   make install PREFIX=/usr DESTDIR=$(CURDIR)/pkg
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
MANDIR ?= $(PREFIX)/share/man
DESTDIR ?=

## Unit + engine tests over the mock network.
test:
	cargo test --workspace

## End to end: drive the real binaries as an operator does. Catches
## whole-process faults (deadlocks, startup regressions) that unit tests
## cannot see.
smoke:
	cargo build --workspace
	./ci/smoke.sh

## Crash resilience: SIGKILL storms during state writes, torn temporaries, and
## a state directory that stops being writable. Runs the unwritable-directory
## case only when unprivileged (root ignores permissions).
chaos:
	cargo build --workspace
	./ci/chaos.sh

## Deep, coverage-guided fuzzing (nightly toolchain + cargo-fuzz). The
## every-push parser coverage is `make test`; see fuzz/README.md.
##   make fuzz TARGET=metainfo SECS=600
TARGET ?= bencode
SECS ?= 60
fuzz:
	cargo +nightly fuzz run $(TARGET) -- -max_total_time=$(SECS)

## Every target, per-target budgets, one report file you can send to someone.
## A crash lands in the report with its input and a reproducer, so it can be
## diagnosed away from the machine that found it.
##   make fuzz-all              # ~56 min
##   make fuzz-all SCALE=4      # a long hunt
##   make fuzz-all QUICK=1      # ~5 min, "does it still build and run"
##   make fuzz-all SEED=1       # and keep what it finds (see fuzz-seed)
SCALE ?= 1
fuzz-all:
	@./ci/fuzz.sh $(if $(QUICK),--quick,--scale $(SCALE)) $(if $(SEED),--seed)

## Shrink the local corpus to the smallest set of inputs reaching the same
## coverage, then repack it as the committed seed. Run after a long sweep —
## `fuzz/corpus/` is git-ignored, so this is what makes a run's findings
## outlive the working tree. Minimising is not housekeeping: an unminimised
## corpus makes the next run both slower and shallower. See ci/fuzz-seed.sh.
fuzz-seed:
	@./ci/fuzz-seed.sh

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

## The rustdoc counterpart to man-lint: broken and private intra-doc links are
## errors, not warnings. SCOPE §9 asks for rustdoc on every public item, which
## `missing_docs = "deny"` enforces for presence — this is the half that keeps
## what is there from pointing at items that were renamed or made private.
doc-lint:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

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
