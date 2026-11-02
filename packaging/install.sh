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
#   /etc/lorawan-sniffer/sniffer.toml               configuration (kept on upgrade;
#                                                   a fresh one gets a generated
#                                                   login token, login stays off)
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

# A random login token, as 32 lower-case hexadecimal characters (128 bits from
# the operating system). Prints it, or fails without printing a usable value.
# Each command substitution keeps the status of the last command in it, so an
# od or tr that fails after printing something still fails the function; the
# shape check at the end catches a short or empty read. Every step returns
# explicitly because "set -e" does not apply inside a function that is called
# as "x=$(gen_token) || die".
gen_token() {
    tok_raw=$(od -An -N16 -tx1 /dev/urandom) || return 1
    tok_hex=$(printf '%s' "$tok_raw" | tr -d ' \n') || return 1
    case $tok_hex in
    *[!0-9a-f]* | '') return 1 ;;
    esac
    [ ${#tok_hex} -eq 32 ] || return 1
    printf '%s\n' "$tok_hex"
}

# What with_token has done so far. It is kept until the target has its final
# mode, because a shell runs a trap only after the command it is waiting for
# has finished: a signal that arrives during the rename is handled after it.
#   tok_phase=''         nothing in progress
#   tok_phase=creating   the stage may or may not exist yet; the target is untouched
#   tok_phase=created    the stage is complete and tok_stage_id is its inode; the
#                        rename may be running or may just have happened
#   tok_phase=published  mv returned success; the target still needs its final mode
# The names are global, since POSIX sh has no "local"; they all start with tok_.
tok_phase=''
tok_stage=''
tok_target=''
tok_stage_id=''

tok_finish() {
    chmod 0644 "$tok_target" ||
        echo "install.sh: $tok_target is complete but readable by root only," \
            "so the service cannot read it; run: chmod 0644 $tok_target" >&2
}

# Leave neither a staged file nor a target without its final mode behind.
# A stage that is gone is not taken as proof of a rename, because it may never
# have been created: only a target that is the very file that was staged (same
# inode; the rename keeps it) is finished, and in "creating" the target is not
# touched at all, so a failed stage cannot change the mode of an existing file.
tok_cleanup() {
    case $tok_phase in
    creating) rm -f "$tok_stage" ;;
    created)
        if [ -e "$tok_stage" ]; then
            rm -f "$tok_stage"
        elif [ "$(stat -c %i "$tok_target" 2>/dev/null)" = "$tok_stage_id" ]; then
            tok_finish
        fi
        ;;
    published) tok_finish ;;
    esac
    tok_phase=''
}
trap tok_cleanup EXIT
# A signal trap that only cleaned up would let the script carry on; these also
# stop it, with the usual failure status of the signal.
trap 'tok_cleanup; exit 130' INT
trap 'tok_cleanup; exit 143' TERM

# Install a copy of a configuration template with a freshly generated login
# token in it, as a file anyone can read (the service runs as another user).
# The copy is staged next to the target and stays readable by root only until
# it has been renamed into place, so the token is never world-readable under a
# temporary name; the target gets its final mode right after the rename. This
# is why put() is not used for files that carry a token: it creates the
# temporary file with the final mode.
with_token() { # template target
    # The template must have exactly one empty token line, or it changed shape.
    [ "$(grep -c '^    token = ""$' "$1")" -eq 1 ] ||
        die "$1 does not have exactly one empty token line in [sniffer.api]"
    tok_value=$(gen_token) || die "cannot read random bytes from /dev/urandom"
    tok_target=$2
    tok_stage="$2.tmp.$$"
    tok_phase=creating
    (umask 077 && sed "s/^    token = \"\"\$/    token = \"$tok_value\"/" "$1" \
        >"$tok_stage") || die "cannot write $tok_stage"
    tok_stage_id=$(stat -c %i "$tok_stage") || die "cannot read $tok_stage"
    tok_phase=created
    mv -f "$tok_stage" "$tok_target" || die "cannot install $tok_target"
    tok_phase=published
    if ! chmod 0644 "$tok_target"; then
        tok_phase=''
        die "$tok_target is complete but readable by root only, so the service" \
            "cannot read it; run: chmod 0644 $tok_target"
    fi
    tok_phase=''
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

    # A run that was killed (kill -9, power loss) can leave a root-only staged
    # copy of the configuration behind; the glob also covers sniffer.toml.new.
    rm -f "$CONF_DIR"/sniffer.toml.tmp.* "$CONF_DIR"/sniffer.toml.new.tmp.*

    # Both copies carry a freshly generated login token. An existing
    # sniffer.toml is never edited; the token in sniffer.toml.new is there to
    # be copied from.
    if [ -e "$CONF_DIR/sniffer.toml" ]; then
        with_token "$here/sniffer.toml" "$CONF_DIR/sniffer.toml.new"
        conf_note="kept $CONF_DIR/sniffer.toml; the template for this version is sniffer.toml.new, with a generated login token you can copy"
    else
        with_token "$here/sniffer.toml" "$CONF_DIR/sniffer.toml"
        conf_note="installed $CONF_DIR/sniffer.toml; a login token was generated in it; login is off until you set login = true"
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
