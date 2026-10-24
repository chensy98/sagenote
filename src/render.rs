//! M3 output renderer: RaTeX math, SVG and PNG.
//!
//! Everything that turns a MIME payload into an egui texture lives here.  The
//! backend and notebook model stay windowing-free; the UI only calls the small
//! `Renderer` facade.  RaTeX is deliberately isolated behind this module so a
//! 0.1.x API bump is one file.

use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions, Vec2};
use ratex_layout::{layout, to_display_list, validate_layout, LayoutOptions};
use ratex_parser::parse;
use ratex_types::color::Color as RatexColor;
use ratex_types::display_item::{DisplayItem, DisplayList};
use ratex_types::math_style::MathStyle;
use ratex_types::path_command::PathCommand;
use sagenote::backend::DisplayMode;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Shader, Stroke, Transform};
use ttf_parser::OutlineBuilder;

const PAD_PX: f32 = 3.0;
const MAX_TEXTURE_SIDE: u32 = 2048;
const MAX_PNG_DISPLAY_WIDTH: f32 = 720.0;
const CACHE_ENTRIES: usize = 64;

#[derive(Debug, Clone)]
pub struct RenderError(String);

impl RenderError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RenderError {}

#[derive(Clone)]
pub struct RenderedImage {
    pub texture: TextureHandle,
    pub size: Vec2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum CacheKey {
    Math {
        hash: u64,
        style: u8,
        color: [u8; 4],
        px_tenths: u32,
    },
    Svg {
        hash: u64,
        ppp_tenths: u32,
    },
    Png {
        hash: u64,
        ppp_tenths: u32,
    },
}

struct CacheEntry {
    image: RenderedImage,
    last_used: u64,
}

pub struct Renderer {
    cache: HashMap<CacheKey, CacheEntry>,
    tick: u64,
    fontdb: Arc<fontdb::Database>,
}

impl Renderer {
    pub fn new() -> Self {
        let mut fontdb = fontdb::Database::new();
        fontdb.load_system_fonts();
        if let Some(path) = std::env::var_os("RATEX_UNICODE_FONT") {
            let path = PathBuf::from(path);
            if let Err(error) = fontdb.load_font_file(&path) {
                log::warn!(
                    "RATEX_UNICODE_FONT {} could not be loaded: {error}",
                    path.display()
                );
            }
        }
        Self {
            cache: HashMap::new(),
            tick: 0,
            fontdb: Arc::new(fontdb),
        }
    }

    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Render a `text/latex` payload.  `font_size` is logical points; the
    /// texture is rasterized at `font_size * pixels_per_point` and displayed in
    /// logical points.
    pub fn math(
        &mut self,
        ctx: &egui::Context,
        latex: &str,
        display: DisplayMode,
        color: Color32,
        font_size: f32,
        pixels_per_point: f32,
    ) -> Result<RenderedImage, RenderError> {
        let ppp = pixels_per_point.clamp(1.0, 4.0);
        let px_per_em = (font_size * ppp).clamp(8.0, 96.0);
        let style = match display {
            DisplayMode::Block => MathStyle::Display,
            DisplayMode::Inline => MathStyle::Text,
        };
        let key = CacheKey::Math {
            hash: hash_str(latex),
            style: match style {
                MathStyle::Display => 0,
                MathStyle::Text => 1,
                _ => 2,
            },
            color: [color.r(), color.g(), color.b(), color.a()],
            px_tenths: (px_per_em * 10.0).round() as u32,
        };
        if let Some(image) = self.get(&key) {
            return Ok(image);
        }

        let raster = rasterize_math(latex, style, color, px_per_em, Some(self.fontdb.as_ref()))?;
        let image = upload_premultiplied(
            ctx,
            format!("math-{}-{}", key_hash(&key), key_px(&key)),
            raster,
            ppp,
        );
        self.put(key, image.clone());
        Ok(image)
    }

    /// Render an `image/svg+xml` payload.  The SVG's natural size is in points;
    /// rasterize at the display scale, capped so a pathological SVG cannot
    /// allocate an unbounded texture.
    pub fn svg(
        &mut self,
        ctx: &egui::Context,
        data: &str,
        pixels_per_point: f32,
    ) -> Result<RenderedImage, RenderError> {
        let ppp = pixels_per_point.clamp(1.0, 4.0);
        let key = CacheKey::Svg {
            hash: hash_bytes(data.as_bytes()),
            ppp_tenths: (ppp * 10.0).round() as u32,
        };
        if let Some(image) = self.get(&key) {
            return Ok(image);
        }
        let raster = self.rasterize_svg(data, ppp)?;
        let image = upload_premultiplied(ctx, format!("svg-{}", key_hash(&key)), raster, ppp);
        self.put(key, image.clone());
        Ok(image)
    }

