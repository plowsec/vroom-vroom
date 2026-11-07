//! Capabilities of the selected board's lifecycle, independent of run state.
use crate::{Error, Result, StartMode};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum LifecyclePolicy {
    /// Existing ARM boards retain their pristine state and reset/snapshot paths.
    #[default]
    Resettable,
    /// Reserved for boards that can only start fresh. No saved state is taken.
    #[allow(dead_code)]
    ColdBootOnly,
}

impl LifecyclePolicy {
    pub(crate) fn require_state(self, operation: &'static str) -> Result<()> {
        match self {
            Self::Resettable => Ok(()),
            Self::ColdBootOnly => Err(Error::UnsupportedLifecycle(operation)),
        }
    }

    pub(crate) fn check_start(self, start: &StartMode) -> Result<()> {
        if matches!(start, StartMode::Restore(_) | StartMode::RestorePaused(_)) {
            self.require_state("restore")?;
        }
        Ok(())
    }
}
