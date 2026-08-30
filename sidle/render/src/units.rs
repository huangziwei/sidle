//! Device dots, and what a declared value is worth in one.
//!
//! [`Metrics`] holds a panel resolution and the resolution the source states
//! its absolute lengths against, and converts between them. The second is a
//! property of the source format: [`bokai::style::Length::Px`] holds a CSS
//! pixel from EPUB and a [`KFX_LENGTH_DPI`] dot from KFX.

/// Dots per inch a CSS pixel is defined at.
pub const CSS_DPI: f32 = 96.0;

/// Dots per inch a KFX absolute length is stated against.
pub const KFX_LENGTH_DPI: f32 = 160.0;

/// Points per inch.
pub const PT_PER_INCH: f32 = 72.0;

/// Panel resolution of the Kindle Colorsoft and the Kindle Scribe.
pub const KINDLE_PANEL_DPI: f32 = 300.0;

/// How a declared value becomes a device dot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// Resolution of the panel being laid out for.
    pub dpi: f32,
    /// Resolution the source's absolute lengths are stated against.
    pub length_dpi: f32,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::css(CSS_DPI)
    }
}

impl Metrics {
    pub const fn new(dpi: f32, length_dpi: f32) -> Self {
        Self { dpi, length_dpi }
    }

    /// Metrics for a source whose absolute lengths are CSS pixels.
    pub const fn css(dpi: f32) -> Self {
        Self::new(dpi, CSS_DPI)
    }

    /// Metrics for a source whose absolute lengths came from KFX.
    pub const fn kfx(dpi: f32) -> Self {
        Self::new(dpi, KFX_LENGTH_DPI)
    }

    /// Metrics for a KFX book on a Kindle panel.
    pub const fn kindle() -> Self {
        Self::kfx(KINDLE_PANEL_DPI)
    }

    /// What one of the source's absolute length units is worth in dots.
    pub fn length_scale(&self) -> f32 {
        self.dpi / self.length_dpi
    }

    /// An absolute length as the IR carries it, in dots.
    pub fn length(&self, value: f32) -> f32 {
        value * self.length_scale()
    }

    /// A typographic point at the panel's resolution, in dots.
    pub fn points(&self, pt: f32) -> f32 {
        pt * self.dpi / PT_PER_INCH
    }

    /// A CSS pixel, in dots. For a size CSS states itself, which is the same
    /// length at every `length_dpi`.
    pub fn css_px(&self, value: f32) -> f32 {
        value * self.dpi / CSS_DPI
    }

    /// One pixel of a raster resource, in dots: one dot each.
    pub fn image_px(&self, value: f32) -> f32 {
        value
    }

    /// An inch, in dots.
    pub fn inches(&self, inches: f32) -> f32 {
        inches * self.dpi
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_lengths_are_dots_only_on_a_96_dpi_panel() {
        assert_eq!(Metrics::css(CSS_DPI).length(10.0), 10.0);
        assert_eq!(Metrics::css(192.0).length(10.0), 20.0);
    }

    #[test]
    fn a_kfx_length_is_a_dot_on_a_160_dpi_panel() {
        assert_eq!(Metrics::kfx(KFX_LENGTH_DPI).length(10.0), 10.0);
    }

    #[test]
    fn the_same_declared_length_is_two_sizes_by_provenance() {
        // One inch: 96 CSS pixels, or 160 dots at `KFX_LENGTH_DPI`.
        let panel = KINDLE_PANEL_DPI;
        assert_eq!(Metrics::css(panel).length(96.0), panel);
        assert_eq!(Metrics::kfx(panel).length(160.0), panel);
    }

    #[test]
    fn a_resources_pixels_are_dots_at_every_resolution() {
        assert_eq!(Metrics::kindle().image_px(200.0), 200.0);
        assert_eq!(Metrics::css(CSS_DPI).image_px(200.0), 200.0);
    }

    #[test]
    fn a_point_is_the_panel_over_seventy_two() {
        // An em is the stop times dpi/72.
        assert_eq!(Metrics::kindle().points(72.0), KINDLE_PANEL_DPI);
        assert!((Metrics::kindle().points(10.78) - 44.9166).abs() < 1e-3);
    }
}
