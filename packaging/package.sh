#!/bin/sh
# Build a release archive from a cross-compiled binary.
#
#   packaging/package.sh TARGET [OUTDIR]
#
#   TARGET  x86_64-unknown-linux-musl or aarch64-unknown-linux-musl; the binary
#           is taken from target/TARGET/release/lorawan-sniffer
#   OUTDIR  where to write the results (default: dist)
#
# Writes:
#
#   OUTDIR/lorawan-sniffer-VERSION-linux-ARCH.tar.gz
#       ARCH as `uname -m` prints it on the target (x86_64, aarch64), so the
#       right archive is easy to pick from a shell on the target machine
#   OUTDIR/docker/PLATFORM/lorawan-sniffer
#       the same binary under its Docker architecture name (amd64, arm64),
#       where docker/Dockerfile.release looks for it
#
# VERSION is read from lorawan-sniffer/Cargo.toml. Run from the repository
# root. The archive is reproducible: fixed order, owner and timestamps (the
# last commit's time, or SOURCE_DATE_EPOCH when set).

set -eu

die() {
    echo "package.sh: $*" >&2
    exit 1
}

target=${1:-}
out=${2:-dist}

case $target in
    x86_64-unknown-linux-musl)
        arch=x86_64 docker_arch=amd64 file_arch=x86-64 ;;
    aarch64-unknown-linux-musl)
        arch=aarch64 docker_arch=arm64 file_arch=aarch64 ;;
    *)
        die "usage: packaging/package.sh {x86_64|aarch64}-unknown-linux-musl [OUTDIR]" ;;
esac

[ -f lorawan-sniffer/Cargo.toml ] || die "run from the repository root"
bin=target/$target/release/lorawan-sniffer
[ -f "$bin" ] || die "$bin not found; build it first (make build-all)"

version=$(sed -n 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
    lorawan-sniffer/Cargo.toml | head -n 1)
[ -n "$version" ] || die "cannot read the version from lorawan-sniffer/Cargo.toml"

# Refuse a binary that would not work where the archive says it does.
info=$(file -b "$bin")
case $info in
    *"$file_arch"*) ;;
    *) die "$bin is not an $arch binary: $info" ;;
esac
case $info in
    *"statically linked"* | *"static-pie linked"*) ;;
    *) die "$bin is not statically linked, so it would depend on the target's libraries: $info" ;;
esac
# Without `npm run build` first, only a placeholder page gets embedded and
# the release would serve a blank WebUI.
grep -a -q 'assets/index-' "$bin" ||
    die "$bin has no WebUI embedded; run 'npm run build' in webui/ and rebuild"

name=lorawan-sniffer-$version-linux-$arch
stage=$out/$name
rm -rf "$stage"
mkdir -p "$stage"

install -m 0755 "$bin" "$stage/lorawan-sniffer"
install -m 0755 packaging/install.sh "$stage/install.sh"
for f in packaging/sniffer.toml packaging/sniffer.env \
    packaging/lorawan-sniffer.service packaging/concentratord.conf \
    README.md CHANGELOG.md; do
    install -m 0644 "$f" "$stage/"
done
if [ -f LICENSE ]; then
    install -m 0644 LICENSE "$stage/"
fi

epoch=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$epoch" \
    -C "$out" -cf - "$name" | gzip -n -9 >"$out/$name.tar.gz"
rm -rf "$stage"

mkdir -p "$out/docker/$docker_arch"
install -m 0755 "$bin" "$out/docker/$docker_arch/lorawan-sniffer"

echo "$out/$name.tar.gz"
