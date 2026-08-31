#!/bin/sh
# Prints the changelog section of one release, for the body of a GitHub
# release. A version without a section, or with an empty one, is an error:
# a release must never go out with a placeholder instead of its notes.
set -eu

usage() {
	echo "usage: ${0##*/} <version|tag> [changelog]" >&2
	exit 2
}

[ $# -ge 1 ] && [ $# -le 2 ] || usage
version=${1#v}
changelog=${2:-CHANGELOG.md}
[ -n "$version" ] || usage

if [ ! -f "$changelog" ]; then
	echo "${0##*/}: $changelog does not exist" >&2
	exit 1
fi

if ! grep -qF "## [$version]" "$changelog"; then
	echo "${0##*/}: $changelog has no section for version $version" >&2
	exit 1
fi

# Everything between this version's heading and the next one. Sub-headings
# start with three hashes and stay in.
notes=$(
	awk -v version="$version" '
		index($0, "## [" version "]") == 1 { inside = 1; next }
		inside && index($0, "## [") == 1 { exit }
		inside { print }
	' "$changelog" | sed -e '/./,$!d'
)

if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
	echo "${0##*/}: the section for version $version in $changelog is empty" >&2
	exit 1
fi

printf '%s\n' "$notes"
