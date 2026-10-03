#!/usr/bin/env bash
# Acceptance for the Python distributions. Builds the wheel and sdist from a
# throwaway copy of the source, deletes that copy, then checks both artifacts,
# uvx and uv tool from outside any checkout.
#
#   packaging/check-dist.sh [--systemd] [--out DIR]
#
# --systemd  also deploy and run real user units through the uv tool entry
#            point (Linux with a running user manager and XDG_RUNTIME_DIR).
# --out DIR  copy the artifacts that passed into DIR.
#
# Installs and removes the devtunnel-service uv tool; run it where no such tool
# exists, or point UV_TOOL_DIR and UV_TOOL_BIN_DIR at persistent scratch paths.
# PYTHON selects the interpreter for the environments (default 3.10).
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
python=${PYTHON:-3.10}
systemd=false
out=
while (($#)); do
	case $1 in
	--systemd) systemd=true ;;
	--out) out=$(mkdir -p -- "$2" && cd -- "$2" && pwd) && shift ;;
	*) echo "unknown argument: $1" >&2 && exit 2 ;;
	esac
	shift
done

log() { printf '==> %s\n' "$*" >&2; }
fail() {
	printf 'FAIL: %s\n' "$*" >&2
	exit 1
}
expect() { grep -qF -- "$1" "$2" || fail "expected '$1' in $2: $(cat -- "$2")"; }
empty() { [[ -z $(ls -A -- "$1") ]] || fail "$1 is not empty: $(ls -A -- "$1")"; }

