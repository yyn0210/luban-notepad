pub(crate) const IDLE_CALLBACK_THRESHOLD: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryChange {
    Register,
    Unregister,
}

pub(crate) struct FrameDemand {
    active: bool,
    registered: bool,
    demanded: bool,
    idle_callbacks: usize,
    pending: Option<DeliveryChange>,
    unregister_failed: bool,
}

impl Default for FrameDemand {
    fn default() -> Self {
        Self {
            active: false,
            registered: false,
            demanded: false,
            idle_callbacks: 0,
            pending: Some(DeliveryChange::Unregister),
            unregister_failed: false,
        }
    }
}

impl FrameDemand {
    pub(crate) fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            self.request();
        } else {
            self.demanded = false;
            self.pending = (!self.unregister_failed).then_some(DeliveryChange::Unregister);
        }
    }

    pub(crate) fn surface_destroyed(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn request(&mut self) -> Option<DeliveryChange> {
        if !self.active {
            return None;
        }
        self.demanded = true;
        self.idle_callbacks = 0;
        self.pending = (!self.registered).then_some(DeliveryChange::Register);
        self.pending
    }

    pub(crate) fn on_callback(&mut self) -> Option<DeliveryChange> {
        // A callback is authoritative even when ability restored a retained registration.
        self.registered = true;
        if self.pending == Some(DeliveryChange::Register) {
            self.pending = None;
        }
        if std::mem::take(&mut self.demanded) {
            self.idle_callbacks = 0;
        } else {
            self.idle_callbacks = self.idle_callbacks.saturating_add(1);
        }
        if (!self.active || self.idle_callbacks >= IDLE_CALLBACK_THRESHOLD)
            && !self.unregister_failed
        {
            self.pending = Some(DeliveryChange::Unregister);
        }
        self.pending
    }

    pub(crate) fn pending(&self) -> Option<DeliveryChange> {
        self.pending
    }

    pub(crate) fn complete(&mut self, change: DeliveryChange, success: bool) {
        self.pending = None;
        match change {
            DeliveryChange::Register => {
                self.registered = success;
            }
            DeliveryChange::Unregister => {
                self.registered = !success;
                self.unregister_failed = !success;
            }
        }
    }
}
