pub(crate) struct LaunchState {
    surface_ready: bool,
    callback: Option<Box<dyn FnOnce()>>,
}

impl LaunchState {
    pub(crate) fn new(callback: Box<dyn FnOnce()>) -> Self {
        Self {
            surface_ready: false,
            callback: Some(callback),
        }
    }

    pub(crate) fn set_surface_ready(&mut self, ready: bool) {
        self.surface_ready = ready;
    }

    pub(crate) fn cancel(&mut self) {
        self.callback.take();
    }

    pub(crate) fn take_callback(&mut self, gpu_ready: bool) -> Option<Box<dyn FnOnce()>> {
        if self.surface_ready && gpu_ready {
            self.callback.take()
        } else {
            None
        }
    }
}
