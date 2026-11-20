#!/usr/bin/env bash
# Install Markdone for the current user (no root needed): the binary in ~/.local/bin, the
# desktop entry and the icon in ~/.local/share, so it shows up in the launcher and in
# "Open with" for Markdown files.
#
# From a release archive, the `markdone` binary next to this script is installed; from
# the source tree (packaging/linux/install.sh), it is built first (cargo, release mode).
#
#   install.sh             install
#   install.sh --default   also make Markdone the default app for Markdown files
#   install.sh --uninstall remove what install put there (the configuration in
#                          ~/.config/markdone and the session stay)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
bin_dir=${XDG_BIN_HOME:-$HOME/.local/bin}
data=${XDG_DATA_HOME:-$HOME/.local/share}
apps=$data/applications
icons=$data/icons/hicolor/scalable/apps

refresh() {
    update-desktop-database -q "$apps" 2>/dev/null || true
    gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
    if command -v kbuildsycoca6 >/dev/null; then kbuildsycoca6 >/dev/null 2>&1 || true; fi
}

if [ "${1:-}" = --uninstall ]; then
    rm -f "$bin_dir/markdone" "$apps/markdone.desktop" "$icons/markdone.svg"
    refresh
    echo "Markdone removed."
    exit 0
fi

if [ -x "$here/markdone" ]; then
    binary=$here/markdone
else
    repo=$(cd "$here/../.." && pwd)
    cargo build --release --manifest-path "$repo/Cargo.toml"
    binary=$repo/target/release/markdone
fi
install -Dm755 "$binary" "$bin_dir/markdone"
# Absolute Exec path: the desktop session's PATH may not include ~/.local/bin.
install -d "$apps"
sed "s|^Exec=markdone|Exec=$bin_dir/markdone|" "$here/markdone.desktop" \
    > "$apps/markdone.desktop"
chmod 644 "$apps/markdone.desktop"
install -Dm644 "$here/markdone.svg" "$icons/markdone.svg"
refresh
if [ "${1:-}" = --default ]; then
    xdg-mime default markdone.desktop text/markdown text/x-markdown
    echo "Markdone is now the default application for Markdown files."
fi
echo "Markdone installed: $bin_dir/markdone"