work=$(mktemp -d "${TMPDIR:-/tmp}/devtunnel-service.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
version=$(sed -n 's/^__version__ = "\(.*\)"$/\1/p' "$repo/src/devtunnel_service/__init__.py")
[[ -n $version ]] || fail "cannot read the package version"
deploy=(deploy --name smoke --tunnel-id smoke-tunnel --port 4000 --binary /bin/sh)

log "Building $version from a copy of the source tree"
mkdir "$work/source"
tar -C "$repo" --exclude=.git --exclude=.venv --exclude=dist --exclude=target -cf - . | tar -xf - -C "$work/source"
uv build --quiet --out-dir "$work/dist" "$work/source"
rm -rf -- "$work/source"
wheel=$work/dist/devtunnel_service-$version-py3-none-any.whl
sdist=$work/dist/devtunnel_service-$version.tar.gz
[[ -f $wheel && -f $sdist ]] || fail "missing artifacts: $(ls "$work/dist")"
# The suite ships in the sdist; run it against each installed artifact.
tar -xzf "$sdist" -C "$work" --strip-components=1 "devtunnel_service-$version/tests"
cd -- "$work"

check_environment() {
	local env=$1
	[[ $("$env/bin/devtunnel-service" --version) == "devtunnel-service $version" ]] || fail "version"
	"$env/bin/devtunnel-service" --help >"$work/stdout"
	expect deploy "$work/stdout"
	"$env/bin/python" -c 'import devtunnel_service, sys
assert devtunnel_service.__file__.startswith(sys.prefix), devtunnel_service.__file__'
	"$env/bin/python" -m unittest discover --start-directory "$work/tests" \
		--top-level-directory "$work/tests" 2>"$work/stderr" || fail "unit tests: $(cat "$work/stderr")"
	mkdir -p "$work/xdg"
	XDG_CONFIG_HOME=$work/xdg "$env/bin/devtunnel-service" "${deploy[@]}" --dry-run \
		--entry-point /usr/bin/devtunnel-service >"$work/stdout" 2>"$work/stderr"
	expect 'ExecStart="/usr/bin/devtunnel-service" host --config' "$work/stdout"
	[[ ! -s $work/stderr ]] || fail "dry run stderr: $(cat "$work/stderr")"
	empty "$work/xdg"
}

for artifact in "$wheel" "$sdist"; do
	log "Installing $(basename "$artifact") with Python $python"
	env=$work/env-${artifact##*.}
	uv venv --quiet --python "$python" "$env"
	uv pip install --quiet --python "$env/bin/python" "$artifact"
	check_environment "$env"
done

log "uvx runs ephemerally and refuses to reference its cache from units"
uvx() { command uvx --python "$python" --from "$wheel" devtunnel-service "$@"; }
[[ $(uvx --version) == "devtunnel-service $version" ]] || fail "uvx version"
mkdir -p "$work/xdg"
XDG_CONFIG_HOME=$work/xdg uvx "${deploy[@]}" --dry-run >"$work/stdout" 2>"$work/stderr"
expect 'deployment would refuse this entry point' "$work/stderr"
if XDG_CONFIG_HOME=$work/xdg uvx "${deploy[@]}" 2>"$work/stderr"; then
	fail "uvx deployment accepted its cache entry point"
fi
expect 'Refusing a non-persistent entry point' "$work/stderr"
empty "$work/xdg"

log "uv tool provides a persistent entry point"
if uv tool list 2>/dev/null | grep -q '^devtunnel-service '; then
	fail "a devtunnel-service uv tool is already installed; use a clean UV_TOOL_DIR"
fi
instance=acceptance
fake=$HOME/.local/share/devtunnel-service-acceptance/devtunnel
units=$HOME/.config/systemd/user/devtunnel-$instance
config=$HOME/.config/devtunnel-service/$instance.json
cleanup() {
	if $systemd; then
		systemctl --user disable --now "devtunnel-$instance.service" \
			"devtunnel-$instance-renew.timer" >/dev/null 2>&1 || true
		rm -f -- "$units"* "$config"*
		rm -rf -- "$(dirname -- "$fake")"
		systemctl --user daemon-reload || true
	fi
	uv tool uninstall devtunnel-service >/dev/null 2>&1 || true
	rm -rf -- "$work"
}
if $systemd && { compgen -G "$units*" >/dev/null || [[ -e $config ]]; }; then
	fail "an instance named $instance already exists; refusing to touch it"
fi
trap cleanup EXIT
uv tool install --quiet --python "$python" "$wheel"
entry=$(uv tool dir --bin)/devtunnel-service
[[ $("$entry" --version) == "devtunnel-service $version" ]] || fail "uv tool version"
# The units must reference the command found on PATH, as for a user's shell.
PATH=$(dirname -- "$entry"):$PATH
XDG_CONFIG_HOME=$work/xdg devtunnel-service "${deploy[@]}" --dry-run >"$work/stdout" 2>"$work/stderr"
expect "ExecStart=\"$entry\" host --config" "$work/stdout"
[[ ! -s $work/stderr ]] || fail "uv tool entry point was rejected: $(cat "$work/stderr")"
empty "$work/xdg"

if $systemd; then
	log "Running user units through $entry"
	install -D -m 0755 "$repo/packaging/fake-devtunnel" "$fake"
	wait_for() {
		local n
		for _ in $(seq 30); do
			n=$(grep -cF -- "$1" "$fake.log" 2>/dev/null || true)
			((${n:-0} >= $2)) && return 0
			sleep 1
		done
		systemctl --user status "devtunnel-$instance.service" >&2 || true
		journalctl --user -u "devtunnel-$instance.service" --no-pager >&2 || true
		fail "expected $2 x '$1' in $fake.log"
	}
	devtunnel-service deploy --name "$instance" --tunnel-id acceptance-tunnel --port 4000 \
		--binary "$fake" --start
	expect "ExecStart=\"$entry\" host" "$units.service"
	wait_for "host acceptance-tunnel" 1
	systemctl --user is-active --quiet "devtunnel-$instance.service" || fail "host is not active"
	systemctl --user is-enabled --quiet "devtunnel-$instance-renew.timer" || fail "timer not enabled"
	systemctl --user start "devtunnel-$instance-renew.service"
	wait_for "update acceptance-tunnel --expiration 30d" 1
	log "Reinstalling the uv tool keeps the units' entry point valid"
	uv tool install --quiet --force --reinstall --python "$python" "$wheel"
	systemctl --user restart "devtunnel-$instance.service"
	wait_for "host acceptance-tunnel" 2
	systemctl --user is-active --quiet "devtunnel-$instance.service" || fail "host inactive after reinstall"
	systemctl --user start "devtunnel-$instance-renew.service"
	wait_for "update acceptance-tunnel --expiration 30d" 2
fi

if [[ -n $out ]]; then
	cp -- "$wheel" "$sdist" "$out/"
	log "Copied the accepted artifacts to $out"
fi
log "Distribution acceptance passed"
