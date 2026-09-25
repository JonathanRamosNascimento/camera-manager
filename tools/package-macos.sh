#!/usr/bin/env bash
# Empacota o nvr-dashboard como "NVR Dashboard.app" e .dmg para macOS.
#
# Pré-requisitos (Homebrew):
#   brew install gtk4 gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad \
#                gst-plugins-ugly gst-libav dylibbundler librsvg pkg-config
# e o plugin gtk4paintablesink compilado (tools/build-gtk4-plugin.sh), cujo caminho
# vai em GTK4_PLUGIN_DYLIB.
#
#   GTK4_PLUGIN_DYLIB=/caminho/libgstgtk4.dylib tools/package-macos.sh
#
# Variáveis:
#   GTK4_PLUGIN_DYLIB     (obrigatória) libgstgtk4.dylib do gst-plugins-rs
#   MACOS_SIGN_IDENTITY   identidade do certificado "Developer ID Application: ..."
#                         (padrão: assinatura "ad-hoc", que basta para rodar no
#                         próprio Mac mas NÃO passa no Gatekeeper de outros Macs)
#
# Estratégia de bibliotecas: tudo é copiado para Contents/Resources/lib e referenciado
# por @rpath; cada binário recebe o LC_RPATH certo para a sua posição. O app se
# autoconfigura ao achar Resources/lib/gstreamer-1.0 (src/bundle.rs).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

NAME=nvr-dashboard
APP_NAME="NVR Dashboard"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
ARCH="$(uname -m)"                                   # arm64 ou x86_64
BREW="$(brew --prefix)"
OUT="$ROOT/dist"
APP="$OUT/$APP_NAME.app"
MACOS="$APP/Contents/MacOS"
RES="$APP/Contents/Resources"

: "${GTK4_PLUGIN_DYLIB:?defina GTK4_PLUGIN_DYLIB com o caminho do libgstgtk4.dylib}"
[ -f "$GTK4_PLUGIN_DYLIB" ] || { echo "não achei $GTK4_PLUGIN_DYLIB" >&2; exit 1; }

echo "==> Compilando ($NAME $VERSION, $ARCH)"
cargo build --release

rm -rf "$APP"
mkdir -p "$MACOS" "$RES/lib/gstreamer-1.0" "$RES/libexec/gstreamer-1.0" "$RES/share"
cp "target/release/$NAME" "$MACOS/$NAME"

echo "==> Plugins do GStreamer"
for p in "$BREW"/lib/gstreamer-1.0/*.dylib; do
    # o brew deixa symlinks quebrados de plugins cujas dependências não estão instaladas
    if [ -e "$p" ]; then cp "$p" "$RES/lib/gstreamer-1.0/"; fi
done
cp -f "$GTK4_PLUGIN_DYLIB" "$RES/lib/gstreamer-1.0/"
SCANNER_SRC="$(find -L "$BREW/opt/gstreamer" -name gst-plugin-scanner -type f | head -n1)"
cp "$SCANNER_SRC" "$RES/libexec/gstreamer-1.0/gst-plugin-scanner"
chmod u+w "$RES"/lib/gstreamer-1.0/*.dylib "$RES/libexec/gstreamer-1.0/gst-plugin-scanner" "$MACOS/$NAME"

echo "==> Copiando as bibliotecas de que dependem (dylibbundler)"
# `-p @rpath/` faz cada dependência ser referenciada como @rpath/<lib>.dylib.
# Array, não `$(printf ...)`: o caminho do .app tem espaço ("NVR Dashboard.app").
BUNDLE_ARGS=(-x "$MACOS/$NAME" -x "$RES/libexec/gstreamer-1.0/gst-plugin-scanner")
for f in "$RES"/lib/gstreamer-1.0/*.dylib; do BUNDLE_ARGS+=(-x "$f"); done
dylibbundler -of -b -cd -d "$RES/lib" -p "@rpath/" "${BUNDLE_ARGS[@]}" >/dev/null

echo "==> rpaths (cada binário enxerga Resources/lib de onde está)"
add_rpath() { install_name_tool -add_rpath "$2" "$1" 2>/dev/null || true; }
add_rpath "$MACOS/$NAME" "@executable_path/../Resources/lib"
add_rpath "$RES/libexec/gstreamer-1.0/gst-plugin-scanner" "@loader_path/../../lib"
for f in "$RES"/lib/gstreamer-1.0/*.dylib; do add_rpath "$f" "@loader_path/.."; done
for f in "$RES"/lib/*.dylib; do add_rpath "$f" "@loader_path"; done

echo "==> Dados do GTK (schemas, ícones)"
mkdir -p "$RES/share/glib-2.0" "$RES/share/icons"
cp -R "$BREW/share/glib-2.0/schemas" "$RES/share/glib-2.0/"
for theme in Adwaita hicolor; do
    [ -d "$BREW/share/icons/$theme" ] && cp -R "$BREW/share/icons/$theme" "$RES/share/icons/"
done
install -Dm644 packaging/io.github.nvrdashboard.NvrDashboard.svg \
    "$RES/share/icons/hicolor/scalable/apps/io.github.nvrdashboard.NvrDashboard.svg"
install -Dm644 config/cameras.example.toml "$RES/cameras.example.toml"

echo "==> Ícone e Info.plist"
MINOS="$(sw_vers -productVersion | cut -d. -f1).0"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@MINOS@/$MINOS/g" packaging/macos/Info.plist.in \
    > "$APP/Contents/Info.plist"
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 64 128 256 512; do
    rsvg-convert -w "$size" -h "$size" packaging/io.github.nvrdashboard.NvrDashboard.svg \
        -o "$ICONSET/icon_${size}x${size}.png"
    rsvg-convert -w "$((size * 2))" -h "$((size * 2))" packaging/io.github.nvrdashboard.NvrDashboard.svg \
        -o "$ICONSET/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$ICONSET" -o "$RES/AppIcon.icns"

echo "==> Assinatura"
IDENTITY="${MACOS_SIGN_IDENTITY:--}"                  # "-" = ad-hoc
# Assina de dentro para fora: bibliotecas, plugins, scanner e, por fim, o app.
find "$RES" -type f \( -name '*.dylib' -o -name 'gst-plugin-scanner' \) -print0 \
    | xargs -0 -n1 codesign --force --sign "$IDENTITY" >/dev/null
codesign --force --deep --sign "$IDENTITY" "$APP"
[ "$IDENTITY" = "-" ] && echo "AVISO: assinatura ad-hoc — em outros Macs, abra com botão direito > Abrir." >&2

echo "==> DMG"
DMG="$OUT/$NAME-$VERSION-macos-$ARCH.dmg"
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
echo "==> Pronto: $DMG ($(du -h "$DMG" | cut -f1))"
