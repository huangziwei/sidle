//! Painting a laid-out chapter onto a pixel buffer. [`Painter::paint`] reads
//! a [`Fragment`] tree and never the document; a glyph comes from a
//! [`Sprite`] where one fits and from its outline where none does.

use std::collections::HashMap;

use ab_glyph::{Font as _, OutlineCurve};
use bokai::style::Color;
use tiny_skia::{
    FillRule, FilterQuality, IntSize, Mask, Paint, PathBuilder, Pixmap, PixmapMut, PixmapPaint,
    Transform,
};

use crate::font::{FaceId, Fonts};
use crate::fragment::{Border, Content, Fragment, GlyphRun, Node, Orientation};
use crate::geom::Rect;
use crate::resource::Resources;

/// Outlines, sprites and decoded pictures a [`Painter`] builds, kept between
/// draws.
#[derive(Default)]
pub struct Cache {
    outlines: HashMap<(FaceId, u16), Option<tiny_skia::Path>>,
    images: HashMap<String, Option<Pixmap>>,
    sprites: HashMap<(FaceId, u16, Shape, u32), Option<Sprite>>,
}

/// Draws fragment trees, caching the glyph outlines it builds.
pub struct Painter<'a> {
    fonts: &'a Fonts,
    resources: &'a dyn Resources,
    cache: Owned<'a>,
    /// The mask every draw is confined to, over the window's own.
    clip: Clip<'a>,
}

/// A [`Cache`] the painter borrowed, or one it made for a single draw.
enum Owned<'a> {
    Borrowed(&'a mut Cache),
    Own(Box<Cache>),
}

impl Owned<'_> {
    fn get(&mut self) -> &mut Cache {
        match self {
            Owned::Borrowed(cache) => cache,
            Owned::Own(cache) => cache,
        }
    }
}

impl<'a> Painter<'a> {
    /// A painter whose outlines live only as long as it does.
    pub fn new(fonts: &'a Fonts, resources: &'a dyn Resources) -> Self {
        Self {
            fonts,
            resources,
            cache: Owned::Own(Box::default()),
            clip: None,
        }
    }

    /// A painter drawing through `cache`, which outlives it.
    pub fn cached(fonts: &'a Fonts, resources: &'a dyn Resources, cache: &'a mut Cache) -> Self {
        Self {
            fonts,
            resources,
            cache: Owned::Borrowed(cache),
            clip: None,
        }
    }

    /// Confine every draw to `mask`, in buffer pixels.
    pub fn within(mut self, mask: &'a Mask) -> Self {
        self.clip = Some(mask);
        self
    }

    /// Draw the part of `tree` inside `window`, in chapter coordinates, onto
    /// `target`, with `window`'s corner at `origin` and `scale` buffer pixels
    /// to the dot. Composites; `target`'s own ground is the caller's to fill.
    pub fn paint(
        &mut self,
        tree: &Fragment,
        window: Rect,
        origin: (f32, f32),
        scale: f32,
        target: &mut PixmapMut<'_>,
    ) {
        let view = Transform::from_translate(origin.0 - window.x, origin.1 - window.y)
            .post_scale(scale, scale);

        // `showing` is gathered once; both passes then run over it.
        let showing = shown(tree, window);
        // Anything straddling the mask's edge is cut at it.
        let own = window_mask(window, &showing, origin, scale, target);
        let clip = self.clip.or(own.as_ref());

        // Backgrounds and borders first; text sits on top of them.
        for fragment in &showing {
            if let Some(color) = fragment.background {
                fill_clipped(fragment.rect, color, view, clip, target);
            }
            if let Some(border) = &fragment.border {
                self.border(fragment.rect, border, view, clip, target);
            }
        }

        for fragment in &showing {
            match &fragment.content {
                Content::Empty => {}
                Content::Glyphs(run) => self.glyphs(fragment.rect, run, view, clip, target),
                Content::Image(src) => self.image(fragment.rect, src, view, clip, target),
            }
        }
    }

