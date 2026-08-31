# appimg

Installs, updates and removes AppImages as proper desktop applications, entirely
inside `$HOME`.

## Install

```
cargo install appimg
```

Arch/AUR: `appimg` or `appimg-bin`.

Needs Rust 1.88, which is what ratatui's dependencies require, not the code
itself.

## Usage

```
appimg install ./someapp.AppImage
appimg install https://example.com/someapp.AppImage
appimg list --json
appimg update --all --check
appimg remove someapp
appimg doctor
```

Run `appimg` with no arguments for the TUI.

Writes to `$XDG_DATA_HOME/appimages`, `$XDG_DATA_HOME/applications` and
`$XDG_DATA_HOME/icons/hicolor`.
