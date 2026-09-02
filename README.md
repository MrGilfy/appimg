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

Checking works out of the box. If the AppImage carries zsync update
information, appimg reads the zsync header itself and compares it to the
installed file. Both forms of it are a delta source: `zsync|<url>`, and
`gh-releases-zsync|...`, where the zsync file is an asset of a GitHub
release.

Applying the delta is appimg's own work as well: it reads the block checksums
out of the zsync file, works out which blocks the installed AppImage already
holds, fetches only the ranges it is missing and verifies the assembled file
against the checksum the zsync file carries. Every update says which path it
took and what it cost:

    Updating ImHex...
      1.38.0 -> 1.38.1
      reused 19054 of 46308 blocks, fetched 107.0 MB in 22 requests

A source with no zsync file says so as plainly:

    Updating Some App...
      1.0.0 -> 1.1.0
      no delta for this source, downloaded 42.0 MB

`appimageupdatetool` is kept as a fallback for the cases the native path
cannot handle, and an update says when it ran. It is not packaged for Arch;
if you want the fallback, it has to be on your PATH:

    mkdir -p ~/.local/bin
    curl -L -o ~/.local/bin/appimageupdatetool \
      https://github.com/AppImageCommunity/AppImageUpdate/releases/download/continuous/appimageupdatetool-x86_64.AppImage
    chmod +x ~/.local/bin/appimageupdatetool

`appimg doctor` tells you whether it found it.

## Where things go

    $XDG_DATA_HOME/appimages/<name>.AppImage      the binary
    $XDG_DATA_HOME/applications/<name>.desktop    the launcher entry
    $XDG_DATA_HOME/icons/hicolor/*/apps/          every icon size it found