    fn border(
        &self,
        rect: Rect,
        border: &Border,
        view: Transform,
        clip: Clip<'_>,
        target: &mut PixmapMut<'_>,
    ) {
        let widths = border.widths;
        if let (Some(color), true) = (border.top, widths.top > 0.0) {
            fill_clipped(
                Rect::new(rect.x, rect.y, rect.width, widths.top),
                color,
                view,
                clip,
                target,
            );
        }
        if let (Some(color), true) = (border.bottom, widths.bottom > 0.0) {
            fill_clipped(
                Rect::new(
                    rect.x,
                    rect.bottom() - widths.bottom,
                    rect.width,
                    widths.bottom,
                ),
                color,
                view,
                clip,
                target,
            );
        }
        if let (Some(color), true) = (border.left, widths.left > 0.0) {
            fill_clipped(
                Rect::new(rect.x, rect.y, widths.left, rect.height),
                color,
                view,
                clip,
                target,
            );
        }
        if let (Some(color), true) = (border.right, widths.right > 0.0) {
            fill_clipped(
                Rect::new(
                    rect.right() - widths.right,
                    rect.y,
                    widths.right,
                    rect.height,
                ),
                color,
                view,
                clip,
                target,
            );
        }
    }

    fn glyphs(
        &mut self,
        rect: Rect,
        run: &GlyphRun,
        view: Transform,
        clip: Clip<'_>,
        target: &mut PixmapMut<'_>,
    ) {
        let unit = run.size / self.fonts.face(run.face).units_per_em();
        let mut paint = Paint {
            anti_alias: true,
            ..Default::default()
        };
        paint.set_color_rgba8(run.color.r, run.color.g, run.color.b, run.color.a);

        for glyph in &run.glyphs {
            // `along` runs down the line and `across` out from its baseline.
            // Each `Orientation` maps the two onto the page differently.
            let placement = match run.orientation {
                Orientation::Horizontal => Transform::from_translate(
                    rect.x + glyph.along,
                    rect.y + run.baseline + glyph.across,
                )
                .pre_scale(unit, -unit),
                Orientation::Upright => Transform::from_translate(
                    rect.x + run.baseline + glyph.across,
                    rect.y + glyph.along,
                )
                .pre_scale(unit, -unit),
                // A quarter turn clockwise puts the tops of the letters
                // towards the right of a vertical line.
                Orientation::Sideways => Transform::from_translate(
                    rect.x + run.baseline + glyph.across,
                    rect.y + glyph.along,
                )
                .pre_rotate(90.0)
                .pre_scale(unit, -unit),
            };
            let transform = placement.post_concat(view);
            if self.blit(run, glyph.id, transform, clip, target) {
                continue;
            }
            let Some(path) = self.outline(run.face, glyph.id) else {
                continue;
            };
            target.fill_path(path, &paint, FillRule::Winding, transform, clip);
        }

        self.rules(rect, run, view, clip, target);
    }

    /// Draw one glyph from its rasterized sprite, `false` where it has none.
    fn blit(
        &mut self,
        run: &GlyphRun,
        glyph: u16,
        transform: Transform,
        clip: Clip<'_>,
        target: &mut PixmapMut<'_>,
    ) -> bool {
        let shape = Shape {
            sx: transform.sx.to_bits(),
            ky: transform.ky.to_bits(),
            kx: transform.kx.to_bits(),
            sy: transform.sy.to_bits(),
        };
        let key = (run.face, glyph, shape, colour_of(run.color));
        if !self.cache.get().sprites.contains_key(&key) {
            let drawn = self
                .outline(run.face, glyph)
                .cloned()
                .and_then(|path| Sprite::of(&path, shape, run.color));
            self.cache.get().sprites.insert(key, drawn);
        }
        let Some(sprite) = self.cache.get().sprites.get(&key).and_then(Option::as_ref) else {
            return false;
        };
        target.draw_pixmap(
            (transform.tx + sprite.dx).round() as i32,
            (transform.ty + sprite.dy).round() as i32,
            sprite.pixmap.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            clip,
        );
        true
    }

    /// Underline and strike-through, drawn across the whole run.
    fn rules(
        &self,
        rect: Rect,
        run: &GlyphRun,
        view: Transform,
        clip: Clip<'_>,
        target: &mut PixmapMut<'_>,
    ) {
        if !run.underline && !run.line_through {
            return;
        }
        let thickness = (run.size / 16.0).max(1.0);
        // A `Sideways` run's own downward points at the page's left edge.
        let side = match run.orientation {
            Orientation::Sideways => -1.0,
            _ => 1.0,
        };
        let along = |offset: f32| match run.orientation {
            Orientation::Horizontal => Rect::new(
                rect.x,
                rect.y + run.baseline + offset,
                rect.width,
                thickness,
            ),
            _ => Rect::new(
                rect.x + run.baseline + offset * side,
                rect.y,
                thickness,
                rect.height,
            ),
        };
        if run.underline {
            fill_clipped(along(run.size * 0.1), run.color, view, clip, target);
        }
        if run.line_through {
            fill_clipped(along(-run.size * 0.3), run.color, view, clip, target);
        }
    }

