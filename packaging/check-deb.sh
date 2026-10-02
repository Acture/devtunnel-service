#!/usr/bin/env bash
# Commands for the container are single-quoted on purpose: they expand there.
# shellcheck disable=SC2016
# Install, upgrade and removal acceptance for the .deb on one distribution and
# architecture, with a real systemd user manager in a privileged container:
#
#   packaging/check-deb.sh IMAGE OLD.deb NEW.deb
#
# IMAGE is debian:13 or ubuntu:24.04. Docker uses the host architecture; set
# DOCKER_DEFAULT_PLATFORM (e.g. linux/amd64) to emulate another one.
#
# OLD.deb is installed with networking disabled, an unprivileged user deploys
# and runs an instance against a fake devtunnel CLI, NEW.deb upgrades it
# offline, and the package is removed, purged and reinstalled while the user's
# configuration and units must survive.
set -Eeuo pipefail

image=$1
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
debs=$(mktemp -d "${TMPDIR:-/tmp}/devtunnel-debs.XXXXXX")
chmod 755 "$debs"
cp -- "$2" "$debs/old.deb"
cp -- "$3" "$debs/new.deb"
cp -- "$repo/packaging/fake-devtunnel" "$debs/"
cp -R -- "$repo/tests" "$debs/tests"
name=devtunnel-deb-$$
uid=
tag=devtunnel-service-acceptance:${image//[:\/]/-}
trap 'docker rm -f "$name" >/dev/null 2>&1; rm -rf -- "$debs"' EXIT

log() { printf '==> [%s %s] %s\n' "$image" "$(uname -m)" "$*" >&2; }
root() { docker exec "$name" "$@"; }
user() {
	docker exec --user tester --workdir /home/tester \
		--env "XDG_RUNTIME_DIR=/run/user/$uid" "$name" bash -euo pipefail -c "$1"
}
diagnose() {
	log "FAILED; recent journal entries follow"
	[[ -z $uid ]] || user 'journalctl --user --no-pager -n 80' >&2 || true
	root journalctl --no-pager -n 40 >&2 || true
}
trap diagnose ERR
installed() { root dpkg-query -W -f '${Version}' devtunnel-service; }
version_of() { root dpkg-deb --field "/debs/$1" Version; }
count() { user "grep -cF -- '$1' ~/bin/devtunnel.log 2>/dev/null || true"; }
wait_count() {
	for _ in $(seq 30); do
		(($(count "$1") >= $2)) && return 0
		sleep 1
	done
	echo "expected $2 x '$1' in the fake devtunnel log" >&2
	return 1
}

log "Preparing a systemd image"
docker build --quiet --tag "$tag" --build-arg "BASE=$image" - >/dev/null <<'EOF'
ARG BASE
FROM $BASE
ENV DEBIAN_FRONTEND=noninteractive
RUN rm -f /etc/apt/apt.conf.d/docker-clean \
 && apt-get update \
 && apt-get install -y --no-install-recommends systemd systemd-sysv dbus dbus-user-session \
 && useradd --create-home --shell /bin/bash tester
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
EOF
docker run --detach --name "$name" --privileged --cgroupns=host \
	--volume /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /run/lock \
	--volume "$debs:/debs:ro" "$tag" >/dev/null
root timeout 120 systemctl is-system-running --wait >/dev/null || true
uid=$(root id -u tester)
root loginctl enable-linger tester
for _ in $(seq 30); do
	root systemctl is-active --quiet "user@$uid.service" && break
	sleep 1
done
root systemctl is-active --quiet "user@$uid.service"

log "Installing $(version_of old.deb) with networking disabled"
root apt-get update -qq
root apt-get install -y -qq --download-only /debs/new.deb >/dev/null
docker network disconnect bridge "$name"
root apt-get install -y -qq --no-download /debs/old.deb >/dev/null
[[ $(installed) == "$(version_of old.deb)" ]]
user 'devtunnel-service --version && devtunnel-service --help >/dev/null'
user 'python3 -c "import devtunnel_service as m, sys
assert m.__file__.startswith(\"/usr/lib/python3/dist-packages/\"), m.__file__
print(\"Python\", sys.version.split()[0])"'
root cp -R /debs/tests /home/tester/tests
root chown -R tester: /home/tester/tests
user 'python3 -m unittest discover --start-directory tests --top-level-directory tests'

log "Deploying and running an instance as an unprivileged user"
user 'install -D -m 0755 /debs/fake-devtunnel ~/bin/devtunnel
deploy=(devtunnel-service deploy --name web --tunnel-id accept-tunnel --port 4000 --binary ~/bin/devtunnel)
"${deploy[@]}" --dry-run >dry-run.txt
grep -qF "ExecStart=\"/usr/bin/devtunnel-service\" host" dry-run.txt
test ! -e ~/.config/devtunnel-service
"${deploy[@]}" --start
devtunnel-service doctor --config ~/.config/devtunnel-service/web.json'
wait_count "host accept-tunnel" 1
user 'systemctl --user is-active --quiet devtunnel-web.service
systemctl --user is-enabled --quiet devtunnel-web-renew.timer
systemctl --user start devtunnel-web-renew.service'
wait_count "update accept-tunnel --expiration 30d" 1

log "Upgrading offline to $(version_of new.deb)"
root apt-get install -y -qq --no-download /debs/new.deb >/dev/null
[[ $(installed) == "$(version_of new.deb)" ]]
user 'systemctl --user is-active --quiet devtunnel-web.service
systemctl --user restart devtunnel-web.service'
wait_count "host accept-tunnel" 2
user 'systemctl --user start devtunnel-web-renew.service'
wait_count "update accept-tunnel --expiration 30d" 2

log "Removing and purging keeps the user's configuration"
user 'systemctl --user disable --now devtunnel-web.service devtunnel-web-renew.timer'
root apt-get remove -y -qq devtunnel-service >/dev/null
root test ! -e /usr/bin/devtunnel-service
root test ! -e /usr/lib/python3/dist-packages/devtunnel_service
user 'test -f ~/.config/devtunnel-service/web.json
test -f ~/.config/systemd/user/devtunnel-web.service'
root apt-get purge -y -qq devtunnel-service >/dev/null
root sh -c '! dpkg-query -W devtunnel-service 2>/dev/null'
user 'test -f ~/.config/devtunnel-service/web.json
test -f ~/.config/systemd/user/devtunnel-web-renew.timer'

log "Reinstalling resumes the existing instance"
root apt-get install -y -qq --no-download /debs/new.deb >/dev/null
user 'systemctl --user start devtunnel-web.service'
wait_count "host accept-tunnel" 3
user 'systemctl --user is-active --quiet devtunnel-web.service'
log "Debian package acceptance passed"
