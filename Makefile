PHP_BIN ?= $(shell command -v php 2>/dev/null)
COMPOSER_SRC_DIR ?= /workspace/composer
COMPOSER_RS_BIN ?= $(CURDIR)/target/debug/composer-rs

ifdef IN_NIX_SHELL
DEV_RUN := bash -e -o pipefail -c
else
DEV_RUN := nix develop path:$(CURDIR) --command bash -e -o pipefail -c
endif

.PHONY: build release fmt test check flake-check composer-reference parity

build:
	@$(DEV_RUN) 'cargo build --workspace'

release:
	@$(DEV_RUN) 'cargo build --release --workspace'

fmt:
	@$(DEV_RUN) 'cargo fmt --all --check'

test:
	@$(DEV_RUN) 'cargo test --workspace'

check: fmt test release

flake-check:
	@nix flake check path:$(CURDIR)

composer-reference: build
	@test -f "$(COMPOSER_SRC_DIR)/bin/composer" || { echo "Missing Composer checkout at $(COMPOSER_SRC_DIR)" >&2; exit 1; }
	@if test ! -f "$(COMPOSER_SRC_DIR)/vendor/autoload.php"; then \
		COMPOSER_RS_PHP="$(PHP_BIN)" "$(COMPOSER_RS_BIN)" install --no-scripts --no-plugins --no-audit --no-interaction -d "$(COMPOSER_SRC_DIR)"; \
	fi

parity: composer-reference
	@for suite in core mutations operations validate config status platform; do \
		COMPOSER_SRC_DIR="$(COMPOSER_SRC_DIR)" PHP_BIN="$(PHP_BIN)" COMPOSER_RS_BIN="$(COMPOSER_RS_BIN)" \
			"./scripts/composer-$$suite-differential.sh" || exit; \
	done