    /// A glyph's outline as a path in font units, y up.
    fn outline(&mut self, face: FaceId, glyph: u16) -> Option<&tiny_skia::Path> {
        let loaded = self.fonts.face(face);
        self.cache
            .get()
            .outlines
            .entry((face, glyph))
            .or_insert_with(|| build_outline(loaded.outline(), glyph))
            .as_ref()
    }

    fn image(
        &mut self,
        rect: Rect,
        src: &str,
        view: Transform,
        clip: Clip<'_>,
        target: &mut PixmapMut<'_>,
    ) {
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        let resources = self.resources;
        let pixmap = self
            .cache
            .get()
            .images
            .entry(src.to_string())
            .or_insert_with(|| decode(resources, src));
        let Some(pixmap) = pixmap else { return };

        let scale_x = rect.width / pixmap.width() as f32;
        let scale_y = rect.height / pixmap.height() as f32;
        let placement = Transform::from_translate(rect.x, rect.y).pre_scale(scale_x, scale_y);
        target.draw_pixmap(
            0,
            0,
            pixmap.as_ref(),
            &PixmapPaint {
                quality: FilterQuality::Bilinear,
                ..Default::default()
            },
            placement.post_concat(view),
            clip,
        );
    }
}

/// What a page may draw into, in device pixels. `None` draws everywhere.
pub type Clip<'a> = Option<&'a Mask>;

/// Fill one rectangle, given in the coordinates `view` maps from.
pub fn fill(rect: Rect, color: Color, view: Transform, clip: Clip<'_>, target: &mut PixmapMut<'_>) {
    fill_clipped(rect, color, view, clip, target);
}

fn fill_clipped(
    rect: Rect,
    color: Color,
    view: Transform,
    clip: Clip<'_>,
    target: &mut PixmapMut<'_>,
) {
    let Some(path) = rectangle(rect) else {
        return;
    };
    let mut paint = Paint {
        anti_alias: false,
        ..Default::default()
    };
    paint.set_color_rgba8(color.r, color.g, color.b, color.a);
    target.fill_path(&path, &paint, FillRule::Winding, view, clip);
}

/// The fragments page `window` shows. A [`Node::Line`]'s own box decides its
/// whole subtree: a ruby strip lies outside that box, and is drawn with the
/// base it reads.
pub fn shown(tree: &Fragment, window: Rect) -> Vec<&Fragment> {
    let mut out = Vec::new();
    gather(tree, &window, &mut out);
    out
}

fn gather<'a>(fragment: &'a Fragment, window: &Rect, out: &mut Vec<&'a Fragment>) {
    if fragment.kind == Node::Line {
        // A line abutting the window belongs to the page on the other side of
        // the cut; the inset keeps it there through a rounded coordinate.
        if fragment.rect.intersects(&window.inset_by(0.5)) {
            out.extend(fragment.walk());
        }
        return;
    }
    if fragment.rect.intersects(window) {
        out.push(fragment);
    }
    for child in &fragment.children {
        gather(child, window, out);
    }
}

/// A mask over the page's content area and what `showing` draws past it, in
/// device pixels. A fragment larger than the window keeps the window's edge.
fn window_mask(
    window: Rect,
    showing: &[&Fragment],
    origin: (f32, f32),
    scale: f32,
    target: &PixmapMut<'_>,
) -> Option<Mask> {
    let bleed = showing
        .iter()
        .filter(|fragment| fragment.draws())
        .map(|fragment| fragment.rect)
        .filter(|rect| rect.width <= window.width && rect.height <= window.height)
        .fold(window, |area, rect| area.union(&rect));
    let area = Rect::new(
        (origin.0 + bleed.x - window.x) * scale,
        (origin.1 + bleed.y - window.y) * scale,
        bleed.width * scale,
        bleed.height * scale,
    );
    // A window covering the whole buffer masks nothing off.
    if area.x <= 0.0
        && area.y <= 0.0
        && area.right() >= target.width() as f32
        && area.bottom() >= target.height() as f32
    {
        return None;
    }
    let mut mask = Mask::new(target.width(), target.height())?;
    mask.fill_path(
        &rectangle(area)?,
        FillRule::Winding,
        true,
        Transform::identity(),
    );
    Some(mask)
}