    /// Decode an `image/png` base64 payload and upload it as an sRGB texture.
    pub fn png(
        &mut self,
        ctx: &egui::Context,
        data: &str,
        pixels_per_point: f32,
    ) -> Result<RenderedImage, RenderError> {
        let ppp = pixels_per_point.clamp(1.0, 4.0);
        let key = CacheKey::Png {
            hash: hash_str(data.trim()),
            ppp_tenths: (ppp * 10.0).round() as u32,
        };
        if let Some(image) = self.get(&key) {
            return Ok(image);
        }
        let raster = rasterize_png(data)?;
        let natural = Vec2::new(raster.width as f32 / ppp, raster.height as f32 / ppp);
        let scale = (MAX_PNG_DISPLAY_WIDTH / natural.x.max(1.0)).min(1.0);
        let size = natural * scale;
        let image = upload_unmultiplied(ctx, format!("png-{}", key_hash(&key)), raster, size);
        self.put(key, image.clone());
        Ok(image)
    }

    fn rasterize_svg(&self, data: &str, ppp: f32) -> Result<Raster, RenderError> {
        let options = usvg::Options {
            fontdb: self.fontdb.clone(),
            ..Default::default()
        };
        let tree = usvg::Tree::from_str(data, &options)
            .map_err(|error| RenderError::new(format!("SVG parse: {error}")))?;
        let size = tree.size();
        let width = size.width();
        let height = size.height();
        if !(width.is_finite() && height.is_finite()) || width <= 0.0 || height <= 0.0 {
            return Err(RenderError::new("SVG has no drawable size"));
        }

        let mut scale = ppp;
        if width * scale > MAX_TEXTURE_SIDE as f32 || height * scale > MAX_TEXTURE_SIDE as f32 {
            scale = (MAX_TEXTURE_SIDE as f32 / width)
                .min(MAX_TEXTURE_SIDE as f32 / height)
                .max(0.01);
        }
        let pixel_width = (width * scale).ceil().max(1.0) as u32;
        let pixel_height = (height * scale).ceil().max(1.0) as u32;
        let mut pixmap = Pixmap::new(pixel_width, pixel_height)
            .ok_or_else(|| RenderError::new("SVG texture is too large"))?;
        pixmap.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 0));
        resvg::render(
            &tree,
            Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        Ok(Raster::from_pixmap(pixmap))
    }

    fn get(&mut self, key: &CacheKey) -> Option<RenderedImage> {
        self.tick = self.tick.wrapping_add(1);
        let entry = self.cache.get_mut(key)?;
        entry.last_used = self.tick;
        Some(entry.image.clone())
    }

    fn put(&mut self, key: CacheKey, image: RenderedImage) {
        if self.cache.len() >= CACHE_ENTRIES && !self.cache.contains_key(&key) {
            if let Some(oldest) = self
                .cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            {
                self.cache.remove(&oldest);
            }
        }
        self.cache.insert(
            key,
            CacheEntry {
                image,
                last_used: self.tick,
            },
        );
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Raster {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl Raster {
    fn from_pixmap(pixmap: Pixmap) -> Self {
        Self {
            width: pixmap.width(),
            height: pixmap.height(),
            data: pixmap.data().to_vec(),
        }
    }
}

fn upload_premultiplied(
    ctx: &egui::Context,
    name: String,
    raster: Raster,
    ppp: f32,
) -> RenderedImage {
    let image = ColorImage::from_rgba_premultiplied(
        [raster.width as usize, raster.height as usize],
        &raster.data,
    );
    let texture = ctx.load_texture(name, image, TextureOptions::LINEAR);
    RenderedImage {
        size: Vec2::new(raster.width as f32 / ppp, raster.height as f32 / ppp),
        texture,
    }
}

fn upload_unmultiplied(
    ctx: &egui::Context,
    name: String,
    raster: Raster,
    size: Vec2,
) -> RenderedImage {
    let image = ColorImage::from_rgba_unmultiplied(
        [raster.width as usize, raster.height as usize],
        &raster.data,
    );
    let texture = ctx.load_texture(name, image, TextureOptions::LINEAR);
    RenderedImage { size, texture }
}

/// Pure rasterization entry point, kept free of egui so it can be unit-tested
/// headlessly.  `system_fonts` is used only for glyphs absent from the embedded
/// KaTeX fonts (CJK, emoji, unusual symbols).
fn rasterize_math(
    latex: &str,
    style: MathStyle,
    color: Color32,
    px_per_em: f32,
    system_fonts: Option<&fontdb::Database>,
) -> Result<Raster, RenderError> {
    // RaTeX is 0.1.x.  A parser/layout panic must become the monospace source
    // fallback, not a dead notebook.
    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let nodes =
            parse(latex).map_err(|error| RenderError::new(format!("RaTeX parse: {error}")))?;
        let options = LayoutOptions {
            style,
            color: RatexColor::new(
                color.r() as f32 / 255.0,
                color.g() as f32 / 255.0,
                color.b() as f32 / 255.0,
                color.a() as f32 / 255.0,
            ),
            ..Default::default()
        };
        let tree = layout(&nodes, &options);
        validate_layout(&tree).map_err(RenderError::new)?;
        let display = to_display_list(&tree);
        rasterize_display_list(&display, px_per_em, system_fonts)
    }));
    match rendered {
        Ok(result) => result,
        Err(_) => Err(RenderError::new("RaTeX panicked while laying out")),
    }
}

