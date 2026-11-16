//! A fixed set of worker threads for blocking work (HTTP calls, image decoding).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

type Job = Box<dyn FnOnce() + Send + 'static>;

struct Inner {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    /// Newest job first. Right for artwork, where the most recent request is what's on screen.
    newest_first: bool,
}

#[derive(Clone)]
pub struct Pool {
    inner: Arc<Inner>,
}

impl Pool {
    pub fn new(name: &str, threads: usize, newest_first: bool) -> Pool {
        let inner =
            Arc::new(Inner { queue: Mutex::new(VecDeque::new()), ready: Condvar::new(), newest_first });
        for index in 0..threads {
            let inner = inner.clone();
            std::thread::Builder::new()
                .name(format!("{name}-{index}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let mut queue = inner.queue.lock().unwrap();
                            loop {
                                let next = if inner.newest_first {
                                    queue.pop_back()
                                } else {
                                    queue.pop_front()
                                };
                                match next {
                                    Some(job) => break job,
                                    None => queue = inner.ready.wait(queue).unwrap(),
                                }
                            }
                        };
                        job();
                    }
                })
                .expect("failed to start a worker thread");
        }
        Pool { inner }
    }

    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        self.inner.queue.lock().unwrap().push_back(Box::new(job));
        self.inner.ready.notify_one();
    }
}
