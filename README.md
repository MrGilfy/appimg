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
installed file.

Applying a delta update needs `appimageupdatetool`. Get it from the AppImageUpdate releases and put it on your PATH:

    appimg install https://github.com/AppImageCommunity/AppImageUpdate/releases/download/continuous/appimageupdatetool-x86_64.AppImage

Without it, updates from a zsync source fail with a message saying so.

## Where things go

    $XDG_DATA_HOME/appimages/<name>.AppImage      the binary
    $XDG_DATA_HOME/applications/<name>.desktop    the launcher entry
    $XDG_DATA_HOME/icons/hicolor/*/apps/          every icon size it found