fn rasterize_display_list(
    display: &DisplayList,
    px_per_em: f32,
    system_fonts: Option<&fontdb::Database>,
) -> Result<Raster, RenderError> {
    if !(display.width.is_finite() && display.height.is_finite() && display.depth.is_finite()) {
        return Err(RenderError::new("layout produced non-finite dimensions"));
    }
    let width = (display.width as f32 * px_per_em).ceil() + 2.0 * PAD_PX;
    let height = (display.total_height() as f32 * px_per_em).ceil() + 2.0 * PAD_PX;
    if width <= 2.0 || height <= 2.0 {
        return Err(RenderError::new("formula has no drawable size"));
    }
    if width > MAX_TEXTURE_SIDE as f32 || height > MAX_TEXTURE_SIDE as f32 {
        return Err(RenderError::new("formula exceeds the texture size limit"));
    }

    let mut pixmap = Pixmap::new(width as u32, height as u32)
        .ok_or_else(|| RenderError::new("formula texture allocation failed"))?;
    pixmap.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 0));

    for item in &display.items {
        match item {
            DisplayItem::GlyphPath {
                x,
                y,
                scale,
                font,
                char_code,
                color,
            } => {
                draw_glyph(
                    &mut pixmap,
                    *x as f32 * px_per_em + PAD_PX,
                    *y as f32 * px_per_em + PAD_PX,
                    *scale as f32,
                    px_per_em,
                    font,
                    *char_code,
                    *color,
                    system_fonts,
                )?;
            }
            DisplayItem::Line {
                x,
                y,
                width,
                thickness,
                color,
                dashed,
            } => {
                draw_line(
                    &mut pixmap,
                    *x as f32 * px_per_em + PAD_PX,
                    *y as f32 * px_per_em + PAD_PX,
                    *width as f32 * px_per_em,
                    *thickness as f32 * px_per_em,
                    *color,
                    *dashed,
                );
            }
            DisplayItem::Rect {
                x,
                y,
                width,
                height,
                color,
            } => {
                fill_rect(
                    &mut pixmap,
                    *x as f32 * px_per_em + PAD_PX,
                    *y as f32 * px_per_em + PAD_PX,
                    *width as f32 * px_per_em,
                    *height as f32 * px_per_em,
                    *color,
                );
            }
            DisplayItem::Path {
                x,
                y,
                commands,
                fill,
                color,
            } => {
                draw_path(
                    &mut pixmap,
                    *x as f32 * px_per_em + PAD_PX,
                    *y as f32 * px_per_em + PAD_PX,
                    commands,
                    *fill,
                    *color,
                    px_per_em,
                )?;
            }
        }
    }
    Ok(Raster::from_pixmap(pixmap))
}

