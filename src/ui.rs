//! Centralized UI/UX components (Mosaic Design Mode).

use comfy_table::{Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL};

/// Create a new visually consistent CLI table according to the Mosaic design guidelines.
/// Enforces Z-Pattern visual hierarchy with rounded corners and full borders.
#[must_use]
pub fn create_table() -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS);
    table
}
