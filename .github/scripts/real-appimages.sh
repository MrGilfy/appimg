#!/bin/sh
# Runs a built appimg against real AppImages on GitHub, in a throwaway XDG
# home: ImHex 1.38.0 installed from its release URL and updated to ImHex's
# newest release through appimg's own zsync delta, then a github: source
# whose newest release ships no AppImage. Kept to a handful of requests on
# purpose.
#
# ImHex is the build for the machine this runs on, x86_64 or arm64, since
# appimg runs the AppImage it installs and picks the zsync file for its own
# architecture.
#
# GITHUB_TOKEN, when set, is used for the two API requests this script makes
# itself, and appimg sends it with its own requests to api.github.com.
set -eu

usage() {
	echo "usage: ${0##*/} <appimg binary>" >&2
	exit 2
}

[ $# -eq 1 ] || usage
appimg=$(realpath "$1")
[ -x "$appimg" ] || usage

fail() {
	# The annotation shows up on the run summary, and reads fine locally.
	echo "::error::$*" >&2
	exit 1
}

api() {
	if [ -n "${GITHUB_TOKEN:-}" ]; then
		curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" "https://api.github.com/$1"
	else
		curl -fsSL "https://api.github.com/$1"
	fi
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export HOME="$work/home" XDG_DATA_HOME="$work/data" XDG_CONFIG_HOME="$work/config"
export TMPDIR="$work/tmp"
unset APPIMG_DIR
mkdir -p "$HOME" "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$TMPDIR"

# --- ImHex: a delta update from 1.38.0 to its newest release --------------

from=1.38.0

# The name ImHex gives the build for this machine, in its file names and in
# those of their zsync files.
case $(uname -m) in
x86_64) arch=x86_64 ;;
aarch64 | arm64) arch=arm64 ;;
*) fail "ImHex ships no AppImage for $(uname -m)" ;;
esac

# ImHex's update information follows `latest`, so the update has to end at
# its newest release that is neither a draft nor a pre-release and ships an
# AppImage for this machine. Whichever that is, the sha256 GitHub publishes
# for that AppImage is what the result has to match, so nothing here goes
# stale, and a zsync file for another architecture shows up as a mismatch.
api "repos/WerWolv/ImHex/releases?per_page=30" >"$work/imhex-releases.json"
appimage="^imhex-.*-$arch[.]AppImage\$"
# shellcheck disable=SC2016 # $appimage is a jq variable.
newest='[.[] | select((.draft or .prerelease) | not) | select(any(.assets[]; .name | test($appimage)))][0]'
tag=$(jq -r --arg appimage "$appimage" "$newest | .tag_name" "$work/imhex-releases.json")
expected=$(jq -r --arg appimage "$appimage" \
	"$newest | first(.assets[] | select(.name | test(\$appimage))) | .digest" "$work/imhex-releases.json")
[ -n "$tag" ] && [ "$tag" != null ] || fail "ImHex has no release with an $arch AppImage"
to=${tag#v}
[ "$to" != "$from" ] || fail "ImHex's newest release is $from itself, there is nothing to update to"
case $expected in
sha256:*) expected=${expected#sha256:} ;;
*) fail "GitHub publishes no sha256 for the $arch AppImage of ImHex $tag: $expected" ;;
esac
echo "ImHex $from for $arch, updated to its newest release, $to"

"$appimg" --yes --no-color install --name ImHex \
	"https://github.com/WerWolv/ImHex/releases/download/v$from/imhex-$from-$arch.AppImage"

"$appimg" --no-color update imhex >"$work/update.log" 2>&1 || {
	cat "$work/update.log"
	fail "the update of ImHex failed"
}
cat "$work/update.log"

version=$("$appimg" list --json | jq -r '.[] | select(.slug == "imhex") | .version')
[ "$version" = "$to" ] || fail "ImHex is at $version after the update, not $to"

# The line every delta update prints. A full download after a failed delta,
# or a server that ignored the ranges, prints something else.
grep -Eq 'reused [1-9][0-9]* of [0-9]+ blocks' "$work/update.log" ||
	fail "the update did not apply a zsync delta that reused blocks"

actual=$(sha256sum "$XDG_DATA_HOME/appimages/imhex.AppImage" | cut -d' ' -f1)
[ "$actual" = "$expected" ] ||
	fail "the updated ImHex is $actual, the published $to is $expected"
echo "ImHex $to, sha256 $actual, as published"

# --- A github: source whose newest release has no AppImage ----------------

repository=obsidianmd/obsidian-releases
echo "github:$repository"

# What makes this check worth anything is a newest release without an
# AppImage. When that changes, it still passes, and says it proves less.
api "repos/$repository/releases?per_page=30" >"$work/releases.json"
appimages=$(jq -r '[.[] | select((.draft or .prerelease) | not)][0] |
	[.assets[].name | select(endswith(".AppImage"))] | length' "$work/releases.json")
[ "$appimages" -eq 0 ] ||
	echo "::warning::the newest release of $repository has an AppImage now, so this check no longer covers a release without one"

# Nothing of Obsidian is downloaded: a stand-in under a name like the real
# one is enough for appimg to pick the matching file out of each release.
payload="$work/payload"
mkdir -p "$payload"
printf '[Desktop Entry]\nType=Application\nName=Obsidian\nExec=AppRun\nIcon=obsidian\nCategories=Office;\n' \
	>"$payload/obsidian.desktop"
stand_in="$work/Obsidian-1.0.0.AppImage"
# shellcheck disable=SC2016 # $1 belongs to the stand-in, not to this script.
printf '#!/bin/sh\n[ "$1" = --appimage-extract ] || exit 0\nmkdir -p squashfs-root\ncp -R %s/. squashfs-root/\n' \
	"'$payload'" >"$stand_in"
chmod +x "$stand_in"
"$appimg" --yes --no-color install "$stand_in" --update-source "github:$repository"

status=0
"$appimg" --no-color update obsidian --check --json >"$work/check.json" 2>"$work/check.err" || status=$?
cat "$work/check.json" "$work/check.err"
# 0 is an update available, 3 is up to date, anything else an error.
[ "$status" -eq 0 ] || [ "$status" -eq 3 ] || fail "the check of github:$repository failed"
! grep -q 'has no AppImage' "$work/check.json" "$work/check.err" ||
	fail "the check of github:$repository stopped at a release without an AppImage"
# Obsidian names its arm64 build and leaves the x86_64 one unlabeled, so on
# either machine exactly one AppImage of the release is the stand-in's.
! grep -q 'of its AppImages match' "$work/check.json" "$work/check.err" ||
	fail "the check of github:$repository could not tell which of its AppImages is the installed one"
latest=$(jq -r '.[0].latest_version' "$work/check.json")
[ -n "$latest" ] && [ "$latest" != null ] || fail "the check of github:$repository found no version"
echo "github:$repository offers $latest"