#[allow(clippy::too_many_arguments)]
fn draw_glyph(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    scale: f32,
    px_per_em: f32,
    font: &str,
    char_code: u32,
    color: RatexColor,
    system_fonts: Option<&fontdb::Database>,
) -> Result<(), RenderError> {
    let ch = char::from_u32(char_code)
        .ok_or_else(|| RenderError::new(format!("invalid glyph codepoint U+{char_code:04X}")))?;
    if ch.is_whitespace() {
        return Ok(());
    }

    let filename = format!("KaTeX_{font}.ttf");
    if let Some(bytes) = ratex_katex_fonts::ttf_bytes(&filename) {
        if let Ok(face) = ttf_parser::Face::parse(bytes.as_ref(), 0) {
            if let Some(glyph) = face.glyph_index(ch) {
                if let Some(path) = outline_to_path(&face, glyph, x, y, scale, px_per_em) {
                    pixmap.fill_path(
                        &path,
                        &paint(color),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                    return Ok(());
                }
                // A glyph without an outline is a space/bitmap glyph; try the
                // system fallback before declaring the formula unrenderable.
            }
        }
    }

    if let Some(db) = system_fonts {
        if draw_system_glyph(db, pixmap, ch, x, y, scale, px_per_em, color) {
            return Ok(());
        }
    }

    Err(RenderError::new(format!(
        "no outline for U+{char_code:04X} in {font}"
    )))
}

fn outline_to_path(
    face: &ttf_parser::Face<'_>,
    glyph: ttf_parser::GlyphId,
    x: f32,
    y: f32,
    scale: f32,
    px_per_em: f32,
) -> Option<tiny_skia::Path> {
    let units = face.units_per_em() as f32;
    if units <= 0.0 {
        return None;
    }
    let factor = px_per_em * scale / units;
    let mut builder = GlyphPathBuilder {
        path: PathBuilder::new(),
        x,
        y,
        factor,
    };
    face.outline_glyph(glyph, &mut builder)?;
    builder.path.finish()
}

struct GlyphPathBuilder {
    path: PathBuilder,
    x: f32,
    y: f32,
    factor: f32,
}

impl OutlineBuilder for GlyphPathBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path
            .move_to(self.x + x * self.factor, self.y - y * self.factor);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.path
            .line_to(self.x + x * self.factor, self.y - y * self.factor);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.path.quad_to(
            self.x + x1 * self.factor,
            self.y - y1 * self.factor,
            self.x + x * self.factor,
            self.y - y * self.factor,
        );
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.path.cubic_to(
            self.x + x1 * self.factor,
            self.y - y1 * self.factor,
            self.x + x2 * self.factor,
            self.y - y2 * self.factor,
            self.x + x * self.factor,
            self.y - y * self.factor,
        );
    }

    fn close(&mut self) {
        self.path.close();
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_system_glyph(
    db: &fontdb::Database,
    pixmap: &mut Pixmap,
    ch: char,
    x: f32,
    y: f32,
    scale: f32,
    px_per_em: f32,
    color: RatexColor,
) -> bool {
    for face in db.faces() {
        let id = face.id;
        let mut drawn = false;
        db.with_face_data(id, |data, index| {
            if let Ok(face) = ttf_parser::Face::parse(data, index) {
                let Some(glyph) = face.glyph_index(ch) else {
                    return;
                };
                if let Some(path) = outline_to_path(&face, glyph, x, y, scale, px_per_em) {
                    pixmap.fill_path(
                        &path,
                        &paint(color),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                    drawn = true;
                }
            }
        });
        if drawn {
            return true;
        }
    }
    false
}

fn draw_line(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    width: f32,
    thickness: f32,
    color: RatexColor,
    dashed: bool,
) {
    let thickness = thickness.max(0.6);
    if !(width.is_finite() && thickness.is_finite()) || width <= 0.0 {
        return;
    }
    let top = y - thickness / 2.0;
    if !dashed {
        fill_rect(pixmap, x, top, width, thickness, color);
        return;
    }
    let dash = (thickness * 4.0).max(2.0);
    let gap = (thickness * 3.0).max(2.0);
    let mut cursor = x;
    while cursor < x + width {
        let segment = dash.min(x + width - cursor);
        fill_rect(pixmap, cursor, top, segment, thickness, color);
        cursor += dash + gap;
    }
}

fn fill_rect(pixmap: &mut Pixmap, x: f32, y: f32, width: f32, height: f32, color: RatexColor) {
    if !(x.is_finite() && y.is_finite() && width.is_finite() && height.is_finite()) {
        return;
    }
    if width <= 0.0 || height <= 0.0 {
        return;
    }
    if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, width, height) {
        pixmap.fill_rect(rect, &paint(color), Transform::identity(), None);
    }
}

fn draw_path(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    commands: &[PathCommand],
    fill: bool,
    color: RatexColor,
    px_per_em: f32,
) -> Result<(), RenderError> {
    let mut builder = PathBuilder::new();
    for command in commands {
        match *command {
            PathCommand::MoveTo { x: cx, y: cy } => {
                builder.move_to(x + cx as f32 * px_per_em, y + cy as f32 * px_per_em);
            }
            PathCommand::LineTo { x: cx, y: cy } => {
                builder.line_to(x + cx as f32 * px_per_em, y + cy as f32 * px_per_em);
            }
            PathCommand::CubicTo {
                x1,
                y1,
                x2,
                y2,
                x: cx,
                y: cy,
            } => {
                builder.cubic_to(
                    x + x1 as f32 * px_per_em,
                    y + y1 as f32 * px_per_em,
                    x + x2 as f32 * px_per_em,
                    y + y2 as f32 * px_per_em,
                    x + cx as f32 * px_per_em,
                    y + cy as f32 * px_per_em,
                );
            }
            PathCommand::QuadTo {
                x1,
                y1,
                x: cx,
                y: cy,
            } => {
                builder.quad_to(
                    x + x1 as f32 * px_per_em,
                    y + y1 as f32 * px_per_em,
                    x + cx as f32 * px_per_em,
                    y + cy as f32 * px_per_em,
                );
            }
            PathCommand::Close => builder.close(),
        }
    }
    let path = builder
        .finish()
        .ok_or_else(|| RenderError::new("empty display path"))?;
    if fill {
        pixmap.fill_path(
            &path,
            &paint(color),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    } else {
        let stroke = Stroke {
            width: (px_per_em * 0.04).max(1.0),
            ..Default::default()
        };
        pixmap.stroke_path(&path, &paint(color), &stroke, Transform::identity(), None);
    }
    Ok(())
}

fn paint(color: RatexColor) -> Paint<'static> {
    let color = tiny_skia::Color::from_rgba8(
        (color.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.b.clamp(0.0, 1.0) * 255.0).round() as u8,
        (color.a.clamp(0.0, 1.0) * 255.0).round() as u8,
    );
    Paint {
        shader: Shader::SolidColor(color),
        anti_alias: true,
        // tiny-skia's default blend space is linear; egui uploads sRGB bytes.
        colorspace: tiny_skia::ColorSpace::SimpleSRGB,
        ..Default::default()
    }
}

fn rasterize_png(data: &str) -> Result<Raster, RenderError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|error| RenderError::new(format!("PNG base64: {error}")))?;
    let image = image::load_from_memory(&bytes)
        .map_err(|error| RenderError::new(format!("PNG decode: {error}")))?;
    let image = if image.width() > MAX_TEXTURE_SIDE || image.height() > MAX_TEXTURE_SIDE {
        image.resize(
            MAX_TEXTURE_SIDE,
            MAX_TEXTURE_SIDE,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };
    let rgba = image.to_rgba8();
    Ok(Raster {
        width: rgba.width(),
        height: rgba.height(),
        data: rgba.into_raw(),
    })
}

