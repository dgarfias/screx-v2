PROFILE ?= release

ifeq ($(PROFILE),release)
  CARGO_FLAGS = --release
else
  CARGO_FLAGS =
endif

UNAME := $(shell uname -s)

.PHONY: build clean

build:
	cargo build $(CARGO_FLAGS)
ifeq ($(UNAME),Darwin)
	@./bundle-macos.sh $(PROFILE)
	@echo ""
	@echo "  macOS app bundle ready:"
	@echo "    target/$(PROFILE)/Screx.app"
	@echo ""
	@echo "  Run with:"
	@echo "    open target/$(PROFILE)/Screx.app"
	@echo ""
endif

clean:
	cargo clean
