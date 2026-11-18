//! `lwk serve`'s workers: a fixed number of threads taking jobs from one
//! bounded queue. No async runtime; a job is one kit call, start to end.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

/// A worker's stack: a structural search parses deep trees on the thread
/// that asked for it (`STACK_BYTES` in `engine/structural.rs`).
const STACK_BYTES: usize = 16 * 1024 * 1024;

pub type Job = Box<dyn FnOnce() + Send + 'static>;

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
}

pub struct Pool {
    shared: Arc<Shared>,
    /// The most jobs that may wait for a worker.
    waiting_max: usize,
}

impl Pool {
    /// Start `workers` threads; None when not one could be started.
    pub fn new(workers: usize, waiting_max: usize) -> Option<Pool> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
        });
        let mut started = 0;
        for at in 0..workers {
            let shared = shared.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("lwk-serve-{at}"))
                .stack_size(STACK_BYTES)
                .spawn(move || work(&shared));
            started += usize::from(spawned.is_ok());
        }
        (started > 0).then_some(Pool { shared, waiting_max })
    }

    /// Queue `job` for the next free worker; false when too many wait
    /// already, and the job is dropped.
    pub fn submit(&self, job: Job) -> bool {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() >= self.waiting_max {
            return false;
        }
        queue.push_back(job);
        drop(queue);
        self.shared.ready.notify_one();
        true
    }
}

/// One worker: run jobs as they come, for the life of the process.
fn work(shared: &Shared) {
    loop {
        let job = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                match queue.pop_front() {
                    Some(job) => break job,
                    None => queue = shared.ready.wait(queue).unwrap_or_else(|e| e.into_inner()),
                }
            }
        };
        job();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn runs_jobs_and_refuses_past_the_queue() {
        let pool = Pool::new(1, 1).unwrap();
        let (hold, held) = mpsc::channel::<()>();
        let (started, has_started) = mpsc::channel::<()>();
        let (done, results) = mpsc::channel::<u8>();
        let first = done.clone();
        assert!(pool.submit(Box::new(move || {
            started.send(()).unwrap();
            held.recv().unwrap();
            first.send(1).unwrap();
        })));
        // The one worker is busy: one job may wait, the next may not.
        has_started.recv().unwrap();
        let second = done.clone();
        assert!(pool.submit(Box::new(move || second.send(2).unwrap())));
        assert!(!pool.submit(Box::new(move || done.send(3).unwrap())));
        hold.send(()).unwrap();
        assert_eq!(results.recv().unwrap(), 1);
        assert_eq!(results.recv().unwrap(), 2);
    }
}
