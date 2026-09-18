# Thin task-runner wrapper around the scripts in ./script.
#
# Those scripts stay the source of truth (channel detection, code signing,
# Info.plist updates, bundled resources); these targets only provide short
# entry points for the two most common local loops. Run `./script/bootstrap`
# once first to install the build dependencies.
#
# Pass extra flags through ARGS, e.g.
#   make dev ARGS="--features with_local_server"
#   make dev ARGS="--open_with_launchd"

SHELL := /bin/bash
UNAME_S := $(shell uname -s)
ARGS ?=

.DEFAULT_GOAL := help
.PHONY: help dev app oss

help:
	@echo "make dev   - build a debug bundle and launch it (development / debugging)"
	@echo "make app   - build the app without launching it, ready to install"
	@echo "make oss   - OSS debug .app (channel oss, single-arch, selfsign; skips DMG)"
	@echo ""
	@echo "Pass extra flags with ARGS=\"...\" (forwarded to the underlying script)."

# Development / debugging: debug profile.
# On macOS this bundles and launches a real signed .app so URL schemes and
# notifications behave like a shipped build; elsewhere it is `cargo run`.
dev:
	./script/run $(ARGS)

# Build the app as a distributable artifact, without running it.
app:
ifeq ($(UNAME_S),Darwin)
	./script/run --release --dont-open $(ARGS)
	@printf '\nBuilt app bundle(s):\n'
	@find "$$(cargo metadata --no-deps --format-version 1 | jq -r '.target_directory')/release/bundle/osx" \
		-maxdepth 1 -name '*.app' -print 2>/dev/null || true
	@printf '\nInstall with: cp -R <path>.app /Applications/\n'
else
	./script/bundle $(ARGS)
endif

# Local OSS debug .app only (no DMG — create-dmg Finder AppleScript often times out locally).
oss:
	./script/bundle --channel oss --debug --nouniversal --selfsign --skip-dmg $(ARGS)