/// A mask over `rect`, given in the coordinates `view` maps from, covering a
/// buffer the size of `target`.
pub fn mask(rect: Rect, view: Transform, target: &PixmapMut<'_>) -> Option<Mask> {
    let mut mask = Mask::new(target.width(), target.height())?;
    mask.fill_path(&rectangle(rect)?, FillRule::Winding, true, view);
    Some(mask)
}

/// Stroke one rectangle's outline, `width` wide in the same coordinates.
pub fn outline(
    rect: Rect,
    color: Color,
    width: f32,
    view: Transform,
    clip: Clip<'_>,
    target: &mut PixmapMut<'_>,
) {
    let Some(path) = rectangle(rect) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.a);
    let stroke = tiny_skia::Stroke {
        width,
        ..Default::default()
    };
    target.stroke_path(&path, &paint, &stroke, view, clip);
}

/// A rectangle as a path. `None` for one with no area.
fn rectangle(rect: Rect) -> Option<tiny_skia::Path> {
    let bounds = tiny_skia::Rect::from_xywh(rect.x, rect.y, rect.width, rect.height)?;
    let mut builder = PathBuilder::new();
    builder.push_rect(bounds);
    builder.finish()
}

/// A glyph's contours as one path, in font units with y up. `outline` marks
/// no contour boundary: one ends wherever the next curve fails to begin
/// where the last finished.
fn build_outline(font: &ab_glyph::FontRef<'static>, glyph: u16) -> Option<tiny_skia::Path> {
    let outline = font.outline(ab_glyph::GlyphId(glyph))?;
    let mut builder = PathBuilder::new();
    let mut cursor: Option<ab_glyph::Point> = None;

    for curve in &outline.curves {
        let (from, to) = match *curve {
            OutlineCurve::Line(from, to) => (from, to),
            OutlineCurve::Quad(from, _, to) => (from, to),
            OutlineCurve::Cubic(from, _, _, to) => (from, to),
        };
        if cursor != Some(from) {
            if cursor.is_some() {
                builder.close();
            }
            builder.move_to(from.x, from.y);
        }
        match *curve {
            OutlineCurve::Line(_, to) => builder.line_to(to.x, to.y),
            OutlineCurve::Quad(_, control, to) => builder.quad_to(control.x, control.y, to.x, to.y),
            OutlineCurve::Cubic(_, first, second, to) => {
                builder.cubic_to(first.x, first.y, second.x, second.y, to.x, to.y)
            }
        }
        cursor = Some(to);
    }
    if cursor.is_some() {
        builder.close();
    }
    builder.finish()
}

/// Decode a resource into a premultiplied pixmap, which is what tiny-skia
/// composites from.
fn decode(resources: &dyn Resources, src: &str) -> Option<Pixmap> {
    let bitmap = resources.image_bitmap(src)?;
    let size = IntSize::from_wh(bitmap.width, bitmap.height)?;
    let mut data = Vec::with_capacity(bitmap.rgba.len());
    for pixel in bitmap.rgba.chunks_exact(4) {
        let alpha = pixel[3] as u32;
        data.push((pixel[0] as u32 * alpha / 255) as u8);
        data.push((pixel[1] as u32 * alpha / 255) as u8);
        data.push((pixel[2] as u32 * alpha / 255) as u8);
        data.push(pixel[3]);
    }
    Pixmap::from_vec(data, size)
}

#[cfg(test)]
mod tests {
    use bokai::model::{NodeId, Role};

    use super::*;

    const RED: Color = Color {
        r: 255,
        g: 0,
        b: 0,
        a: 255,
    };

    fn block(x: f32, y: f32, w: f32, h: f32) -> Fragment {
        let mut fragment = Fragment::new(NodeId(1), Role::Paragraph, Rect::new(x, y, w, h));
        fragment.background = Some(RED);
        fragment
    }

    fn painter() -> (Fonts, crate::resource::Unknown) {
        (Fonts::empty(), crate::resource::Unknown)
    }

