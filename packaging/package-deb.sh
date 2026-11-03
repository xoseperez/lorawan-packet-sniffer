#!/bin/sh
# Build a Debian package from a release archive.
#
#   packaging/package-deb.sh ARCHIVE [OUTDIR]
#   packaging/package-deb.sh --version VERSION
#
#   ARCHIVE  lorawan-sniffer-VERSION-linux-x86_64.tar.gz or
#            lorawan-sniffer-VERSION-linux-aarch64.tar.gz, as package.sh
#            writes them
#   OUTDIR   where to write the result (default: the archive's folder)
#   --version  print the Debian version of a release version and stop:
#              2.1.0 -> 2.1.0-1, and 2.2.0-rc.1 -> 2.2.0~rc.1-1, which sorts
#              before 2.2.0-1 (a plain "-" would sort after it)
#
# Writes OUTDIR/lorawan-sniffer_VERSION-1_ARCH.deb, with ARCH as dpkg names it
# (amd64, arm64), and prints its path. The file name keeps the release version
# (lorawan-sniffer_2.2.0-rc.1-1_amd64.deb) because GitHub rewrites "~" in asset
# names; dpkg reads the version from the package's control data, not from the
# name.
#
# The package wraps the archive's files: the binary byte for byte, the unit
# with only its ExecStart path changed, and the sample configuration files as
# conffiles. Its control data and maintainer scripts are in packaging/deb/. The
# systemd parts of the maintainer scripts are debhelper's own snippets, filled
# in here the way dh_installsystemd fills them in (packaging/deb/autoscripts/);
# packaging/deb/check-autoscripts.sh compares the result with a real
# dh_installsystemd run.
#
# The package is reproducible: building the same archive twice with the same
# tools gives the same bytes. Every timestamp is SOURCE_DATE_EPOCH, taken from
# the environment or else from the files in the archive (package.sh sets them
# to the commit time). Owners, modes and the order of the files are fixed, and
# the compressor runs on one thread, because the number of threads changes the
# compressed bytes.
#
# Needs GNU tar, find, date and stat, and dpkg-deb from dpkg 1.21 or newer.

set -eu

# Modes below are set explicitly; the umask only covers what is not.
umask 022
LC_ALL=C
export LC_ALL

PKG=lorawan-sniffer
# The debhelper release whose snippets packaging/deb/autoscripts/ holds.
DH_VERSION=13.11.4

die() {
    echo "package-deb.sh: $*" >&2
    exit 1
}

usage() {
    die "usage: packaging/package-deb.sh ARCHIVE [OUTDIR]
       packaging/package-deb.sh --version VERSION"
}

# The Debian version of a release version (see the top of this file).
debian_version() {
    case $1 in
    '' | *[!0-9A-Za-z.-]*) return 1 ;;
    esac
    printf '%s\n' "$1" |
        grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || return 1
    case $1 in
    *-*) printf '%s~%s-1\n' "${1%%-*}" "${1#*-}" ;;
    *) printf '%s-1\n' "$1" ;;
    esac
}

case ${1:-} in
--version)
    [ $# -eq 2 ] || usage
    debian_version "$2" ||
        die "'$2' is not a release version (expected N.N.N or N.N.N-prerelease)"
    exit 0
    ;;
'' | -*) usage ;;
esac
[ $# -le 2 ] || usage

archive=$1
base=${archive##*/}
case $base in
lorawan-sniffer-*-linux-x86_64.tar.gz)
    arch=x86_64 debarch=amd64 file_arch=x86-64 ;;
lorawan-sniffer-*-linux-aarch64.tar.gz)
    arch=aarch64 debarch=arm64 file_arch=aarch64 ;;
*)
    die "$base is not a release archive (lorawan-sniffer-VERSION-linux-x86_64.tar.gz or -aarch64.tar.gz)" ;;
esac
[ -f "$archive" ] || die "$archive not found"
out=${2:-$(dirname -- "$archive")}

