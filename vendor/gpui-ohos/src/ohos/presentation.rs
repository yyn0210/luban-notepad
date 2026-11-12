use std::cell::Cell;

/// Result of a platform presentation attempt. A transient acquisition failure
/// retains demand until the next frame; an unavailable surface waits for input
/// or lifecycle events instead of spinning on a hidden or invalid surface.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presentation {
    Presented,
    Retry,
    Unavailable,
}

pub(crate) struct PresentationDemand {
    pending: Cell<bool>,
}

impl Default for PresentationDemand {
    fn default() -> Self {
        Self {
            pending: Cell::new(true),
        }
    }
}

impl PresentationDemand {
    pub(crate) fn request(&self) {
        self.pending.set(true);
    }

    pub(crate) fn needs_presentation(&self) -> bool {
        self.pending.get()
    }

    /// Returns whether the next frame should be requested.
    pub(crate) fn record(&self, presentation: Presentation) -> bool {
        let retry = presentation == Presentation::Retry;
        self.pending.set(retry);
        retry
    }
}
