//! Boot liveness for tests that play several boots of one runtime inside a
//! single process. A boot the test has not declared is dead — the shape of a
//! restart, where the previous boot is gone.

use std::collections::HashMap;
use std::sync::Mutex;

use leveler_core::{BootId, BootLiveness, BootLivenessProbe};

#[derive(Default)]
pub struct TestBoots {
    declared: Mutex<HashMap<BootId, BootLiveness>>,
}

impl TestBoots {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare what the probe answers for `boot`.
    pub fn set(&self, boot: &BootId, liveness: BootLiveness) {
        self.declared.lock().unwrap().insert(boot.clone(), liveness);
    }
}

impl BootLivenessProbe for TestBoots {
    fn liveness(&self, boot: &BootId) -> BootLiveness {
        self.declared
            .lock()
            .unwrap()
            .get(boot)
            .copied()
            .unwrap_or(BootLiveness::Dead)
    }
}
