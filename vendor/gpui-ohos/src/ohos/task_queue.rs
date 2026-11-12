//! Priority work queue with an explicit shutdown boundary.
use crate::Priority;
use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

struct State<T> {
    queues: [VecDeque<T>; 3],
    cursor: usize,
    closed: bool,
    started: u64,
    wake_on_progress: bool,
}

pub(crate) struct TaskQueue<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
    changed: Condvar,
}

pub(crate) struct StallBackoff {
    interval: Duration,
    started: u64,
}

impl Default for StallBackoff {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(10),
            started: 0,
        }
    }
}

impl StallBackoff {
    pub(crate) fn reset(&mut self) {
        self.interval = Duration::from_millis(10);
    }

    pub(crate) fn back_off(&mut self) {
        self.interval = (self.interval * 2).min(Duration::from_millis(100));
    }
}

impl<T> Default for TaskQueue<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                queues: std::array::from_fn(|_| VecDeque::new()),
                cursor: 0,
                closed: false,
                started: 0,
                wake_on_progress: false,
            }),
            ready: Condvar::new(),
            changed: Condvar::new(),
        }
    }
}

impl<T> TaskQueue<T> {
    pub(crate) fn push(&self, priority: Priority, task: T) -> Result<(), T> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(task);
        }
        let index = match priority {
            Priority::High => 0,
            Priority::Medium => 1,
            Priority::Low => 2,
            Priority::RealtimeAudio => unreachable!("realtime work has a dedicated thread"),
        };
        let was_empty = state.queues.iter().all(VecDeque::is_empty);
        state.queues[index].push_back(task);
        self.ready.notify_one();
        if was_empty {
            self.changed.notify_one();
        }
        Ok(())
    }

    fn take(state: &mut State<T>) -> Option<T> {
        // GPUI's priority weights are 60/30/10. FIFO service with the same
        // proportions also guarantees that low-priority work makes progress.
        const ORDER: [usize; 10] = [0, 0, 0, 0, 0, 0, 1, 1, 1, 2];
        for _ in 0..ORDER.len() {
            let index = ORDER[state.cursor];
            state.cursor = (state.cursor + 1) % ORDER.len();
            if let Some(task) = state.queues[index].pop_front() {
                state.started = state.started.wrapping_add(1);
                return Some(task);
            }
        }
        None
    }

    pub(crate) fn pop(&self) -> Option<T> {
        self.pop_until(None)
    }

    pub(crate) fn pop_timeout(&self, timeout: Duration) -> Option<T> {
        self.pop_until(Some(Instant::now() + timeout))
    }

    fn pop_until(&self, deadline: Option<Instant>) -> Option<T> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(task) = Self::take(&mut state) {
                if state.wake_on_progress {
                    state.wake_on_progress = false;
                    self.changed.notify_one();
                }
                return Some(task);
            }
            if state.closed {
                return None;
            }
            state = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return None;
                    }
                    self.ready
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.ready.wait(state).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }

    // The monitor sleeps when there is no backlog. Stalling requests an OS-state
    // observation; it does not by itself authorize another worker.
    pub(crate) fn wait_for_stalled_work(&self, backoff: &mut StallBackoff) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            while !state.closed && state.queues.iter().all(VecDeque::is_empty) {
                backoff.reset();
                state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.closed {
                return false;
            }
            if backoff.started != state.started {
                backoff.reset();
                backoff.started = state.started;
            }
            state.wake_on_progress = backoff.interval > Duration::from_millis(10);
            let (next, timeout) = self
                .changed
                .wait_timeout(state, backoff.interval)
                .unwrap_or_else(|e| e.into_inner());
            state = next;
            state.wake_on_progress = false;
            if state.closed {
                return false;
            }
            if timeout.timed_out()
                && backoff.started == state.started
                && state.queues.iter().any(|queue| !queue.is_empty())
            {
                return true;
            }
        }
    }

    pub(crate) fn close(&self) -> Vec<T> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        let remaining = state
            .queues
            .iter_mut()
            .flat_map(|queue| queue.drain(..))
            .collect();
        self.ready.notify_all();
        self.changed.notify_all();
        remaining
    }
}
