# Instalação do nvr-dashboard no sistema.
#
#   make            compila em release
#   make install    instala binário, .desktop e ícone (use sudo para PREFIX do sistema)
#   make user-config  cria ~/.config/nvr-dashboard/cameras.toml a partir do exemplo
#   make uninstall  remove o que o install colocou

PREFIX  ?= /usr/local
DESTDIR ?=
BIN     := nvr-dashboard
APP_ID  := io.github.nvrdashboard.NvrDashboard

BINDIR     := $(DESTDIR)$(PREFIX)/bin
APPDIR     := $(DESTDIR)$(PREFIX)/share/applications
ICONDIR    := $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps
DATADIR    := $(DESTDIR)$(PREFIX)/share/$(BIN)
USER_CONFIG := $(HOME)/.config/$(BIN)/cameras.toml

.PHONY: all build test check lint install uninstall user-config clean

all: build

build:
	cargo build --release

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings

# Valida a configuração e o alcance dos NVRs, sem abrir a interface.
check: build
	./target/release/$(BIN) --check

install: build
	install -Dm755 target/release/$(BIN)          $(BINDIR)/$(BIN)
	install -Dm644 packaging/$(APP_ID).desktop    $(APPDIR)/$(APP_ID).desktop
	install -Dm644 packaging/$(APP_ID).svg        $(ICONDIR)/$(APP_ID).svg
	install -Dm644 config/cameras.example.toml    $(DATADIR)/cameras.example.toml
	-update-desktop-database $(APPDIR) 2>/dev/null
	-gtk4-update-icon-cache -qtf $(DESTDIR)$(PREFIX)/share/icons/hicolor 2>/dev/null
	@echo
	@echo "Instalado. Agora crie a configuração:  make user-config"

# Cria a configuração do usuário com permissão 600, sem sobrescrever a existente.
user-config:
	@if [ -f "$(USER_CONFIG)" ]; then \
		echo "$(USER_CONFIG) já existe; nada a fazer."; \
	else \
		install -Dm600 config/cameras.example.toml "$(USER_CONFIG)"; \
		echo "Criado $(USER_CONFIG) (modo 600) — preencha host, usuário e senha."; \
	fi

uninstall:
	rm -f $(BINDIR)/$(BIN) $(APPDIR)/$(APP_ID).desktop $(ICONDIR)/$(APP_ID).svg
	rm -rf $(DATADIR)
	-update-desktop-database $(APPDIR) 2>/dev/null

clean:
	cargo clean
