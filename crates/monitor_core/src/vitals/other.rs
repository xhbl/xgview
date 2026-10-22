//! Everywhere else: what the target calls itself, and nothing it cannot read.

pub(super) struct Platform;

impl Platform {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn os(&self) -> &str {
        std::env::consts::OS
    }

    pub(super) fn memory(&self) -> Option<(u64, u64)> {
        None
    }

    pub(super) fn cpu(&mut self) -> Option<f32> {
        None
    }
}
