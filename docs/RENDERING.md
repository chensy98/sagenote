# Rendering

## Math (`text/latex`)

- Native RaTeX only: `ratex-types`, `ratex-parser`, `ratex-layout` and
  `ratex-katex-fonts` are pinned to `=0.1.15`. `ratex-parser` is an additional
  direct dependency because `layout()` consumes `ParseNode`s. Never
  `ratex-wasm`; no KaTeX, MathJax or web renderer.
- Pipeline: `parse → layout → validate_layout → to_display_list`. The
  DisplayList is rasterized with **tiny-skia**; glyph outlines are read from the
  embedded KaTeX TTFs with `ttf-parser`. Upload is
  `ColorImage::from_rgba_premultiplied`; tiny-skia bytes are premultiplied and
  the paint uses `SimpleSRGB` so the texture is sRGB, not linear.
- Glyphs absent from KaTeX (CJK, emoji, unusual symbols) try a system face from
  `fontdb::Database::load_system_fonts()`. `RATEX_UNICODE_FONT`, if set, is
  loaded into that database before rendering. If no face has the outline, the
  whole formula uses the source fallback.
- Cache key `(latex hash, style, color, px_size * pixels_per_point)` with an
  LRU of 64 textures. `Renderer::clear()` runs on cell delete and on
  new/open, dropping handles immediately. Rendered images are fitted to the
  available cell width at draw time, not cached at that width.
- Inline vs display comes from the driver's `display` field:
  `MathStyle::Display` at 19 pt for block, `MathStyle::Text` at 15 pt for
  inline (inside `horizontal_wrapped`).
- Every render call has a fallback: LaTeX source in monospace plus a red
  "unrendered" tag (hover shows the reason). RaTeX is 0.1.x, so parse/layout
  is wrapped in `catch_unwind`; resource limits, missing glyphs, unsupported
  commands and textures over 2048 px per side all use the same fallback. Never
  blank.
- Keep RaTeX behind `src/render.rs::Renderer` so a version bump is one file.

## SVG (`image/svg+xml`)

`image/svg+xml` → `usvg::Tree::from_str` (with the shared system fontdb),
then `resvg::render(&tree, transform, &mut pixmap.as_mut())` into a
`tiny_skia::Pixmap` → egui texture. Natural SVG size is displayed; the raster
scale follows `pixels_per_point` and is capped at 2048 px per side.

## PNG (`image/png`)

`image/png` → base64 decode (the driver's `_image_bundle` format) →
`image::load_from_memory` → RGBA8 → unmultiplied sRGB texture. Images larger
than 2048 px are downscaled; display width is capped at 720 pt.

## Text and errors

`text/plain`, stderr and errors are monospaced; errors are red with the full
traceback. ANSI CSI/OSC escapes are stripped at display time by
`src/text.rs`; the notebook preserves the original bytes.

## Placeholders

`text/html` and 3D (`OutputScene*`, `Graphics3d`) remain explicit
"interactive 3D plot not supported in this prototype" placeholders. Do not
attempt a browser fallback.

## Stale outputs

Saved outputs are stale on load: math is rasterized in grey, SVG/PNG receive
a grey tint, text is grey, and every output has a "stale" tag. Do not re-run
the notebook on open.
