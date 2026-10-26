# Platforms

Linux (x11 + wayland) is the supported platform; macOS (Apple Silicon) is
best effort — it compiles and passes CI, but has not been runtime-tested on
real hardware. Windows is unsupported. Everything below is verified on Linux
with real Sage 10.10, through native zenity error dialogs and xdg-portal file
dialogs.

Both targets are Unix: signals, process groups and atomic rename are shared.
Divergences live in `platform.rs` with `cfg`, not a `trait Platform`:

- driver location: `std::env::temp_dir()/sagenote-<pid>/driver.py`
  (`$XDG_RUNTIME_DIR` does not exist on macOS)
- `rfd`: target-scoped `xdg-portal` on Linux; AppKit panels on macOS. GTK may
  be omitted if xdg-portal is used. rfd's Linux **message** dialog is implemented
  through `zenity`; native error dialogs require it (the in-window banner is the
  fallback).
- shortcut modifiers: `Modifiers::COMMAND` everywhere, no branching.
- `cargo-bundle` `Info.plist` must set `NSHighResolutionCapable` to `YES`, or
  the whole UI renders at 1× while the terminal binary looks fine.
- wgpu resolves to Metal on M1. Confirm once with `RUST_LOG=wgpu=info`; no
  silent GL fallback.
- Math rendering uses `fontdb` system fonts for SVG text (matplotlib labels)
  and for CJK/emoji glyphs absent from KaTeX. A minimal Linux install needs
  DejaVu/Noto fonts for labels; if no face has a CJK outline the formula falls
  back to source. `RATEX_UNICODE_FONT` is loaded into the same database when set.

## Linux (Debian/Ubuntu)

```sh
sudo apt update
sudo apt install pkg-config libxkbcommon-dev libwayland-dev libxcb1-dev \
  libx11-dev mesa-vulkan-drivers libvulkan-dev
```

rfd uses xdg-portal on Linux, so GTK is not needed. Install `libgtk-3-dev`
only if you deliberately switch rfd to its gtk3 feature.

Recommended for SVG labels and CJK math fallback, not for compilation:
`fonts-dejavu-core fonts-noto-cjk`.

rfd uses zenity for native error message boxes.

Over SSH or under llvmpipe, the GUI needs `WGPU_BACKEND=gl`; the default wgpu
path fails with a Vulkan error that reads like a code bug. Confirm the adapter
once with `RUST_LOG=wgpu=info` rather than shipping a silent GL fallback.

## macOS (Apple Silicon)

```sh
xcode-select --install
```

On Apple Silicon wgpu resolves to Metal. The Linux rfd build uses xdg-portal;
a running desktop portal is required for the file dialogs. Native error
message boxes on Linux are implemented by rfd through **zenity**; install it if
your desktop does not already provide it.

## Windows

Unsupported.
