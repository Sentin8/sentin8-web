# SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
# SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
# SPDX-FileCopyrightText: Copyright Contributors to the Sentin8:Web
# SPDX-License-Identifier: Apache-2.0

# Makefile — sentin8-web (service repo, RS §4.5)
#
# Self-contained dev controls for THIS service. Sources the root .env for
# fleet env, guarantees the dev infra, and uses an ABSOLUTE
# SENTIN8_CAPTAIN_BINARY_PREFIX (cwd-independent — RS §4.5.1-1).
SHELL := /bin/bash
CARGO ?= cargo

# This service's identity (from service-registration.json — RS-8).
SERVICE_TYPES := sentin8:web
# Trimmed-boot pin (RS §4.5.1-4): this service + acme — the ONE boot-relevant
# cross-service peer (TLS certs). Everything else fails open (RS §5.1); acme
# itself pins only itself — it is tier 1 and needs nothing.
BOOT_PIN := sentin8:acme,sentin8:web

# Cargo profile flag: empty = dev/debug (default), --release = release.
RELEASE ?=
PROFILED := build start stop upgrade status
PROFILE := $(if $(RELEASE),release,debug)

# Layout anchors. CLI_DIR is the sibling sentin8-cli checkout (platform
# group); a system-installed sentin8-cli is the last-resort fallback.
TREE_ROOT := $(abspath ../..)
GROUP := $(abspath ..)
PLATFORM := $(TREE_ROOT)/sentin8
CLI_DIR := $(abspath ../sentin8-cli)
ROOT_ENV := $(TREE_ROOT)/.env
CLI_BIN := $(firstword $(wildcard $(CLI_DIR)/target/$(PROFILE)/sentin8-cli))
ifeq ($(CLI_BIN),)
CLI_BIN := $(shell command -v sentin8-cli 2>/dev/null)
endif
# Command prefix that runs the sentin8-cli (prebuilt binary when present,
# otherwise cargo-run from the sibling checkout).
CLI_RUN = $(if $(CLI_BIN),$(CLI_BIN),cargo run $(RELEASE) --quiet --manifest-path $(CLI_DIR)/Cargo.toml --bin sentin8-cli --)

# Absolute binary-prefix chain for captain spawning: this repo's
# target/$(PROFILE) first, then every platform service repo's
# target/$(PROFILE) (cwd-independent — RS §4.5.1-1).
PREFIX_DIRS := $(CURDIR)/target/$(PROFILE) \
	$(foreach d,$(wildcard $(PLATFORM)/sentin8-*),$(if $(wildcard $(d)/target/$(PROFILE)),$(d)/target/$(PROFILE),))
space := $(empty) $(empty)
PREFIX := $(subst $(space),:,$(strip $(PREFIX_DIRS)))

