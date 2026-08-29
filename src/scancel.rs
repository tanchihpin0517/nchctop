//! Cancelling a job.
//!
//! `scancel` on its own thread, like every other cluster command here: it is
//! usually quick, but the draw loop must not be the thing that finds out it
//! was not.

use std::io;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// Cancellations asked for, and what they came to.
///
/// A cancellation happens once rather than on a timer, so unlike a
/// [`Poller`](crate::poller::Poller) this owns no thread of its own: each
/// request gets one that ends with the command.
pub struct Cancels {
    /// What a request actually runs. A field rather than a call, so the tests
    /// can watch the flow without a cluster to cancel anything on.
    run: fn(&str) -> io::Result<()>,
    /// Cloned into each worker, and kept here so the receiver stays connected
    /// while nothing is in flight.
    outgoing: Sender<(String, io::Result<()>)>,
    results: Receiver<(String, io::Result<()>)>,
}

impl Cancels {
    /// Cancellations that ask Slurm.
    pub fn new() -> Self {
        Self::running(cancel)
    }

    /// The same, over some other command — how a test watches the flow
    /// without asking a cluster to cancel anything.
    pub(crate) fn running(run: fn(&str) -> io::Result<()>) -> Self {
        let (outgoing, results) = mpsc::channel();

        Self {
            run,
            outgoing,
            results,
        }
    }

    /// Ask for job `id` to be cancelled, in the background.
    pub fn request(&self, id: String) {
        let run = self.run;
        let outgoing = self.outgoing.clone();

        thread::spawn(move || {
            let result = run(&id);
            // A send that fails only means the app quit first.
            let _ = outgoing.send((id, result));
        });
    }

    /// Every cancellation that has finished since the last call, oldest first.
    pub fn drain(&self) -> impl Iterator<Item = (String, io::Result<()>)> {
        self.results.try_iter()
    }
}

/// Run `scancel` for one job.
///
/// Slurm takes the cancellation and the job leaves the queue on its own time,
/// so a command that returns without complaint is as much as there is to
/// know; the queue pane reports the rest a fetch later.
fn cancel(id: &str) -> io::Result<()> {
    let output = Command::new("scancel").arg(id).output()?;

    if output.status.success() {
        return Ok(());
    }

    Err(io::Error::other(
        reason(&String::from_utf8_lossy(&output.stderr))
            .unwrap_or_else(|| format!("scancel exited with {}", output.status)),
    ))
}

/// The part of `scancel`'s complaint worth a footer.
///
/// It prefixes its own name to every line and can print more than one — the
/// last is the one that says what went wrong, and its prefix only repeats the
/// word already sitting next to it on screen.
fn reason(stderr: &str) -> Option<String> {
    let line = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())?;

    Some(
        line.trim_start_matches("scancel: ")
            .trim_start_matches("error: ")
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Generous enough that a loaded test machine does not fail the suite.
    const PATIENCE: Duration = Duration::from_secs(30);

    /// Waits for one result, rather than draining whatever has arrived by now:
    /// the worker is a thread, so an immediate drain is a race.
    fn awaited(cancels: &Cancels) -> (String, io::Result<()>) {
        cancels.results.recv_timeout(PATIENCE).expect("a result")
    }

    #[test]
    fn reports_the_job_it_cancelled() {
        let cancels = Cancels::running(|_| Ok(()));
        cancels.request("308208".to_string());

        let (id, result) = awaited(&cancels);

        assert_eq!(id, "308208");
        assert!(result.is_ok());
    }

    /// A cancellation Slurm refused is the whole reason results come back at
    /// all, so it has to arrive tagged with the job it was about.
    #[test]
    fn reports_one_it_could_not() {
        let cancels = Cancels::running(|_| Err(io::Error::other("Access/permission denied")));
        cancels.request("308208".to_string());

        let (id, result) = awaited(&cancels);

        assert_eq!(id, "308208");
        assert_eq!(
            result.expect_err("refused").to_string(),
            "Access/permission denied"
        );
    }

    #[test]
    fn takes_the_last_line_of_a_complaint() {
        let stderr = "scancel: error: Kill job error on job id 308208: \
                      Access/permission denied\n";

        assert_eq!(
            reason(stderr).expect("a reason"),
            "Kill job error on job id 308208: Access/permission denied"
        );
    }

    /// Nothing on stderr leaves the exit status to speak for it, which is the
    /// caller's line rather than this one's.
    #[test]
    fn finds_no_reason_in_silence() {
        assert_eq!(reason(""), None);
        assert_eq!(reason("  \n\n"), None);
    }
}
