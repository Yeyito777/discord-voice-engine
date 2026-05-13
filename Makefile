PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
CARGO ?= cargo
INSTALL ?= install
PROFILE ?= release

ifeq ($(PROFILE),release)
CARGO_BUILD_FLAGS := --release
TARGET_DIR := target/release
else
CARGO_BUILD_FLAGS :=
TARGET_DIR := target/debug
endif

BIN := $(TARGET_DIR)/discord-voice-engine
INSTALLED_BIN := $(DESTDIR)$(BINDIR)/discord-voice-engine

.PHONY: all build test check fmt install uninstall clean

all: build

build:
	$(CARGO) build $(CARGO_BUILD_FLAGS)

test:
	$(CARGO) test

check:
	$(CARGO) check

fmt:
	$(CARGO) fmt

install: build
	$(INSTALL) -Dm755 $(BIN) $(INSTALLED_BIN)
	@echo "installed $(INSTALLED_BIN)"

uninstall:
	rm -f $(INSTALLED_BIN)
	@echo "removed $(INSTALLED_BIN)"

clean:
	$(CARGO) clean