    #[test]
    fn a_declared_background_reaches_the_buffer() {
        let (fonts, resources) = painter();
        let mut painter = Painter::new(&fonts, &resources);
        let mut pixmap = Pixmap::new(10, 10).unwrap();

        painter.paint(
            &block(0.0, 0.0, 10.0, 10.0),
            Rect::new(0.0, 0.0, 10.0, 10.0),
            (0.0, 0.0),
            1.0,
            &mut pixmap.as_mut(),
        );

        assert_eq!(pixmap.pixel(5, 5).unwrap().red(), 255);
    }

    #[test]
    fn a_box_scrolled_past_paints_nothing() {
        let (fonts, resources) = painter();
        let mut painter = Painter::new(&fonts, &resources);
        let mut pixmap = Pixmap::new(10, 10).unwrap();

        painter.paint(
            &block(0.0, 0.0, 10.0, 10.0),
            Rect::new(0.0, 20.0, 10.0, 10.0),
            (0.0, 0.0),
            1.0,
            &mut pixmap.as_mut(),
        );

        assert_eq!(pixmap.pixel(5, 5).unwrap().alpha(), 0);
    }

    #[test]
    fn scrolling_moves_a_box_up_by_the_scroll_distance() {
        let (fonts, resources) = painter();
        let mut painter = Painter::new(&fonts, &resources);
        let mut pixmap = Pixmap::new(10, 20).unwrap();

        painter.paint(
            &block(0.0, 10.0, 10.0, 10.0),
            Rect::new(0.0, 10.0, 10.0, 20.0),
            (0.0, 0.0),
            1.0,
            &mut pixmap.as_mut(),
        );

        assert_eq!(pixmap.pixel(5, 0).unwrap().red(), 255);
        assert_eq!(pixmap.pixel(5, 15).unwrap().alpha(), 0);
    }

    #[test]
    fn a_zero_height_box_paints_nothing_instead_of_panicking() {
        let (fonts, resources) = painter();
        let mut painter = Painter::new(&fonts, &resources);
        let mut pixmap = Pixmap::new(10, 10).unwrap();

        painter.paint(
            &block(0.0, 0.0, 10.0, 0.0),
            Rect::new(0.0, 0.0, 10.0, 10.0),
            (0.0, 0.0),
            1.0,
            &mut pixmap.as_mut(),
        );

        assert_eq!(pixmap.pixel(5, 0).unwrap().alpha(), 0);
    }

    #[test]
    fn a_border_draws_on_all_four_sides() {
        let (fonts, resources) = painter();
        let mut painter = Painter::new(&fonts, &resources);
        let mut pixmap = Pixmap::new(10, 10).unwrap();
        let mut fragment = Fragment::new(NodeId(1), Role::Rule, Rect::new(0.0, 0.0, 10.0, 10.0));
        fragment.background = None;
        fragment.border = Some(Border {
            widths: crate::geom::Edges::new(2.0, 2.0, 2.0, 2.0),
            top: Some(RED),
            right: Some(RED),
            bottom: Some(RED),
            left: Some(RED),
        });

        painter.paint(
            &fragment,
            Rect::new(0.0, 0.0, 10.0, 10.0),
            (0.0, 0.0),
            1.0,
            &mut pixmap.as_mut(),
        );

        assert_eq!(pixmap.pixel(5, 0).unwrap().red(), 255);
        assert_eq!(pixmap.pixel(5, 9).unwrap().red(), 255);
        assert_eq!(pixmap.pixel(0, 5).unwrap().red(), 255);
        assert_eq!(pixmap.pixel(9, 5).unwrap().red(), 255);
        // The middle is inside the border, and nothing filled it.
        assert_eq!(pixmap.pixel(5, 5).unwrap().alpha(), 0);
    }

    /// A line carrying a ruby strip, the strip set `strip` before the line's
    /// own box and given `source`.
    fn ruby_line(source: u32, block: f32, height: f32, strip: f32) -> Fragment {
        let mut line = Fragment::new(
            NodeId(source),
            Role::Paragraph,
            Rect::new(0.0, block, 100.0, height),
        )
        .as_kind(Node::Line);
        let mut annotation = Fragment::new(
            NodeId(source + 1),
            Role::Paragraph,
            Rect::new(10.0, block - strip, 20.0, strip),
        );
        annotation.background = Some(RED);
        line.children.push(annotation);
        line
    }