name=${base%.tar.gz}
version=${name#lorawan-sniffer-}
version=${version%-linux-"$arch"}
debversion=$(debian_version "$version") ||
    die "the archive name has '$version' as its version, which is not N.N.N or N.N.N-prerelease"
debfile=${PKG}_${version}-1_$debarch.deb

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
debdir=$here/deb
autodir=$debdir/autoscripts
for f in control conffiles copyright lintian-overrides preinst postinst prerm \
    postrm autoscripts/SHA256SUMS; do
    [ -f "$debdir/$f" ] || die "$debdir/$f not found"
done

for tool in dpkg-deb file gzip sha256sum md5sum tar find stat date touch install cmp awk; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found"
done
# Without it the compressor picks the thread count from the machine, and the
# output differs from one machine to the next.
dpkg-deb --help 2>&1 | grep -q -- '--threads-max' ||
    die "this dpkg-deb has no --threads-max (dpkg 1.21 or newer is needed); without it the package would not be reproducible"

work=$(mktemp -d "${TMPDIR:-/tmp}/package-deb.XXXXXX") ||
    die "cannot create a temporary folder"
trap 'rm -rf "$work"' EXIT
trap 'exit 1' HUP INT TERM

# --- Unpack the archive -------------------------------------------------------

tar -tzf "$archive" >"$work/entries" || die "cannot read $archive"
# Everything must be inside the archive's own folder.
awk -v dir="$name/" '
    index($0, dir) != 1 || $0 ~ /(^|\/)\.\.(\/|$)/ { bad = 1 }
    END { exit bad }' "$work/entries" ||
    die "$base has files outside $name/"
mkdir "$work/src"
tar -xzf "$archive" -C "$work/src" --no-same-owner --no-same-permissions ||
    die "cannot unpack $archive"
src=$work/src/$name

for f in lorawan-sniffer lorawan-sniffer.service sniffer.toml sniffer.env \
    concentratord.conf README.md CHANGELOG.md; do
    [ -f "$src/$f" ] && [ ! -L "$src/$f" ] || die "$base has no $f"
done

epoch=${SOURCE_DATE_EPOCH:-}
[ -n "$epoch" ] || epoch=$(stat -c %Y "$src/lorawan-sniffer")
case $epoch in
'' | *[!0-9]*) die "SOURCE_DATE_EPOCH must be a whole number of seconds (got '$epoch')" ;;
esac
SOURCE_DATE_EPOCH=$epoch
export SOURCE_DATE_EPOCH

# --- Check what goes in -------------------------------------------------------

# The same checks as package.sh makes: refuse a binary that would not work
# where the package says it does.
info=$(file -b "$src/lorawan-sniffer")
case $info in
*"$file_arch"*) ;;
*) die "the binary in $base is not an $arch binary: $info" ;;
esac
case $info in
*"statically linked"* | *"static-pie linked"*) ;;
*) die "the binary in $base is not statically linked, so it would depend on the target's libraries: $info" ;;
esac

# The shipped configuration is a conffile that every install gets: it must not
# carry a login token or turn the login on.
if grep -Eq '^[[:space:]]*(login[[:space:]]*=[[:space:]]*true|token[[:space:]]*=[[:space:]]*"[^"])' \
    "$src/sniffer.toml"; then
    die "sniffer.toml in $base turns the login on or carries a token"
fi

# The copyright file repeats the licence; catch a change of year or holder in
# the project's LICENSE that it does not follow.
if [ -f "$src/LICENSE" ]; then
    holder=$(sed -n 's/^Copyright (c) //p' "$src/LICENSE" | head -n 1)
    [ -n "$holder" ] || die "cannot read the copyright line from LICENSE in $base"
    grep -qF -- "$holder" "$debdir/copyright" ||
        die "packaging/deb/copyright does not say '$holder', as LICENSE does"
fi

# The homepage is the unit's Documentation= address.
[ "$(grep -c '^Documentation=' "$src/lorawan-sniffer.service")" -eq 1 ] ||
    die "the unit must have exactly one Documentation= line"
homepage=$(sed -n 's/^Documentation=//p' "$src/lorawan-sniffer.service")
case $homepage in
https://*[!A-Za-z0-9./:_~%+-]* | '' | *' '*) die "Documentation= in the unit is not a plain https address: '$homepage'" ;;
https://*) ;;
*) die "Documentation= in the unit is not a plain https address: '$homepage'" ;;
esac

# The archive's unit runs the program from /usr/local/bin, where install.sh
# puts it. In the package it lives in /usr/bin. This is the only change, and
# it must hit exactly one line: a second ExecStart, or any other /usr/local
# path, would be left pointing at a place the package does not use.
unit_in=$src/lorawan-sniffer.service
[ "$(grep -c '^ExecStart=' "$unit_in")" -eq 1 ] &&
    [ "$(grep -c '^ExecStart=/usr/local/bin/lorawan-sniffer ' "$unit_in")" -eq 1 ] ||
    die "the unit must have exactly one ExecStart=/usr/local/bin/lorawan-sniffer line"
sed 's|^ExecStart=/usr/local/bin/lorawan-sniffer |ExecStart=/usr/bin/lorawan-sniffer |' \
    "$unit_in" >"$work/unit"
if grep -q '/usr/local' "$work/unit"; then
    die "the unit still mentions /usr/local after the ExecStart change"
fi

# --- Stage the files ----------------------------------------------------------

