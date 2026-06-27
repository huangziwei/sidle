//! EPUB format support - pure parsing functions.

pub(crate) mod parser;
mod zip_repair;

pub use parser::{
    OpfData, parse_container_xml, parse_nav_landmarks, parse_nav_toc, parse_ncx, parse_opf,
    parse_opf_guide, strip_bom,
};
pub use zip_repair::neutralize_spurious_zip64;
