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

LOCAL_AGENT_LISTEN ?= 127.0.0.1:9377

.DEFAULT_GOAL := help
.PHONY: help dev app oss agent agent-release

help:
	@echo "make dev           - build a debug bundle and launch it (development / debugging)"
	@echo "make app           - build the app without launching it, ready to install"
	@echo "make oss           - OSS debug .app (channel oss, single-arch, selfsign; skips DMG)"
	@echo "make agent         - run the local agent service that answers Ollama agent turns"
	@echo "make agent-release - the same service, built with optimizations"
	@echo ""
	@echo "Pass extra flags with ARGS=\"...\" (forwarded to the underlying script)."
	@echo "Override the service address with LOCAL_AGENT_LISTEN=host:port."

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

# Agent turns for a configured Ollama model are answered by this service rather than by the app,
# so iterating on prompts, tool schemas or providers means restarting it instead of rebuilding
# Warp. Point Warp at it from Settings > AI, or with WARP_LOCAL_AGENT_URL.
agent:
	cargo run -p warp_local_agent -- --listen $(LOCAL_AGENT_LISTEN) $(ARGS)

agent-release:
	cargo run --release -p warp_local_agent -- --listen $(LOCAL_AGENT_LISTEN) $(ARGS)
