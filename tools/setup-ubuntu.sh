#!/usr/bin/env bash
# Prepara o Ubuntu 24.04 (e derivados/Debian recentes) para compilar e rodar o
# nvr-dashboard. Roda como usuário comum; usa `sudo` só para o `apt`.
#
# O que ele resolve (e por que não dá só um `apt install`):
#   1. O Rust do apt no Ubuntu 24.04 é o 1.75; o projeto exige >= 1.92
#      (edition 2024 + crates gtk4/gstreamer recentes) -> instala via rustup.
#   2. O Ubuntu 24.04 não empacota o elemento `gtk4paintablesink` (vem do
#      gst-plugins-rs) -> compila só esse plugin, na versão para o GStreamer 1.24,
#      e instala em ~/.local/share/gstreamer-1.0/plugins (o GStreamer procura lá
#      sozinho, sem variável de ambiente).
#
# Uso:  tools/setup-ubuntu.sh
# Variáveis opcionais:
#   GST_PLUGINS_RS_REF   branch/tag do gst-plugins-rs (padrão: 0.13, p/ GStreamer 1.24)
#   NO_APT=1             não roda o apt (você já instalou as dependências)
set -euo pipefail

REF="${GST_PLUGINS_RS_REF:-0.13}"
PLUGIN_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/gstreamer-1.0/plugins"
MIN_RUST="1.92"

log() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }

if [ "$(id -u)" -eq 0 ]; then
    echo "Rode como usuário comum (sem sudo): o script pede sudo só para o apt." >&2
    exit 1
fi

# --- 1. Dependências do sistema -------------------------------------------------
if [ -z "${NO_APT:-}" ]; then
    log "Instalando dependências com apt (pede a sua senha)"
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends \
        build-essential pkg-config curl git ca-certificates \
        libgtk-4-dev libglib2.0-dev libgraphene-1.0-dev \
        libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
        gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav \
        gstreamer1.0-gl gstreamer1.0-pulseaudio gstreamer1.0-pipewire \
        libwayland-dev libx11-dev libegl-dev libgl-dev libdrm-dev \
        libgtk-4-bin desktop-file-utils
fi

# --- 2. Rust >= 1.92 --------------------------------------------------------------
rust_ok() {
    command -v cargo >/dev/null 2>&1 || return 1
    local have; have="$(rustc --version | awk '{print $2}')"
    [ "$(printf '%s\n%s\n' "$MIN_RUST" "$have" | sort -V | head -n1)" = "$MIN_RUST" ]
}
if ! rust_ok; then
    log "Instalando Rust (rustup) — o do apt é antigo demais (precisa >= $MIN_RUST)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
rust_ok || { echo "Rust >= $MIN_RUST não encontrado; rode: rustup update stable" >&2; exit 1; }
log "Rust: $(rustc --version)"

# --- 3. gtk4paintablesink ---------------------------------------------------------
if gst-inspect-1.0 gtk4paintablesink >/dev/null 2>&1; then
    log "gtk4paintablesink já disponível; nada a compilar"
else
    log "Compilando o plugin gtk4paintablesink (gst-plugins-rs @ $REF) — leva alguns minutos"
    SRC="$(mktemp -d)"
    trap 'rm -rf "$SRC"' EXIT
    git clone --depth 1 --branch "$REF" \
        https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs.git "$SRC/gst-plugins-rs"
    (cd "$SRC/gst-plugins-rs" && cargo build --release -p gst-plugin-gtk4)
    mkdir -p "$PLUGIN_DIR"
    install -m755 "$SRC/gst-plugins-rs/target/release/libgstgtk4.so" "$PLUGIN_DIR/"
    log "Plugin instalado em $PLUGIN_DIR"
fi

# --- 4. Verificação ---------------------------------------------------------------
log "Verificando o ambiente"
gst-inspect-1.0 gtk4paintablesink rtspsrc splitmuxsink parsebin >/dev/null
echo "GStreamer: $(gst-inspect-1.0 --version | head -n1)"
echo "GTK4:      $(pkg-config --modversion gtk4)"
echo
echo "Pronto. Agora, no diretório do projeto:"
echo "  cargo build --release && ./target/release/nvr-dashboard"
echo "  (ou: sudo make install)"
