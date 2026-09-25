# Instalação do nvr-dashboard no sistema.
#
#   sudo make install     compila (como o SEU usuário) e instala binário, .desktop e ícone
#   sudo make uninstall   remove o que o install colocou (seus dados ficam)
#   make                  só compila em release
#   make user-config      cria ~/.config/nvr-dashboard/cameras.toml (ajustes opcionais)
#
# Com `sudo`, o cargo é executado como o usuário que chamou o sudo: o root não tem
# o Rust do rustup configurado, e compilar como root deixaria `target/` dele.

PREFIX  ?= /usr/local
DESTDIR ?=
BIN     := nvr-dashboard
APP_ID  := io.github.nvrdashboard.NvrDashboard

BINDIR     := $(DESTDIR)$(PREFIX)/bin
APPDIR     := $(DESTDIR)$(PREFIX)/share/applications
ICONDIR    := $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps
DATADIR    := $(DESTDIR)$(PREFIX)/share/$(BIN)

# Usuário "de verdade": com sudo, quem chamou o sudo; senão, o atual.
IS_ROOT   := $(shell id -u)
REAL_USER := $(if $(SUDO_USER),$(SUDO_USER),$(shell id -un))
REAL_HOME := $(shell getent passwd $(REAL_USER) | cut -d: -f6)
# Prefixo para rodar como o usuário real quando estamos sob sudo (vazio caso contrário).
AS_USER   := $(if $(filter 0,$(IS_ROOT)),$(if $(SUDO_USER),sudo -u $(SUDO_USER) -H,),)
USER_CONFIG := $(REAL_HOME)/.config/$(BIN)/cameras.toml

.PHONY: all build test check lint install uninstall user-config clean

all: build

build:
	$(AS_USER) sh -c 'PATH="$$HOME/.cargo/bin:$$PATH" cargo build --release'

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
	@echo "Instalado. Abra o \"NVR Dashboard\" no menu (ou rode: $(BIN)) e cadastre as câmeras."
	@echo "Ajustes opcionais (gravação, movimento…):  make user-config"

# Cria os ajustes do usuário (opcionais) com permissão 600, sem sobrescrever.
# As câmeras NÃO ficam aqui: são cadastradas pela janela do app.
user-config:
	@if [ -f "$(USER_CONFIG)" ]; then \
		echo "$(USER_CONFIG) já existe; nada a fazer."; \
	else \
		$(AS_USER) install -Dm600 config/cameras.example.toml "$(USER_CONFIG)"; \
		echo "Criado $(USER_CONFIG) (modo 600). É opcional: ajustes de gravação, movimento etc."; \
	fi

uninstall:
	rm -f $(BINDIR)/$(BIN) $(APPDIR)/$(APP_ID).desktop $(ICONDIR)/$(APP_ID).svg
	rm -rf $(DATADIR)
	-update-desktop-database $(APPDIR) 2>/dev/null
	-gtk4-update-icon-cache -qtf $(DESTDIR)$(PREFIX)/share/icons/hicolor 2>/dev/null
	@echo
	@echo "Removido. Seus dados foram mantidos (câmeras, layout, ajustes)."
	@echo "Para apagá-los também:  rm -r $(REAL_HOME)/.config/$(BIN)"

clean:
	cargo clean
