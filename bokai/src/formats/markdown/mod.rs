//! Pure markdown generation from IR.

mod escape;
mod render;
mod slugify;

pub use escape::{calculate_fence_length, calculate_inline_code_ticks, escape_markdown};
pub use render::{Footnote, RenderContext, RenderResult, render_chapter};
pub use slugify::{build_heading_slugs, collect_heading_text, slugify};
