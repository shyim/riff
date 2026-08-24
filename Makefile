PHP_BIN ?= $(shell command -v php 2>/dev/null)
COMPOSER_SRC_DIR ?= /workspace/composer
COMPOSER_RS_BIN ?= $(CURDIR)/target/debug/sonata

.PHONY: build release fmt test check composer-reference parity

build:
	@cargo build --workspace

release:
	@cargo build --release --workspace

fmt:
	@cargo fmt --all --check

test:
	@cargo test --workspace

check: fmt test release

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
