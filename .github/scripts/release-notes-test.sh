#!/bin/sh
# Exercises release-notes.sh against a fixture changelog, so the release
# workflow can be trusted without cutting a release.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
script="$here/release-notes.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fixture="$tmp/CHANGELOG.md"

cat >"$fixture" <<'EOF'
# Changelog

## [Unreleased]

## [0.2.0] - 2026-09-01

### Added

- A second thing.

### Fixed

- A first thing.

## [0.1.0] - 2026-08-31

### Added

- The first release.
EOF

failures=0

expect_output() {
	description=$1
	expected=$2
	shift 2
	if ! actual=$("$@" 2>"$tmp/stderr"); then
		echo "FAIL: $description: exited $?" >&2
		failures=$((failures + 1))
		return
	fi
	if [ "$actual" != "$expected" ]; then
		echo "FAIL: $description" >&2
		printf 'expected:\n%s\ngot:\n%s\n' "$expected" "$actual" >&2
		failures=$((failures + 1))
		return
	fi
	echo "ok: $description"
}

expect_failure() {
	description=$1
	shift
	if actual=$("$@" 2>"$tmp/stderr"); then
		echo "FAIL: $description: succeeded and printed:" >&2
		printf '%s\n' "$actual" >&2
		failures=$((failures + 1))
		return
	fi
	if [ -z "$(cat "$tmp/stderr")" ]; then
		echo "FAIL: $description: failed without saying why" >&2
		failures=$((failures + 1))
		return
	fi
	echo "ok: $description ($(cat "$tmp/stderr"))"
}

section_0_2_0='### Added

- A second thing.

### Fixed

- A first thing.'

expect_output "the section of one version, without the next one" \
	"$section_0_2_0" "$script" 0.2.0 "$fixture"
expect_output "a tag name works like a version" \
	"$section_0_2_0" "$script" v0.2.0 "$fixture"
expect_output "the last section ends at the end of the file" \
	'### Added

- The first release.' "$script" 0.1.0 "$fixture"

expect_failure "a version without a section" "$script" 9.9.9 "$fixture"
expect_failure "an empty section" "$script" Unreleased "$fixture"
expect_failure "a version that is only a prefix of another" "$script" 0.2 "$fixture"
expect_failure "a changelog that does not exist" "$script" 0.2.0 "$tmp/nothing.md"
expect_failure "no arguments at all" "$script"

if [ "$failures" -gt 0 ]; then
	echo "$failures check(s) failed" >&2
	exit 1
fi
echo "all checks passed"