    fn ruby_pages() -> Fragment {
        let mut root = Fragment::new(
            NodeId(0),
            Role::Paragraph,
            Rect::new(0.0, 0.0, 100.0, 100.0),
        );
        root.children.push(ruby_line(1, 0.0, 50.0, 10.0));
        root.children.push(ruby_line(3, 50.0, 50.0, 10.0));
        root
    }

    fn sources(showing: &[&Fragment]) -> Vec<u32> {
        showing.iter().map(|fragment| fragment.source.0).collect()
    }

    #[test]
    fn a_ruby_strip_reaching_back_stays_with_its_own_page() {
        let root = ruby_pages();
        let first = sources(&shown(&root, Rect::new(0.0, 0.0, 100.0, 50.0)));

        assert!(first.contains(&2));
        assert!(!first.contains(&4));
    }

    #[test]
    fn a_line_abutting_the_window_belongs_to_the_page_past_it() {
        let mut root = Fragment::new(
            NodeId(0),
            Role::Paragraph,
            Rect::new(0.0, 0.0, 100.0, 100.0),
        );
        root.children.push(ruby_line(1, 0.0, 50.0, 10.0));
        root.children.push(ruby_line(3, 50.0, 50.0, 10.0));

        // The window ends where the second line starts.
        let first = sources(&shown(&root, Rect::new(0.0, 0.0, 100.0, 50.0)));

        assert!(first.contains(&1));
        assert!(!first.contains(&3));
    }

    #[test]
    fn a_first_lines_ruby_strip_is_drawn_with_it() {
        let root = ruby_pages();
        let second = sources(&shown(&root, Rect::new(0.0, 50.0, 100.0, 50.0)));

        assert!(second.contains(&4));
        assert!(!second.contains(&2));
    }

    #[test]
    fn a_page_paints_its_own_strip_and_not_the_next_pages() {
        let (fonts, resources) = painter();
        let mut painter = Painter::new(&fonts, &resources);
        let mut pixmap = Pixmap::new(100, 100).unwrap();

        painter.paint(
            &ruby_pages(),
            Rect::new(0.0, 0.0, 100.0, 50.0),
            (0.0, 10.0),
            1.0,
            &mut pixmap.as_mut(),
        );

        // The strip of the line this page starts with, ten dots above the
        // window's own edge.
        assert_eq!(pixmap.pixel(15, 5).unwrap().red(), 255);
        // The strip of the line the page after it starts with.
        assert_eq!(pixmap.pixel(15, 55).unwrap().alpha(), 0);
    }
}

/// A glyph's transform without its position: what a sprite is baked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Shape {
    sx: u32,
    ky: u32,
    kx: u32,
    sy: u32,
}

impl Shape {
    fn transform(self) -> Transform {
        Transform::from_row(
            f32::from_bits(self.sx),
            f32::from_bits(self.ky),
            f32::from_bits(self.kx),
            f32::from_bits(self.sy),
            0.0,
            0.0,
        )
    }
}

/// One glyph rasterized at one shape and colour.
struct Sprite {
    pixmap: Pixmap,
    /// Where the sprite's own corner sits against the glyph's origin.
    dx: f32,
    dy: f32,
}

/// The widest sprite kept. A larger glyph takes the path straight to the page.
const SPRITE_LIMIT: u32 = 320;

impl Sprite {
    fn of(path: &tiny_skia::Path, shape: Shape, color: Color) -> Option<Sprite> {
        let placed = path.clone().transform(shape.transform())?;
        let bounds = placed.bounds();
        let dx = bounds.left().floor();
        let dy = bounds.top().floor();
        let width = (bounds.right().ceil() - dx).max(1.0) as u32;
        let height = (bounds.bottom().ceil() - dy).max(1.0) as u32;
        if width > SPRITE_LIMIT || height > SPRITE_LIMIT {
            return None;
        }
        let mut pixmap = Pixmap::new(width, height)?;
        let mut paint = Paint {
            anti_alias: true,
            ..Default::default()
        };
        paint.set_color_rgba8(color.r, color.g, color.b, color.a);
        pixmap.fill_path(
            &placed,
            &paint,
            FillRule::Winding,
            Transform::from_translate(-dx, -dy),
            None,
        );
        Some(Sprite { pixmap, dx, dy })
    }
}

fn colour_of(color: Color) -> u32 {
    u32::from_be_bytes([color.r, color.g, color.b, color.a])
}