# Catalog-path hardening (RS §4.5.1): make every root-.env CATALOG_PATHS
# entry absolute (they are TREE_ROOT-relative in .env.example), then append
# this group's own catalog.json when it is missing.
define catalog_fix
if [ -n "$$SENTIN8_CAPTAIN_CATALOG_PATHS" ]; then \
	ABS=""; oldIFS=$$IFS; IFS=','; \
	for p in $$SENTIN8_CAPTAIN_CATALOG_PATHS; do \
		case "$$p" in /*) ABS="$$ABS$$p," ;; *) ABS="$$ABS$(TREE_ROOT)/$$p," ;; esac; \
	done; IFS=$$oldIFS; \
	export SENTIN8_CAPTAIN_CATALOG_PATHS="$${ABS%,}"; \
fi; \
if [ -f "$(GROUP)/catalog.json" ]; then \
	export SENTIN8_CAPTAIN_CATALOG_PATHS="$${SENTIN8_CAPTAIN_CATALOG_PATHS:+$$SENTIN8_CAPTAIN_CATALOG_PATHS,}$(GROUP)/catalog.json"; \
fi
endef
# Run-dir pinning (RS §4.5.1-2): run_dir() defaults to cwd-relative ".run",
# so a captain started in one repo binds ITS OWN control socket while a CLI
# running elsewhere resolves a stale one (ECONNREFUSED) or falls back to a
# token-gated TCP (401 signed-out). ONE absolute run dir for the whole tree;
# the exports are re- asserted AFTER .env sourcing (the operator file must
# not clobber them).
define run_dir_fix
export SENTIN8_RUN_DIR="$(TREE_ROOT)/.run"; \
export SENTIN8_LOG_DIR="$(TREE_ROOT)/.run/logs"; \
export SENTIN8_CAPTAIN_SOCK="$(TREE_ROOT)/.run/captain.sock"
endef
# Infra guarantee (RS §4.5.1-5): PG 18 + Valkey are HARD deps for most of
# the fleet (RS §5.1). When the platform group's compose stack is down,
# `start` brings it up and waits for accept-connections (bounded).
define infra_check
if [ -z "$$(docker compose -f $(PLATFORM)/docker-compose.yml ps --status=running -q 2>/dev/null)" ]; then \
	echo "→ dev infra down — starting PG 18 + Valkey (RS §2.2.1)"; \
	docker compose -f $(PLATFORM)/docker-compose.yml up -d >/dev/null || exit 1; \
	for i in $$(seq 1 30); do \
		n=$$(docker compose -f $(PLATFORM)/docker-compose.yml ps --status=running -q 2>/dev/null | wc -l); \
		[ "$$n" -ge 2 ] && break; \
		sleep 1; \
	done; \
fi
endef
# Key resolution shared by stop/upgrade/key (RS §4.5.1-3): parse the
# captain services table — rows whose first column is this service's type,
# printing the KEY column (second-to-last field = ip:port).
define resolve_keys
export SENTIN8_RUN_DIR="$(TREE_ROOT)/.run"; export SENTIN8_LOG_DIR="$(TREE_ROOT)/.run/logs"; export SENTIN8_CAPTAIN_SOCK="$(TREE_ROOT)/.run/captain.sock"; types="$(SERVICE_TYPES)"; keys=""; \
for t in $$types; do \
	rows=$$($(CLI_RUN) captain services --no-infra 2>/dev/null | awk -v t="$$t" '$$1==t {print $$(NF-1)}'); \
	keys="$$keys $$rows"; \
done; \
keys=$${keys# }
endef
.PHONY: help build test lint clean start stop restart upgrade key status enroll \
	$(PROFILED:%=%-release) $(PROFILED:%=%-debug)

help: ## Show this help
	@echo "sentin8-web (service: sentin8:web)"
	@grep -E '^[a-z-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## cargo build (dev profile; RELEASE=--release for release)
	@$(CARGO) build $(RELEASE)

test: ## cargo test
	@$(CARGO) test

lint: ## cargo fmt -- --check
	@$(CARGO) fmt -- --check

clean: ## cargo clean
	@$(CARGO) clean
	@rm -rf .run .run/logs .tmp
start: ## Start THIS service: infra + trimmed boot if captain is down; idempotent otherwise
	@test -f $(ROOT_ENV) || { echo "ERROR: no root env — create $(ROOT_ENV) (copy .env.example)" >&2; exit 1; }
	@if [ -z "$(CLI_BIN)" ] && [ ! -f "$(CLI_DIR)/Cargo.toml" ]; then \
		echo "ERROR: sentin8-cli not found — clone the platform group or install it" >&2; exit 1; fi
	@export SENTIN8_CAPTAIN_BINARY_PREFIX="$(PREFIX)"; \
	export SENTIN8_PG_POOL_MAX=$${SENTIN8_PG_POOL_MAX:-8}; \
	set -a; . $(ROOT_ENV); set +a; \
	$(call run_dir_fix); \
	$(call infra_check); \
	if $(CLI_RUN) captain services --no-infra >/dev/null 2>&1; then \
		echo "→ captain running — starting sentin8:web via API (idempotent)"; \
		$(CLI_RUN) captain start sentin8:web; \
	else \
		echo "→ captain down — trimmed boot: $(BOOT_PIN)"; \
		export SENTIN8_SERVICES="$(BOOT_PIN)"; \
		$(CLI_RUN) captain start; \
	fi

stop: ## Stop THIS service's instance(s); captain + rest of the fleet stay up
	@$(resolve_keys); \
	if [ -z "$$keys" ]; then echo "(sentin8:web): not running"; exit 0; fi; \
	for k in $$keys; do \
		echo "→ stopping $$k"; \
		$(CLI_RUN) captain stop-instance "$$k" || exit 1; \
	done

restart: stop start ## stop + start THIS service only

upgrade: build ## Rolling-upgrade THIS service to the locally built binary
	@$(resolve_keys); \
	if [ -z "$$keys" ]; then echo "ERROR: sentin8:web has no running instance — make start first" >&2; exit 1; fi; \
	echo "→ upgrading keys: $$keys"; \
	$(CLI_RUN) captain upgrade-instance $$keys

key: ## Print this service's live instance key(s) (sentin8:web/ip:port)
	@$(resolve_keys); \
	if [ -z "$$keys" ]; then echo "(sentin8:web): no running instance" >&2; exit 1; fi; \
	echo "$$keys" | tr ' ' '\n'

status: ## Fleet rows for THIS service type
	@$(resolve_keys) >/dev/null; $(CLI_RUN) captain services -t $(firstword $(SERVICE_TYPES))

enroll: ## Register this repo (composable enrollment, RS §6)
	@$(CLI_RUN) vertical enroll $(CURDIR)

# Profile wrappers — make foo-release == RELEASE=--release make foo (§4.5).
$(PROFILED:%=%-release):
	@$(MAKE) --no-print-directory RELEASE=--release $(patsubst %-release,%,$@)

$(PROFILED:%=%-debug):
	@$(MAKE) --no-print-directory RELEASE= $(patsubst %-debug,%,$@)