stage=$work/stage
doc=$stage/usr/share/doc/$PKG
install -d -m 0755 "$stage" "$stage/DEBIAN" "$stage/usr/bin" \
    "$stage/lib/systemd/system" "$stage/etc/$PKG" "$doc/examples" \
    "$stage/usr/share/lintian/overrides"

install -m 0755 "$src/lorawan-sniffer" "$stage/usr/bin/$PKG"
install -m 0644 "$work/unit" "$stage/lib/systemd/system/$PKG.service"
install -m 0644 "$src/sniffer.toml" "$stage/etc/$PKG/sniffer.toml"
# Secrets: readable by root only, and systemd reads it as root.
install -m 0600 "$src/sniffer.env" "$stage/etc/$PKG/sniffer.env"

# gzip -n leaves the name and the time out of the file.
for pair in README.md:README.md.gz CHANGELOG.md:changelog.gz; do
    gzip -9n <"$src/${pair%%:*}" >"$work/${pair##*:}"
    install -m 0644 "$work/${pair##*:}" "$doc/${pair##*:}"
done
install -m 0644 "$debdir/copyright" "$doc/copyright"
install -m 0644 "$src/concentratord.conf" "$doc/examples/concentratord.conf"
install -m 0644 "$debdir/lintian-overrides" "$stage/usr/share/lintian/overrides/$PKG"

# The package is not native (its version has a revision), so it needs a Debian
# changelog. It has one entry.
maintainer=$(sed -n 's/^Maintainer: //p' "$debdir/control")
[ -n "$maintainer" ] || die "packaging/deb/control has no Maintainer"
{
    printf '%s (%s) unstable; urgency=medium\n\n' "$PKG" "$debversion"
    printf '  * Built from the release archive\n    %s.\n\n' "$base"
    printf ' -- %s  %s\n' "$maintainer" \
        "$(date -u -d "@$epoch" '+%a, %d %b %Y %H:%M:%S +0000')"
} | gzip -9n >"$work/changelog.Debian.gz"
install -m 0644 "$work/changelog.Debian.gz" "$doc/changelog.Debian.gz"

# --- Maintainer scripts -------------------------------------------------------

# debhelper's snippets, checked against the sums kept with them.
templates="postinst-systemd-enable postinst-systemd-restart prerm-systemd-restart postrm-systemd postrm-systemd-reload-only"
for t in $templates; do
    want=$(awk -v f="$t" '$2 == f { print $1 }' "$autodir/SHA256SUMS")
    have=$(sha256sum <"$autodir/$t" | awk '{ print $1 }')
    [ -n "$want" ] && [ "$want" = "$have" ] ||
        die "packaging/deb/autoscripts/$t differs from debhelper $DH_VERSION (see SHA256SUMS)"
done

# What dh_installsystemd writes for one unit that has an [Install] section, at
# its default options (enable, start, restart after an upgrade): the unit name
# in single quotes in place of both unit placeholders, "restart" as the action,
# and the marker lines debhelper puts around each snippet.
unit_q="'$PKG.service'"
snippet() { # template
    printf '# Automatically added by dh_installsystemd/%s\n' "$DH_VERSION"
    sed -e "s/#UNITFILES#/$unit_q/g" -e "s/#UNITFILE#/$unit_q/g" \
        -e 's/#RESTART_ACTION#/restart/g' "$autodir/$1"
    printf '# End automatically added section\n'
}

# The snippets for one maintainer script, in debhelper's order. Snippets for
# postinst come in the order they were added; those for prerm and postrm are
# put in front of what is there, so the one added later comes first. There is
# no stop-before-upgrade snippet for preinst, because the service is restarted
# after the upgrade instead.
fragment() { # script
    case $1 in
    preinst) ;;
    postinst)
        snippet postinst-systemd-enable
        snippet postinst-systemd-restart
        ;;
    prerm) snippet prerm-systemd-restart ;;
    postrm)
        snippet postrm-systemd-reload-only
        snippet postrm-systemd
        ;;
    esac
}

