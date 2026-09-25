#!/usr/bin/env bash
# Gera o pacote .deb (Debian/Ubuntu) do nvr-dashboard em dist/.
#
#   tools/build-deb.sh
#
# Pré-requisito: o ambiente de compilação pronto (no Ubuntu 24.04, rode antes
# tools/setup-ubuntu.sh). Compile na MAIS ANTIGA versão que você quer suportar:
# o .deb herda a versão mínima da glibc/GTK do sistema onde foi gerado.
#
# O plugin `gtk4paintablesink` (que o Ubuntu 24.04 não empacota) vai DENTRO do
# .deb, numa pasta própria, e um script de lançamento o soma ao GST_PLUGIN_PATH:
# assim não conflita com o pacote `gstreamer1.0-gtk4` das distros que o têm.
#
# Variáveis:
#   GTK4_PLUGIN_SO   caminho do libgstgtk4.so a embutir (padrão: o instalado por
#                    tools/setup-ubuntu.sh em ~/.local/share/gstreamer-1.0/plugins)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

NAME=nvr-dashboard
APP_ID=io.github.nvrdashboard.NvrDashboard
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
ARCH="$(dpkg --print-architecture)"
LIBDIR="/usr/lib/$NAME"
OUT="$ROOT/dist"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

echo "==> Compilando ($NAME $VERSION, $ARCH)"
cargo build --release

# --- árvore do pacote ------------------------------------------------------------
install -Dm755 "target/release/$NAME" "$STAGE$LIBDIR/$NAME"
install -Dm644 "packaging/$APP_ID.desktop" "$STAGE/usr/share/applications/$APP_ID.desktop"
install -Dm644 "packaging/$APP_ID.svg" "$STAGE/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"
install -Dm644 config/cameras.example.toml "$STAGE/usr/share/$NAME/cameras.example.toml"
install -Dm644 README.md "$STAGE/usr/share/doc/$NAME/README.md"

# Script de lançamento: soma a pasta de plugins do pacote ao caminho do GStreamer.
install -d "$STAGE/usr/bin"
cat > "$STAGE/usr/bin/$NAME" <<WRAP
#!/bin/sh
d=$LIBDIR
if [ -d "\$d/gstreamer-1.0" ]; then
    export GST_PLUGIN_PATH_1_0="\$d/gstreamer-1.0\${GST_PLUGIN_PATH_1_0:+:\$GST_PLUGIN_PATH_1_0}"
fi
exec "\$d/$NAME" "\$@"
WRAP
chmod 755 "$STAGE/usr/bin/$NAME"

# --- plugin gtk4paintablesink --------------------------------------------------
PLUGIN_SO="${GTK4_PLUGIN_SO:-$HOME/.local/share/gstreamer-1.0/plugins/libgstgtk4.so}"
DEPENDS_EXTRA=""
if [ -f "$PLUGIN_SO" ]; then
    echo "==> Embutindo $PLUGIN_SO"
    install -Dm755 "$PLUGIN_SO" "$STAGE$LIBDIR/gstreamer-1.0/libgstgtk4.so"
else
    echo "AVISO: libgstgtk4.so não encontrado; o pacote dependerá de gstreamer1.0-gtk4" >&2
    DEPENDS_EXTRA=", gstreamer1.0-gtk4"
fi

# --- metadados -----------------------------------------------------------------
SIZE_KB="$(du -sk "$STAGE" | cut -f1)"
install -d "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<CTL
Package: $NAME
Version: $VERSION
Section: video
Priority: optional
Architecture: $ARCH
Installed-Size: $SIZE_KB
Maintainer: nvr-dashboard <noreply@localhost>
Depends: libc6 (>= 2.35), libgtk-4-1 (>= 4.12), libglib2.0-0, libgstreamer1.0-0 (>= 1.22), libgstreamer-plugins-base1.0-0, libgraphene-1.0-0, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad, gstreamer1.0-libav, gstreamer1.0-gl$DEPENDS_EXTRA
Recommends: gstreamer1.0-pulseaudio | gstreamer1.0-pipewire, gstreamer1.0-plugins-ugly
Homepage: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs
Description: Dashboard de câmeras RTSP (NVR iCSee/XMEye e câmeras IP)
 Mostra ao vivo, num grid, câmeras de NVRs e câmeras IP via RTSP. Cadastro por
 varredura de rede ou manual, reconexão automática, gravação, captura de tela,
 áudio e detecção de movimento.
CTL

mkdir -p "$OUT"
DEB="$OUT/${NAME}_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$STAGE" "$DEB" >/dev/null
echo "==> Gerado: $DEB ($(du -h "$DEB" | cut -f1))"
dpkg-deb --info "$DEB" | sed -n '1,12p'
