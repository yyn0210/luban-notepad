pub(super) struct CursorVisibility {
    visible: bool,
    applied: Option<bool>,
    pending: bool,
    supported: bool,
}

impl Default for CursorVisibility {
    fn default() -> Self {
        Self {
            visible: true,
            applied: Some(true),
            pending: false,
            supported: true,
        }
    }
}

impl CursorVisibility {
    pub(super) fn is_visible(&self) -> bool {
        self.visible
    }

    pub(super) fn request(&mut self, visible: bool) -> Option<bool> {
        if !self.supported {
            return None;
        }
        self.visible = visible;
        if self.pending || self.applied == Some(visible) {
            return None;
        }
        self.pending = true;
        Some(visible)
    }

    pub(super) fn complete(&mut self, visible: bool, succeeded: bool) -> Option<bool> {
        self.pending = false;
        self.applied = succeeded.then_some(visible);
        // A failed native request must remain retryable on the next input, but
        // must not create an unbounded retry loop while the service is failing.
        if !succeeded && self.visible == visible {
            return None;
        }
        self.request(self.visible)
    }

    pub(super) fn mark_unsupported(&mut self) {
        self.supported = false;
        self.pending = false;
        self.visible = true;
        self.applied = None;
    }
}