# Replace the line holding only the debhelper token with the snippets, as
# debhelper does: the text goes in place of the token, and the line's own
# newline stays after it.
assemble() { # script
    script=$debdir/$1
    [ -z "$(tail -c 1 "$script")" ] ||
        die "packaging/deb/$1 does not end with a newline"
    [ "$(grep -c -x '#DEBHELPER#' "$script")" -eq 1 ] ||
        die "packaging/deb/$1 must have the debhelper token on a line of its own, once"
    # debhelper replaces other #WORD# tokens as well (#PACKAGE#, for example).
    [ "$(grep -c -E '#[A-Za-z0-9_.+]+#' "$script")" -eq 1 ] ||
        die "packaging/deb/$1 has a #WORD# token besides the debhelper one"
    fragment "$1" >"$work/fragment.$1"
    if grep -q -E '#[A-Z_]+#' "$work/fragment.$1"; then
        die "a placeholder is left in the snippets for $1"
    fi
    while IFS= read -r line; do
        if [ "$line" = '#DEBHELPER#' ]; then
            cat "$work/fragment.$1"
            echo
        else
            printf '%s\n' "$line"
        fi
    done <"$script" >"$stage/DEBIAN/$1"
    chmod 0755 "$stage/DEBIAN/$1"
    sh -n "$stage/DEBIAN/$1" || die "the assembled $1 is not valid shell"
}
for s in preinst postinst prerm postrm; do
    assemble "$s"
done

install -m 0644 "$debdir/conffiles" "$stage/DEBIAN/conffiles"

# --- Control data -------------------------------------------------------------

# Installed-Size as dpkg-gencontrol works it out for a debhelper build: each
# regular file and symlink counts its size in KiB rounded up, every other
# object (directories, including the top one) counts 1, and the tree includes
# DEBIAN with the scripts and conffiles, which exist by now, but not yet the
# control and md5sums files.
installed_size=$(find "$stage" -printf '%y %s\n' | awk '
    $1 == "f" || $1 == "l" { n += int(($2 + 1023) / 1024); next }
    { n++ }
    END { print n + 0 }')

sed -e "s|@VERSION@|$debversion|" -e "s|@ARCH@|$debarch|" \
    -e "s|@INSTALLED_SIZE@|$installed_size|" -e "s|@HOMEPAGE@|$homepage|" \
    "$debdir/control" >"$stage/DEBIAN/control"
if grep -q '@[A-Z_]*@' "$stage/DEBIAN/control"; then
    die "packaging/deb/control has a placeholder that was not filled in"
fi
chmod 0644 "$stage/DEBIAN/control"

# Conffiles are listed too, as the package's own record of what it shipped.
(
    cd "$stage"
    find . -path ./DEBIAN -prune -o -type f -print | sed 's|^\./||' | sort |
        while IFS= read -r f; do
            md5sum -- "$f"
        done
) >"$stage/DEBIAN/md5sums"
chmod 0644 "$stage/DEBIAN/md5sums"

# --- Build --------------------------------------------------------------------

# dpkg-deb only lowers timestamps that are newer than SOURCE_DATE_EPOCH, so set
# them all to it. Directories get their mode last; nothing is written after
# this.
find "$stage" -type d -exec chmod 0755 {} +
find "$stage" -exec touch -h -d "@$epoch" {} +

deb=$work/$debfile
dpkg-deb --root-owner-group -Zxz --threads-max=1 --build "$stage" "$deb" >&2 ||
    die "dpkg-deb failed"

# --- Check what came out ------------------------------------------------------

field() { dpkg-deb -f "$deb" "$1"; }
[ "$(field Package)" = "$PKG" ] || die "the package is not called $PKG"
[ "$(field Version)" = "$debversion" ] ||
    die "the package version is '$(field Version)', not $debversion"
[ "$(field Architecture)" = "$debarch" ] ||
    die "the package architecture is '$(field Architecture)', not $debarch"
[ "$(field Homepage)" = "$homepage" ] || die "the package homepage is wrong"

want=$(sha256sum <"$src/lorawan-sniffer")
have=$(dpkg-deb --fsys-tarfile "$deb" | tar -xOf - "./usr/bin/$PKG" | sha256sum)
[ "$want" = "$have" ] || die "the binary in the package differs from the one in $base"

dpkg-deb --ctrl-tarfile "$deb" | tar -xOf - ./conffiles >"$work/conffiles.out" ||
    die "the package has no conffiles member"
cmp -s "$work/conffiles.out" "$debdir/conffiles" ||
    die "the conffiles in the package are not the two configuration files"

dpkg-deb -c "$deb" >"$work/listing"
awk '
    $2 != "root/root" { bad = 1 }
    $6 == "./etc/lorawan-sniffer/sniffer.toml" && $1 == "-rw-r--r--" { toml = 1 }
    $6 == "./etc/lorawan-sniffer/sniffer.env" && $1 == "-rw-------" { env = 1 }
    END { exit (bad || !toml || !env) }' "$work/listing" ||
    die "an entry is not owned by root, or a conffile has the wrong mode (sniffer.toml 0644, sniffer.env 0600)"

mkdir -p "$out"
install -m 0644 "$deb" "$out/$debfile.tmp.$$"
mv -f "$out/$debfile.tmp.$$" "$out/$debfile"

echo "$out/$debfile"
