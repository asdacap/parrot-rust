//! Focused T57 wrappers over the real T40 execution and T41 scoped owner.
//!
//! These tests require an explicit shell (`T57_TEST_SHELL`). PTY tests also
//! require `T57_PTY_HELPER`; the fixture never substitutes a pipe for a PTY.

mod input;
mod wrappers;
