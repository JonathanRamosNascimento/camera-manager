#!/usr/bin/env bash
# Empacota o camera-manager para Windows (x64): pasta autocontida, .zip portátil e,
# se o Inno Setup estiver disponível, um instalador .exe.
#
# Roda no MSYS2, terminal "MSYS2 MINGW64". Pré-requisitos (pacman):
#   mingw-w64-x86_64-{toolchain,rust,gtk4,gstreamer,gst-plugins-base,gst-plugins-good,
#   gst-plugins-bad,gst-plugins-ugly,gst-libav,adwaita-icon-theme} git zip
# e o plugin gtk4paintablesink compilado (veja tools/build-gtk4-plugin.sh), cujo
# caminho vai em GTK4_PLUGIN_DLL.
#
#   GTK4_PLUGIN_DLL=/caminho/gstgtk4.dll tools/package-windows.sh
#
# Variáveis:
#   GTK4_PLUGIN_DLL     (obrigatória) gstgtk4.dll do gst-plugins-rs
#   ISCC                caminho do ISCC.exe do Inno Setup (padrão: procura no PATH e em
#                       "C:\Program Files (x86)\Inno Setup 6")
#   OPENVINO_RUNTIME_DIR (opcional) pasta onde foi extraído o runtime do OpenVINO para
#                       Windows (o .zip "w_openvino_toolkit_windows..." de
#                       https://docs.openvino.ai/, baixado à parte — não vem com o
#                       MSYS2). É a pasta que contém `runtime\`, não o zip. Sem essa
#                       variável, o pacote sai igual, só que sem acelerar por NPU/GPU
#                       Intel (sempre CPU via tract).
#
# O app se autoconfigura ao achar `lib/gstreamer-1.0` ao lado do .exe (src/bundle.rs).
# Se houver também uma pasta `openvino/`, ele aponta `INTEL_OPENVINO_DIR` pra ela.
#
# O ícone (icone.ico, na raiz do repo) é embutido no .exe pelo build.rs (windres, do
# mingw-w64-x86_64-toolchain) e reaproveitado pelo instalador (Inno Setup).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

NAME=camera-manager
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
PREFIX="${MSYSTEM_PREFIX:-/mingw64}"
OUT="$ROOT/dist"
DIST="$OUT/$NAME-$VERSION-windows-x64"

: "${GTK4_PLUGIN_DLL:?defina GTK4_PLUGIN_DLL com o caminho do gstgtk4.dll}"
[ -f "$GTK4_PLUGIN_DLL" ] || { echo "não achei $GTK4_PLUGIN_DLL" >&2; exit 1; }

echo "==> Compilando ($NAME $VERSION)"
cargo build --release

rm -rf "$DIST"
mkdir -p "$DIST/lib/gstreamer-1.0" "$DIST/libexec/gstreamer-1.0" "$DIST/share"
cp "target/release/$NAME.exe" "$DIST/"

echo "==> Plugins do GStreamer"
cp "$PREFIX"/lib/gstreamer-1.0/*.dll "$DIST/lib/gstreamer-1.0/"
cp "$GTK4_PLUGIN_DLL" "$DIST/lib/gstreamer-1.0/"
cp "$PREFIX/libexec/gstreamer-1.0/gst-plugin-scanner.exe" "$DIST/libexec/gstreamer-1.0/"

echo "==> DLLs de que o app, os plugins e o scanner dependem"
# `ldd` já é transitivo; guardamos só o que vem do MSYS2 (não as DLLs do Windows).
{
    ldd "$DIST/$NAME.exe"
    for f in "$DIST"/lib/gstreamer-1.0/*.dll "$DIST"/libexec/gstreamer-1.0/*.exe; do
        ldd "$f" 2>/dev/null || true
    done
} | awk '/=>/ {print $3}' | grep -i "^$PREFIX/" | sort -u | while read -r dll; do
    cp -n "$dll" "$DIST/"
done

echo "==> Dados do GTK (schemas, ícones, pixbuf)"
mkdir -p "$DIST/share/glib-2.0"
cp -r "$PREFIX/share/glib-2.0/schemas" "$DIST/share/glib-2.0/"
mkdir -p "$DIST/share/icons"
for theme in Adwaita hicolor; do
    [ -d "$PREFIX/share/icons/$theme" ] && cp -r "$PREFIX/share/icons/$theme" "$DIST/share/icons/"
done
for size in 16 24 32 48 64 128 256; do
    install -Dm644 "packaging/icons/$size.png" \
        "$DIST/share/icons/hicolor/${size}x${size}/apps/io.github.cameramanager.CameraManager.png"
done
[ -d "$PREFIX/lib/gdk-pixbuf-2.0" ] && cp -r "$PREFIX/lib/gdk-pixbuf-2.0" "$DIST/lib/"
install -Dm644 config/cameras.example.toml "$DIST/cameras.example.toml"
install -Dm644 README.md "$DIST/README.md"
install -Dm644 icone.ico "$DIST/icone.ico"

if [ -n "${OPENVINO_RUNTIME_DIR:-}" ]; then
    echo "==> OpenVINO (NPU/GPU Intel)"
    [ -d "$OPENVINO_RUNTIME_DIR/runtime" ] || {
        echo "OPENVINO_RUNTIME_DIR não tem uma pasta runtime/ dentro: $OPENVINO_RUNTIME_DIR" >&2
        exit 1
    }
    OV_DIST="$DIST/openvino"
    # Só as subpastas que o openvino-finder (crate) procura via INTEL_OPENVINO_DIR;
    # o resto do runtime (headers, samples, python...) não serve pro app empacotado.
    for sub in runtime/bin/intel64/Release runtime/3rdparty/tbb/bin; do
        if [ -d "$OPENVINO_RUNTIME_DIR/$sub" ]; then
            mkdir -p "$OV_DIST/$sub"
            cp -r "$OPENVINO_RUNTIME_DIR/$sub/." "$OV_DIST/$sub/"
        fi
    done
    [ -d "$OV_DIST/runtime/bin/intel64/Release" ] || {
        echo "não achei runtime/bin/intel64/Release em $OPENVINO_RUNTIME_DIR (layout inesperado?)" >&2
        exit 1
    }
    [ -d "$OPENVINO_RUNTIME_DIR/licensing" ] && cp -r "$OPENVINO_RUNTIME_DIR/licensing" "$OV_DIST/"
else
    echo "AVISO: OPENVINO_RUNTIME_DIR não definido; pacote sairá sem NPU/GPU (sempre CPU)." >&2
fi

echo "==> ZIP portátil"
( cd "$OUT" && rm -f "$NAME-$VERSION-windows-x64.zip" && zip -qr "$NAME-$VERSION-windows-x64.zip" "$(basename "$DIST")" )

ISCC="${ISCC:-$(command -v iscc || true)}"
if [ -z "$ISCC" ] && [ -x "/c/Program Files (x86)/Inno Setup 6/ISCC.exe" ]; then
    ISCC="/c/Program Files (x86)/Inno Setup 6/ISCC.exe"
fi
if [ -n "$ISCC" ]; then
    echo "==> Instalador (Inno Setup)"
    MSYS2_ARG_CONV_EXCL="*" "$ISCC" "/DAppVersion=$VERSION" "/DSourceDir=$(cygpath -w "$DIST")" \
        "/DOutputDir=$(cygpath -w "$OUT")" "$(cygpath -w "$ROOT/packaging/windows/camera-manager.iss")"
else
    echo "AVISO: Inno Setup (ISCC.exe) não encontrado; só o .zip foi gerado." >&2
fi

echo "==> Pronto:"
ls -lh "$OUT" | grep -E "windows" || true