fn hash_str(value: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn hash_bytes(value: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn key_hash(key: &CacheKey) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

fn key_px(key: &CacheKey) -> u32 {
    match key {
        CacheKey::Math { px_tenths, .. } => *px_tenths,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_rasterizes_to_nonempty_pixels() {
        let raster = rasterize_math(
            r"x^2 + \frac{1}{2}",
            MathStyle::Display,
            Color32::BLACK,
            32.0,
            None,
        )
        .expect("math should rasterize");
        assert!(raster.width > 0);
        assert!(raster.height > 0);
        assert!(raster.data.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn invalid_latex_falls_back_to_an_error() {
        let error = rasterize_math(r"\frac{", MathStyle::Display, Color32::BLACK, 24.0, None)
            .expect_err("invalid formula must not render");
        assert!(error.to_string().starts_with("RaTeX parse"));
    }

    #[test]
    fn svg_rasterizes_a_rectangle() {
        let renderer = Renderer::new();
        let raster = renderer
            .rasterize_svg(
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="12"><rect width="20" height="12" fill="#ff0000"/></svg>"##,
                1.0,
            )
            .expect("svg should render");
        assert_eq!((raster.width, raster.height), (20, 12));
        assert!(raster.data.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn png_decodes_a_one_pixel_image() {
        // 1x1 opaque red PNG, generated with Pillow.
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
        let raster = rasterize_png(b64).expect("png should decode");
        assert_eq!((raster.width, raster.height), (1, 1));
        assert_eq!(raster.data.len(), 4);
    }

    #[test]
    fn texture_cache_reuses_the_same_handle() {
        let ctx = egui::Context::default();
        let mut renderer = Renderer::new();
        let first = renderer
            .math(&ctx, "x+1", DisplayMode::Block, Color32::BLACK, 16.0, 1.0)
            .expect("first render");
        let second = renderer
            .math(&ctx, "x+1", DisplayMode::Block, Color32::BLACK, 16.0, 1.0)
            .expect("cached render");
        assert_eq!(first.texture.id(), second.texture.id());
    }
}
