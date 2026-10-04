#!/usr/bin/env bash
# Build the binary .deb from the released sdist plus packaging/debian/, so the
# .deb and the Python artifacts share one version. Runs as
# root in a Debian 13 container:
#
#   docker run --rm -v "$PWD:/src" debian:13 \
#       /src/packaging/build-deb.sh /src/dist/devtunnel_service-X.tar.gz /src/dist/debs [VERSION]
#
# VERSION overrides the Debian version; acceptance uses it to build an older
# package to upgrade from. Lintian runs only on the regular build.
set -euo pipefail

sdist=$(realpath -- "$1")
out=$(realpath -m -- "$2")
version=${3-}
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
export DEBIAN_FRONTEND=noninteractive

apt-get update -qq
apt-get install -y -qq --no-install-recommends dpkg-dev lintian >/dev/null

upstream=$(basename -- "$sdist" .tar.gz)
upstream=${upstream#devtunnel_service-}
work=$(mktemp -d)
cp -- "$sdist" "$work/devtunnel-service_$upstream.orig.tar.gz"
mkdir "$work/devtunnel-service-$upstream"
cd "$work/devtunnel-service-$upstream"
tar -xzf "$sdist" --strip-components=1
cp -a -- "$repo/packaging/debian" .
if [[ -n $version ]]; then
	sed -i "1s/([^)]*)/($version)/" debian/changelog
fi
[[ $(dpkg-parsechangelog -S Version) == "$upstream"-* ]] ||
	{ echo "debian/changelog does not match sdist version $upstream" >&2 && exit 1; }

apt-get build-dep -y -qq ./ >/dev/null
dpkg-buildpackage -b -us -uc
if [[ -z $version ]]; then
	# Not uploaded to Debian, so there is no ITP bug to close.
	lintian --fail-on error,warning --info --display-info \
		--suppress-tags initial-upload-closes-no-bugs ../*.changes
fi
mkdir -p -- "$out"
cp -- ../*.deb "$out/"
ls -l -- "$out"
