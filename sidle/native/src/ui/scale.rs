//! Physical sizing. Every layout constant in this crate is a design pixel at
//! [`DESIGN_DPI`]; [`Scale`] puts one on the panel being drawn, keyed on the
//! framebuffer width.

/// The density every design pixel is written at.
pub const DESIGN_DPI: i32 = 300;

/// Panel density by framebuffer width, for the widths that are not
/// [`DESIGN_DPI`]. A width absent from this table is [`DESIGN_DPI`].
const PANELS: &[(u32, i32)] = &[(600, 167), (758, 212)];

/// What one design pixel is worth on the panel being drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scale {
    dpi: i32,
}

impl Scale {
    /// The density [`PANELS`] gives `xres`, else [`DESIGN_DPI`].
    pub fn of_width(xres: u32) -> Self {
        let dpi = PANELS
            .iter()
            .find(|(width, _)| *width == xres)
            .map(|(_, dpi)| *dpi)
            .unwrap_or(DESIGN_DPI);
        Self { dpi }
    }

    pub fn dpi(self) -> i32 {
        self.dpi
    }

    /// `design` in device pixels, keeping its sign. A non-zero `design` answers
    /// at least 1.
    pub fn px(self, design: i32) -> i32 {
        match design {
            0 => 0,
            _ => {
                let scaled = (design.abs() * self.dpi / DESIGN_DPI).max(1);
                match design < 0 {
                    true => -scaled,
                    false => scaled,
                }
            }
        }
    }

    /// [`Scale::px`] for an unsigned length.
    pub fn u(self, design: u32) -> u32 {
        self.px(design as i32).max(0) as u32
    }

    /// [`Scale::px`] for a type size, keeping its fraction.
    pub fn font(self, design: f32) -> f32 {
        design * self.dpi as f32 / DESIGN_DPI as f32
    }
}

#[cfg(test)]
mod tests {
    use super::{DESIGN_DPI, Scale};

    /// Every shipped framebuffer width.
    pub(crate) const WIDTHS: [u32; 7] = [600, 758, 1072, 1236, 1264, 1860, 2400];

    /// [`Scale::of_width`] over `PANELS`, and over widths absent from it.
    #[test]
    fn the_shipped_widths_carry_their_own_density() {
        assert_eq!(Scale::of_width(600).dpi(), 167);
        assert_eq!(Scale::of_width(758).dpi(), 212);
        for wide in [1072, 1236, 1264, 1272, 1860, 2400] {
            assert_eq!(Scale::of_width(wide).dpi(), DESIGN_DPI, "{wide}");
        }
        // A width absent from `PANELS`.
        assert_eq!(Scale::of_width(999).dpi(), DESIGN_DPI);
    }

    /// [`Scale::px`] answers `design` at [`DESIGN_DPI`] and less below it.
    #[test]
    fn a_design_pixel_shrinks_with_the_panel() {
        let oasis = Scale::of_width(1264);
        assert_eq!(oasis.px(360), 360);
        assert_eq!(oasis.font(28.0), 28.0);

        let pw2 = Scale::of_width(758);
        assert_eq!(pw2.px(360), 360 * 212 / 300);
        assert_eq!(pw2.px(80), 56);

        let basic = Scale::of_width(600);
        assert_eq!(basic.px(360), 360 * 167 / 300);
    }

    /// [`Scale::px`] answers 1 where the product rounds to 0, and 0 for 0.
    #[test]
    fn a_rule_survives_the_smallest_panel() {
        let basic = Scale::of_width(600);
        assert_eq!(basic.px(1), 1);
        assert_eq!(basic.px(2), 1);
        assert_eq!(basic.px(0), 0);
        assert_eq!(basic.u(0), 0);
        assert_eq!(basic.u(2), 1);
    }

    /// [`Scale::px`] keeps the sign of `design`.
    #[test]
    fn a_signed_constant_scales_both_ways() {
        let pw2 = Scale::of_width(758);
        assert_eq!(pw2.px(-44), -pw2.px(44));
        assert_eq!(pw2.px(-1), -1);
    }

    /// [`Scale::font`] answers one physical size across `WIDTHS`.
    #[test]
    fn a_body_line_is_one_size_on_every_panel() {
        // Within a fiftieth of an inch of 28 px at `DESIGN_DPI`.
        let want = 28.0 / DESIGN_DPI as f32;
        for width in WIDTHS {
            let scale = Scale::of_width(width);
            let inches = scale.font(28.0) / scale.dpi() as f32;
            assert!((inches - want).abs() < 0.02, "{width}: {inches}″");
        }
    }
}
