#!/usr/bin/env bash
# Puts a Font Changer AppImage into the application menu (section "Office") of Linux Mint, Ubuntu and other
# desktops that follow the freedesktop standard. No root needed.
#
#   scripts/install-appimage-linux.sh "path/to/Font Changer_1.0.0_amd64.AppImage"    install
#   scripts/install-appimage-linux.sh --uninstall                                    remove again
set -euo pipefail

APP_DIR="$HOME/.local/share/font-changer"
DESKTOP_FILE="$HOME/.local/share/applications/font-changer.desktop"
ICON_FILE="$HOME/.local/share/icons/hicolor/128x128/apps/font-changer.png"

refresh() {
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$HOME/.local/share/applications" >/dev/null 2>&1 || true
  command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$HOME/.local/share/icons/hicolor" >/dev/null 2>&1 || true
}

if [ "${1:-}" = "--uninstall" ]; then
  rm -rf "$APP_DIR" "$DESKTOP_FILE" "$ICON_FILE"
  refresh
  echo "Font Changer was removed from the menu."
  exit 0
fi

SRC="${1:-}"
if [ -z "$SRC" ] || [ ! -f "$SRC" ]; then
  echo "Usage: $0 <path to Font Changer .AppImage>   |   $0 --uninstall" >&2
  exit 1
fi

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ICON_SRC="$HERE/../src-tauri/icons/128x128.png"

mkdir -p "$APP_DIR" "$(dirname "$DESKTOP_FILE")" "$(dirname "$ICON_FILE")"
cp "$SRC" "$APP_DIR/Font-Changer.AppImage"
chmod +x "$APP_DIR/Font-Changer.AppImage"
[ -f "$ICON_SRC" ] && cp "$ICON_SRC" "$ICON_FILE"

cat > "$DESKTOP_FILE" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Font Changer
Name[ru]=Font Changer
Comment=Batch font and font size changer for documents
Comment[ru]=Массовая смена шрифтов и размеров в документах
Exec="$APP_DIR/Font-Changer.AppImage"
Icon=font-changer
Terminal=false
Categories=Office;
Keywords=font;fonts;size;docx;xlsx;pptx;odt;rtf;html;шрифт;шрифты;размер;
DESKTOP

refresh
echo "Done. Font Changer is now in the menu: Office > Font Changer."
