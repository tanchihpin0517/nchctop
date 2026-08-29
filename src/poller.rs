//! Background polling.
//!
//! Each [`Poller`] owns a thread that re-runs one fetch on a timer and hands
//! the results back over a channel, so the draw loop never blocks on a cluster
//! command. Sources are independent: they each pick their own interval, and a
//! slow one cannot hold up the rest.

use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

/// A fetch running on its own thread, repeated every interval.
///
/// `Req` is whatever the fetch needs to know — a scope, a partition, a job id.
/// It is sent back alongside each result so the UI can drop one that a later
/// [`Poller::request`] has already made stale.
pub struct Poller<Req, Out> {
    /// Immediate-fetch requests, carrying the `Req` to use from then on.
    /// Dropping this ends the worker thread.
    requests: Sender<Req>,
    results: Receiver<(Req, io::Result<Out>)>,
}

impl<Req, Out> Poller<Req, Out> {
    /// Start polling every `interval`, with one fetch straight away.
    ///
    /// The interval is the gap *between* fetches, not the period: a slow
    /// command backs itself off instead of piling up overlapping runs.
    ///
    /// Only spawning needs anything of `Req` and `Out`; holding a `Poller` and
    /// reading from it does not, so a caller generic over the row type is not
    /// forced to repeat these bounds.
    pub fn spawn<F>(interval: Duration, initial: Req, fetch: F) -> Self
    where
        Req: Clone + Send + 'static,
        Out: Send + 'static,
        F: Fn(Req) -> io::Result<Out> + Send + 'static,
    {
        let (requests, incoming) = mpsc::channel();
        let (outgoing, results) = mpsc::channel();

        thread::spawn(move || {
            let mut request = initial;

            loop {
                let result = fetch(request.clone());
                if outgoing.send((request.clone(), result)).is_err() {
                    return; // The UI is gone.
                }

                match incoming.recv_timeout(interval) {
                    // Somebody asked for a fetch now; loop round without waiting.
                    Ok(next) => request = next,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });

        Self { requests, results }
    }

    /// Ask for a fetch right away, and for `request` to apply to later polls.
    pub fn request(&self, request: Req) {
        // A dead worker only means the rows stop updating, which the caller
        // cannot do anything about, so there is nothing to report.
        let _ = self.requests.send(request);
    }

    /// Everything that has arrived since the last call, oldest first.
    pub fn drain(&self) -> impl Iterator<Item = (Req, io::Result<Out>)> {
        self.results.try_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUICK: Duration = Duration::from_millis(20);

    /// Generous enough that a loaded test machine does not fail the suite.
    const PATIENCE: Duration = Duration::from_secs(30);

    /// Reports every fetch on the returned channel, so a test can watch the
    /// worker without depending on any real command.
    fn counted() -> (Sender<u32>, Receiver<u32>) {
        mpsc::channel()
    }

    #[test]
    fn fetches_immediately_and_then_on_the_interval() {
        let (calls, seen) = counted();
        let poller = Poller::spawn(QUICK, 0u32, move |req| {
            calls.send(req).map_err(io::Error::other)?;
            Ok(req)
        });

        for _ in 0..3 {
            seen.recv_timeout(PATIENCE).expect("fetch");
        }

        // Results carry the request they were fetched under.
        let (req, out) = poller.drain().next().expect("result");
        assert_eq!(req, 0);
        assert_eq!(out.expect("ok"), 0);
    }

    #[test]
    fn a_request_switches_the_parameter() {
        let (calls, seen) = counted();
        let poller = Poller::spawn(PATIENCE, 1u32, move |req| {
            calls.send(req).map_err(io::Error::other)?;
            Ok(req)
        });

        assert_eq!(seen.recv_timeout(PATIENCE).expect("first fetch"), 1);

        // The interval is far longer than the test's patience, so a second
        // fetch can only mean the request cut the wait short.
        poller.request(2);
        assert_eq!(seen.recv_timeout(PATIENCE).expect("fetch on request"), 2);
    }

    #[test]
    fn dropping_the_poller_stops_the_thread() {
        let (calls, seen) = counted();
        let poller = Poller::spawn(QUICK, 0u32, move |req| {
            calls.send(req).map_err(io::Error::other)?;
            Ok(req)
        });

        seen.recv_timeout(PATIENCE).expect("first fetch");
        drop(poller);

        // The worker owns `calls`, so the channel disconnects only once the
        // thread has actually returned. Fetches already in flight may land
        // first, hence the loop.
        let stopped = loop {
            match seen.recv_timeout(PATIENCE) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Disconnected) => break true,
                Err(RecvTimeoutError::Timeout) => break false,
            }
        };

        assert!(stopped, "worker thread outlived the poller");
    }
}
