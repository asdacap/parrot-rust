//! The prelude evaluated inside the VM before each script.
//!
//! Adapted from pi's `prelude-source.ts`
//! (~/repo/pi/packages/codemode, MIT licensed). The VM owns its built-ins, so
//! the prelude freezes them and keeps the host bridge in a closure. It builds
//! `tools`, `ALL_TOOLS`, `text`, `console`, `exit`, `store`, and `load` on top
//! of the bridge, and hands back `settle`, `run`, `stalled`, and `describe`.
//! Placeholders are replaced with the limits from [`crate::values`].

/// The prelude source with the limit constants substituted.
pub fn prelude_source() -> String {
    PRELUDE_TEMPLATE
        .replace(
            "__MAX_STORE_VALUE_CHARS__",
            &format!("{}", crate::values::MAX_STORE_VALUE_CHARS),
        )
        .replace(
            "__MAX_STORE_TOTAL_CHARS__",
            &format!("{}", crate::values::MAX_STORE_TOTAL_CHARS),
        )
        .replace(
            "__MAX_OUTPUT_CHARS__",
            &format!("{}", crate::values::MAX_OUTPUT_CHARS),
        )
        .replace(
            "__MAX_OUTPUT_ITEMS__",
            &format!("{}", crate::values::MAX_OUTPUT_ITEMS),
        )
}

const PRELUDE_TEMPLATE: &str = include_str!("prelude.js");
