#!/bin/sh
# Install, upgrade or remove the LoRaWAN Packet Sniffer as a systemd service.
#
# Run it as root from the unpacked release folder:
#
#   sudo ./install.sh              install, or upgrade an existing install
#   sudo ./install.sh --uninstall  remove the binary and the service
#
# Installed files:
#
#   /usr/local/bin/lorawan-sniffer                  the program
#   /etc/systemd/system/lorawan-sniffer.service     the service
#   /etc/lorawan-sniffer/sniffer.toml               configuration (kept on upgrade)
#   /etc/lorawan-sniffer/sniffer.env                secrets, root only (kept on upgrade)
#   /var/lib/lorawan-sniffer/                       database, created by the service
#
# The service is never started or restarted by this script; it prints the
# command to run instead, so an upgrade on a busy server happens when you
# choose.

set -eu

NAME=lorawan-sniffer
BIN=/usr/local/bin/$NAME
UNIT=/etc/systemd/system/$NAME.service
CONF_DIR=/etc/$NAME

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

die() {
    echo "install.sh: $*" >&2
    exit 1
}

# Copy to a temporary name next to the target, then rename over it. Renaming
# is atomic and leaves a running copy of the program untouched, where writing
# into the file in place would fail with "text file busy".
put() { # mode source target
    install -m "$1" "$2" "$3.tmp.$$"
    mv -f "$3.tmp.$$" "$3"
}

uninstall() {
    systemctl disable --now "$NAME.service" 2>/dev/null || true
    rm -f "$BIN" "$UNIT"
    systemctl daemon-reload
    echo "Removed $BIN and $UNIT."
    echo "Kept, to delete by hand if no longer needed:"
    echo "  $CONF_DIR                 configuration and secrets"
    # With a dynamic user, systemd keeps the data under /var/lib/private and
    # /var/lib/$NAME is only a link to it; deleting the link leaves the data.
    if [ -d "/var/lib/private/$NAME" ]; then
        echo "  /var/lib/private/$NAME    database (/var/lib/$NAME links to it)"
    else
        echo "  /var/lib/$NAME            database"
    fi
    if [ -d "$UNIT.d" ]; then
        echo "  $UNIT.d   service overrides; a reinstall would use them again"
    fi
}

install_all() {
    for f in "$NAME" sniffer.toml sniffer.env "$NAME.service"; do
        [ -f "$here/$f" ] || die "$f not found next to install.sh; run it from the unpacked release folder"
    done
    # A binary for another architecture fails here, before anything is copied.
    version=$("$here/$NAME" --version 2>/dev/null) ||
        die "this build does not run on this machine ($(uname -m)); download the archive for $(uname -m)"

    upgrade=no
    [ -e "$BIN" ] && upgrade=yes

    install -d -m 0755 "$(dirname "$BIN")" "$CONF_DIR"
    put 0755 "$here/$NAME" "$BIN"
    put 0644 "$here/$NAME.service" "$UNIT"

    if [ -e "$CONF_DIR/sniffer.toml" ]; then
        put 0644 "$here/sniffer.toml" "$CONF_DIR/sniffer.toml.new"
        conf_note="kept $CONF_DIR/sniffer.toml; the template for this version is sniffer.toml.new"
    else
        put 0644 "$here/sniffer.toml" "$CONF_DIR/sniffer.toml"
        conf_note="installed $CONF_DIR/sniffer.toml"
    fi
    env_note=""
    if [ ! -e "$CONF_DIR/sniffer.env" ]; then
        put 0600 "$here/sniffer.env" "$CONF_DIR/sniffer.env"
    elif [ "$(stat -c '%a %u' "$CONF_DIR/sniffer.env")" != "600 0" ]; then
        # It holds secrets, and only systemd (as root) needs to read it.
        chown root:root "$CONF_DIR/sniffer.env"
        chmod 0600 "$CONF_DIR/sniffer.env"
        env_note="made $CONF_DIR/sniffer.env readable by root only, as it holds secrets"
    fi

    systemctl daemon-reload

    echo "Installed $version: $BIN"
    echo "Configuration: $conf_note"
    if [ -n "$env_note" ]; then
        echo "Secrets: $env_note"
    fi
    if [ "$upgrade" = yes ] && systemctl is-active --quiet "$NAME.service"; then
        echo
        echo "The service is still running the previous version. Restart it with:"
        echo "  systemctl restart $NAME"
    elif systemctl is-enabled --quiet "$NAME.service" 2>/dev/null; then
        echo
        echo "The service is enabled but not running. Start it with:"
        echo "  systemctl start $NAME"
    else
        echo
        echo "Next steps:"
        echo "  1. Review $CONF_DIR/sniffer.toml (ports, upstream LNS)"
        echo "  2. systemctl enable --now $NAME"
        echo "  3. journalctl -u $NAME -f"
    fi
}

[ "$(id -u)" -eq 0 ] || die "must run as root (sudo $0)"
command -v systemctl >/dev/null 2>&1 || die "systemd (systemctl) not found"

case "${1:-}" in
    "") install_all ;;
    --uninstall) uninstall ;;
    *) die "unknown option '$1' (use no option to install, or --uninstall)" ;;
esac
