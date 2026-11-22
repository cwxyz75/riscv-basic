//! What a boot's kernel-side checks report (shared by the test boot and the interactive one).

use alloc::string::String;

pub type Outcome = Result<(), String>;

pub struct Report {
    /// (name, outcome) of each kernel-side test.
    pub tests: alloc::vec::Vec<(String, Outcome)>,
    /// `##TEST` lines the user suite reported: (pass, fail), already counted by `user`.
    pub user_reported: (u32, u32),
}
