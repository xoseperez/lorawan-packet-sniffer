#!/bin/sh
# Check that package-deb.sh puts the systemd parts into the maintainer scripts
# exactly as debhelper does.
#
#   packaging/deb/check-autoscripts.sh [ARCHIVE]
#
# It installs debhelper 13.11.4 (the version in Debian 12, the one the
# snippets in packaging/deb/autoscripts/ come from), so it must run as root in
# a debian:12 container with network access. `make deb-check` starts one:
#
#   make deb-check                                  the x86_64 archive in dist/
#   make deb-check DEB_CHECK_ARCHIVE=path/to.tar.gz an archive of your choice
#
# ARCHIVE is a release archive (either architecture; the program is never run).
# Without one, the newest lorawan-sniffer-*-linux-*.tar.gz under dist/ and
# target/verify/ of the repository is used, if the repository is mounted whole.
#
# The check:
#   1. installs debhelper and stops unless it is version 13.11.4, and unless the
#      templates kept in autoscripts/ are byte for byte the installed ones;
#   2. builds a .deb from ARCHIVE with package-deb.sh;
#   3. builds a scratch source package with the real dh (compatibility level 13,
#      default options) from the unit that .deb ships and the same four
#      hand-written scripts, and
#   4. compares the preinst, postinst, prerm and postrm of both, byte for byte
#      (and their modes), printing a diff for any that differ.
#
# Run it again after any change in packaging/deb/ or in the assembly code of
# package-deb.sh.

set -eu

LC_ALL=C
export LC_ALL
DEBIAN_FRONTEND=noninteractive
export DEBIAN_FRONTEND

WANT_DH=13.11.4

die() {
    echo "check-autoscripts.sh: $*" >&2
    exit 1
}

[ "$(id -u)" -eq 0 ] || die "run it as root, in a debian:12 container (make deb-check)"

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
packaging=$(dirname -- "$here")
repo=$(dirname -- "$packaging")

archive=${1:-}
if [ -z "$archive" ]; then
    archive=$(find "$repo/dist" "$repo/target/verify" -name 'lorawan-sniffer-*-linux-*.tar.gz' \
        -printf '%T@ %p\n' 2>/dev/null | sort -rn | head -n 1 | cut -d ' ' -f 2-)
    [ -n "$archive" ] || die "no release archive found; pass one as the argument"
fi
[ -f "$archive" ] || die "$archive not found"

work=$(mktemp -d) || die "cannot create a temporary folder"
trap 'rm -rf "$work"' EXIT
trap 'exit 1' HUP INT TERM

echo "== Installing debhelper"
apt-get update -qq >"$work/apt.log" 2>&1 || { cat "$work/apt.log" >&2; die "apt-get update failed"; }
apt-get install -y -qq --no-install-recommends debhelper file >>"$work/apt.log" 2>&1 ||
    { cat "$work/apt.log" >&2; die "cannot install debhelper"; }
for p in debhelper libdebhelper-perl; do
    v=$(dpkg-query -W -f '${Version}' "$p")
    echo "$p $v"
    [ "$v" = "$WANT_DH" ] ||
        die "$p is $v, not $WANT_DH; the snippets in autoscripts/ are those of $WANT_DH, so compare against that version"
done

echo "== Comparing the kept templates with the installed ones"
status=0
for t in postinst-systemd-enable postinst-systemd-restart prerm-systemd-restart \
    postrm-systemd postrm-systemd-reload-only; do
    if cmp -s "$here/autoscripts/$t" "/usr/share/debhelper/autoscripts/$t"; then
        echo "same: $t"
    else
        echo "DIFFERENT: $t" >&2
        diff -u "/usr/share/debhelper/autoscripts/$t" "$here/autoscripts/$t" >&2 || true
        status=1
    fi
done

echo "== Building the package with package-deb.sh from $archive"
sh "$packaging/package-deb.sh" "$archive" "$work/out" >"$work/out.log" 2>&1 ||
    { cat "$work/out.log" >&2; die "package-deb.sh failed"; }
set -- "$work"/out/*.deb
[ $# -eq 1 ] && [ -f "$1" ] || die "package-deb.sh did not produce exactly one .deb"
deb=$1
echo "built $(basename -- "$deb")"
dpkg-deb -e "$deb" "$work/ours"
dpkg-deb -x "$deb" "$work/data"

echo "== Building the scratch package with dh $WANT_DH (compat 13, default options)"
ref=$work/ref
mkdir -p "$ref/debian"
cp "$work/data/lib/systemd/system/lorawan-sniffer.service" "$ref/debian/"
for s in preinst postinst prerm postrm; do
    cp "$here/$s" "$ref/debian/$s"
done
cat >"$ref/debian/control" <<'EOF'
Source: lorawan-sniffer
Section: net
Priority: optional
Maintainer: Scratch <scratch@example.org>
Build-Depends: debhelper-compat (= 13)

Package: lorawan-sniffer
Architecture: all
Description: scratch package for the maintainer script comparison
 Not shipped anywhere.
EOF
cat >"$ref/debian/changelog" <<'EOF'
lorawan-sniffer (0-1) unstable; urgency=medium

  * Scratch.

 -- Scratch <scratch@example.org>  Mon, 02 Nov 2026 23:04:00 +0000
EOF
printf '#!/usr/bin/make -f\n%%:\n\tdh $@\n' >"$ref/debian/rules"
chmod 0755 "$ref/debian/rules"
(cd "$ref" && debian/rules binary) >"$work/dh.log" 2>&1 ||
    { cat "$work/dh.log" >&2; die "the scratch package did not build"; }
set -- "$work"/lorawan-sniffer_*.deb
[ $# -eq 1 ] && [ -f "$1" ] || die "the scratch package did not produce exactly one .deb"
dpkg-deb -e "$1" "$work/theirs"

echo "== Comparing the maintainer scripts"
for s in preinst postinst prerm postrm; do
    if [ ! -f "$work/theirs/$s" ]; then
        echo "MISSING from the debhelper build: $s" >&2
        status=1
    elif cmp -s "$work/ours/$s" "$work/theirs/$s" &&
        [ "$(stat -c %a "$work/ours/$s")" = "$(stat -c %a "$work/theirs/$s")" ]; then
        echo "identical: $s ($(wc -c <"$work/ours/$s") bytes, mode $(stat -c %a "$work/ours/$s"))"
    else
        echo "DIFFERENT: $s (package-deb.sh on the left, debhelper on the right)" >&2
        diff -u "$work/ours/$s" "$work/theirs/$s" >&2 || true
        status=1
    fi
done
# Files in the debhelper build that package-deb.sh does not make would be
# behaviour the package lacks.
for f in "$work"/theirs/*; do
    case ${f##*/} in
    control | md5sums | conffiles | preinst | postinst | prerm | postrm) ;;
    *)
        echo "debhelper made an extra control file: ${f##*/}" >&2
        status=1
        ;;
    esac
done

if [ "$status" -eq 0 ]; then
    echo "OK: the maintainer scripts equal debhelper $WANT_DH's own output"
else
    echo "FAILED: the maintainer scripts differ from debhelper $WANT_DH's output" >&2
fi
exit "$status"
