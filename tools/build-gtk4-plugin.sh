#!/usr/bin/env bash
# Compila o plugin `gtk4paintablesink` (gst-plugins-rs) na versão que casa com o
# GStreamer instalado. Usado para Windows e macOS, onde não há pacote pronto.
#
#   tools/build-gtk4-plugin.sh [pasta-de-saída]
#
# Escreve o caminho da biblioteca gerada na última linha da saída (stdout).
# Variável: GST_PLUGINS_RS_REF força a branch/tag (padrão: escolhida pela versão).
set -euo pipefail

OUT="${1:-$PWD/gtk4-plugin}"

ver="$(pkg-config --modversion gstreamer-1.0)"        # ex.: 1.26.4
minor="$(echo "$ver" | cut -d. -f2)"
# gst-plugins-rs lança um ramo por versão do GStreamer: 0.13 ↔ 1.24, 0.14 ↔ 1.26,
# 0.15 ↔ 1.28... (0.<minor/2 + 1>).
REF="${GST_PLUGINS_RS_REF:-0.$(( minor / 2 + 1 ))}"

echo "GStreamer $ver -> gst-plugins-rs @ $REF" >&2
SRC="$(mktemp -d)"
git clone --depth 1 --branch "$REF" \
    https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs.git "$SRC/gst-plugins-rs" >&2
( cd "$SRC/gst-plugins-rs" && cargo build --release -p gst-plugin-gtk4 >&2 )

mkdir -p "$OUT"
lib="$(find "$SRC/gst-plugins-rs/target/release" -maxdepth 1 \
    \( -name 'libgstgtk4.so' -o -name 'libgstgtk4.dylib' -o -name 'gstgtk4.dll' -o -name 'libgstgtk4.dll' \) | head -n1)"
[ -n "$lib" ] || { echo "biblioteca do plugin não encontrada" >&2; exit 1; }
cp "$lib" "$OUT/"
echo "$OUT/$(basename "$lib")"
