# Bundled UI fonts

IBM Plex Sans (Text 400, Regular 400, Medium 500, SemiBold 600) and IBM Plex
Mono (Regular 400), the typefaces used by the IBM Carbon Design System.

- Source: <https://github.com/IBM/plex> (`packages/plex-sans` and
  `packages/plex-mono`, `fonts/complete/ttf`)
- Copyright IBM Corp.
- Licensed under the SIL Open Font License 1.1; see [OFL.txt](OFL.txt).

`Plex Sans Text` is the optical size designed for small UI copy; Regular is
kept as a fallback. These are embedded in the `sagenote` binary with
`include_bytes!` and installed by `src/settings.rs::install_fonts`, so the UI
does not depend on system fonts.
