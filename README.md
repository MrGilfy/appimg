# appimg

Installs, updates and removes AppImages as proper desktop applications, entirely
inside `$HOME`.

![appimg](docs/screenshot.png)

## Install

Arch, from the AUR:

    yay -S appimg        # builds from source
    yay -S appimg-bin    # prebuilt binary

Anywhere else:

    cargo install appimg

Requires Rust 1.88. That comes from ratatui's dependencies, not from this code.

## Usage

Run `appimg` without arguments for the TUI, or go straight to a command:

    appimg install ./someapp.AppImage
    appimg install https://example.com/someapp.AppImage
    appimg list --json
    appimg update --all --check
    appimg remove someapp
    appimg doctor

The entry is registered right away. Some launchers read their application
list once at startup, so it may only show up after you restart your shell.

## Updates

    appimg update --all --check    check without changing anything
    appimg update --all            download and replace

Both halves of an update work with nothing else installed. If the AppImage
carries zsync update information, appimg reads the zsync file itself: the
header says whether the file on disk is still the one on offer, and the block
checksums that follow it say which parts of the new version you already have.
Only the missing ranges are fetched, and the assembled file is checked against
the checksum the zsync file carries before it replaces anything. A file that
does not match is thrown away and the installed version stays as it is.

Both forms of zsync update information are a delta source: `zsync|<url>`, and
`gh-releases-zsync|...`, where the zsync file is an asset of a GitHub release.
Everything else is a full download, because there is nothing to apply a delta
from.

Every update says which path it took and what it cost:

    Updating ImHex...
      1.38.0 -> 1.38.1
      reused 19054 of 46308 blocks, fetched 107.0 MB in 22 requests

    Updating Some App...
      1.0.0 -> 1.1.0
      no delta for this source, downloaded 42.0 MB

How much a delta saves is up to the AppImage. One that changes a little
between builds fetches a few megabytes; one whose squashfs shifts under every
change fetches most of itself either way.

The previous version is kept as `<slug>.AppImage.bak` until the new one has
run once, so a broken update can be rolled back.

`appimageupdatetool` is not needed. appimg falls back to it when its own delta
path fails, and says so when that happens, but nothing has to be installed for
updates to work. `appimg doctor` reports whether it is around.

## Where things go

    $XDG_DATA_HOME/appimages/<name>.AppImage      the binary
    $XDG_DATA_HOME/applications/<name>.desktop    the launcher entry
    $XDG_DATA_HOME/icons/hicolor/*/apps/          every icon size it found
