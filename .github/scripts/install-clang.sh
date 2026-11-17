#!/bin/sh
# Installs clang 21 where the distribution's own clang is older than the
# hermetic libc++ of the pinned libwebrtc needs, and writes CXX to
# $GITHUB_ENV, so that tools/dev.py builds the call worker (ADR-0043).
# Does nothing when the default clang++ is new enough.
#
# Each extra repository is trusted only through its public signing key,
# committed in .github/keys and checked by fingerprint in
# docs/development.md; no key is downloaded here.
#   Debian, Ubuntu: apt.llvm.org, llvm-toolchain-<codename>-21
#   openSUSE Leap:  openSUSE Build Service devel:tools:compiler
set -eu

NEEDED=21
keys="$(cd "$(dirname "$0")/../keys" && pwd)"
sudo=""
[ "$(id -u)" -eq 0 ] || sudo=sudo

major() {
    "$1" -dumpversion 2>/dev/null | cut -d. -f1
}

have="$(major clang++ || true)"
if [ -n "$have" ] && [ "$have" -ge "$NEEDED" ]; then
    echo "clang++ is clang $have; nothing to install"
    exit 0
fi

# shellcheck source=/dev/null
. /etc/os-release
case "$ID" in
    debian | ubuntu)
        $sudo install -m 0644 "$keys/apt.llvm.org.asc" /usr/share/keyrings/apt.llvm.org.asc
        printf 'Types: deb\nURIs: https://apt.llvm.org/%s/\nSuites: llvm-toolchain-%s-%s\nComponents: main\nSigned-By: /usr/share/keyrings/apt.llvm.org.asc\n' \
            "$VERSION_CODENAME" "$VERSION_CODENAME" "$NEEDED" | $sudo tee /etc/apt/sources.list.d/apt.llvm.org.sources >/dev/null
        $sudo apt-get update
        $sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "clang-$NEEDED"
        cxx="$(command -v "clang++-$NEEDED")"
        ;;
    opensuse-leap)
        $sudo rpm --import "$keys/devel-tools-compiler.asc"
        # Priority 120 is below the distribution's 99, so only packages that
        # the distribution lacks, such as clang21, come from this repository.
        $sudo zypper --non-interactive addrepo --gpgcheck --priority 120 \
            "https://download.opensuse.org/repositories/devel:/tools:/compiler/$VERSION_ID/" devel-tools-compiler
        $sudo zypper --non-interactive refresh devel-tools-compiler
        $sudo zypper --non-interactive install "clang$NEEDED"
        cxx="$(rpm -ql "clang$NEEDED" | grep -E "/bin/clang\+\+(-$NEEDED)?$" | head -n 1)"
        ;;
    *)
        echo "::error::clang ${have:-none} is older than clang $NEEDED, and no clang $NEEDED source is set up for $ID"
        exit 1
        ;;
esac

found="$(major "$cxx" || true)"
if [ "$found" != "$NEEDED" ]; then
    echo "::error::installed ${cxx:-no compiler}, which is clang ${found:-unknown}, not clang $NEEDED"
    exit 1
fi
echo "CXX=$cxx" >> "$GITHUB_ENV"
echo "using $cxx (clang $found)"
