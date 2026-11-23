#!/bin/sh
# Unpacks PipeWire 1.0.5 headers where the distribution's PipeWire headers are
# older than the pinned `pipewire` crates need, and writes PKG_CONFIG_PATH to
# $GITHUB_ENV so that the build uses them. Does nothing when the system
# headers are new enough.
#
# libspa 0.10.1 calls spa_meta_region_is_valid and spa_meta_first as inline
# functions and uses the `flags` field and the unsigned `modifier` of
# spa_video_info_raw. PipeWire 0.3.65 is the first release with all four.
# Ubuntu 22.04, which sets the glibc floor of the Linux release builds, has
# 0.3.48.
#
# Only the headers change. The call worker still links the distribution's
# libpipewire-0.3.so, so the linker checks that each PipeWire function it
# calls exists in that older library, and the packages still depend on the
# system's libpipewire-0.3-0.
#
# The packages come from the Ubuntu 24.04 release pocket, which does not
# change during that release's life. Each SHA-256 below is the one in noble's
# main/binary-amd64 Packages index, which noble's InRelease signs with the
# Ubuntu archive key (see docs/development.md). Headers do not depend on the
# architecture, so the amd64 packages also serve the arm64 build. Their files
# are unpacked, not installed.
set -eu

NEEDED=0.3.65
VERSION=1.0.5
POOL=https://archive.ubuntu.com/ubuntu/pool/main/p/pipewire
SPA_DEB=libspa-0.2-dev_1.0.5-1_amd64.deb
SPA_SHA256=6c99d162e23e13cacf8bc3edc312c60545281078398e52e34dc5ebb4209649dc
PW_DEB=libpipewire-0.3-dev_1.0.5-1_amd64.deb
PW_SHA256=c0b34ed6a34391a94c0711966dbea9b6b5a98758d88813824c05d6662e0b60c9

if pkg-config --atleast-version="$NEEDED" libspa-0.2 &&
    pkg-config --atleast-version="$NEEDED" libpipewire-0.3; then
    echo "PipeWire headers are $(pkg-config --modversion libpipewire-0.3); nothing to install"
    exit 0
fi

# The system library that the build links. Its development package must be
# installed, because the unpacked package's libpipewire-0.3.so link points to
# a library that is not unpacked.
libdir="$(pkg-config --variable=libdir libpipewire-0.3)"
if [ ! -e "$libdir/libpipewire-0.3.so" ]; then
    echo "::error::$libdir/libpipewire-0.3.so is missing; install libpipewire-0.3-dev first"
    exit 1
fi

root="${RUNNER_TEMP:?}/pipewire-headers"
rm -rf "$root"
mkdir -p "$root/download" "$root/pkgconfig"

fetch() {
    curl --proto '=https' --tlsv1.3 -fsSL -o "$root/download/$1" "$POOL/$1"
    echo "$2  $root/download/$1" | sha256sum -c -
    dpkg-deb -x "$root/download/$1" "$root"
}
fetch "$SPA_DEB" "$SPA_SHA256"
fetch "$PW_DEB" "$PW_SHA256"

includedir="$root/usr/include"
for dir in "$includedir/spa-0.2/spa" "$includedir/pipewire-0.3/pipewire"; do
    if [ ! -d "$dir" ]; then
        echo "::error::$dir is missing from the unpacked packages"
        exit 1
    fi
done

# These describe the unpacked headers and the system library together.
cat > "$root/pkgconfig/libspa-0.2.pc" <<EOF
includedir=$includedir

Name: libspa
Description: PipeWire $VERSION SPA headers, for building only
Version: $VERSION
Cflags: -I\${includedir}/spa-0.2 -D_REENTRANT
EOF

cat > "$root/pkgconfig/libpipewire-0.3.pc" <<EOF
libdir=$libdir
includedir=$includedir

Name: libpipewire
Description: PipeWire $VERSION headers with the system library, for building only
Version: $VERSION
Requires: libspa-0.2
Libs: -L\${libdir} -lpipewire-0.3
Cflags: -I\${includedir}/pipewire-0.3 -D_REENTRANT
EOF

PKG_CONFIG_PATH="$root/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export PKG_CONFIG_PATH
for lib in libspa-0.2 libpipewire-0.3; do
    if ! pkg-config --atleast-version="$NEEDED" "$lib"; then
        echo "::error::pkg-config does not find $lib $NEEDED or later after the install"
        exit 1
    fi
done
echo "PKG_CONFIG_PATH=$PKG_CONFIG_PATH" >> "$GITHUB_ENV"
echo "using PipeWire $VERSION headers from $includedir with $libdir/libpipewire-0.3.so"
