use std::io;
use std::process::Command;
use std::time::Duration;

use crate::poller::Poller;

/// The sacct fields we ask for, in the order the table renders them.
const FORMAT: &str = "JobID,Partition,JobName,User,State,Elapsed,End,ExitCode";

/// How far back the recent view looks.
const WINDOW: &str = "now-1days";

/// How often to re-run `sacct`. Much slower than squeue: the accounting
/// database is heavier to query and finished jobs never change again.
const INTERVAL: Duration = Duration::from_secs(30);

/// One job from the accounting database, finished or still going.
pub struct Run {
    pub id: String,
    pub partition: String,
    pub name: String,
    pub user: String,
    pub state: String,
    pub elapsed: String,
    pub end: String,
    pub exit: String,
}

impl Run {
    /// Parse one `--parsable2` line. Returns `None` for a line that does not
    /// have every field, so one malformed row cannot take the whole refresh
    /// down.
    fn parse(line: &str) -> Option<Self> {
        let fields: Vec<&str> = line.splitn(8, '|').collect();
        let [id, partition, name, user, state, elapsed, end, exit] = fields[..] else {
            return None;
        };

        Some(Self {
            id: id.trim().to_string(),
            partition: partition.trim().to_string(),
            name: name.trim().to_string(),
            user: user.trim().to_string(),
            state: state_name(state.trim()),
            elapsed: elapsed.trim().to_string(),
            end: short_time(end.trim()),
            exit: exit.trim().to_string(),
        })
    }
}

/// Slurm reports a cancelled job as `CANCELLED by 12345`. The uid that did it
/// is rarely what you are scanning for and it doubles the width of the column,
/// so keep just the state itself.
fn state_name(state: &str) -> String {
    match state.split_once(" by ") {
        Some((name, _)) => name.to_string(),
        None => state.to_string(),
    }
}

/// `2026-08-28T19:59:28` is more precision than the column has room for. A
/// job that has not finished reports something else entirely ("Unknown"),
/// which passes through untouched.
fn short_time(stamp: &str) -> String {
    let Some((date, time)) = stamp.split_once('T') else {
        return stamp.to_string();
    };

    // Drop the year from the date and the seconds from the time.
    match (date.get(5..), time.get(..5)) {
        (Some(day), Some(minute)) => format!("{day} {minute}"),
        _ => stamp.to_string(),
    }
}

/// Run `sacct` once over the recent window, newest job first. With `only_me`,
/// asks for just the current user's jobs, which is sacct's own default.
pub fn fetch(only_me: bool) -> io::Result<Vec<Run>> {
    let mut command = Command::new("sacct");
    command.args([
        "--parsable2",
        "--noheader",
        // Whole jobs only, not their .batch and .extern steps.
        "--allocations",
        &format!("--starttime={WINDOW}"),
        &format!("--format={FORMAT}"),
    ]);
    if !only_me {
        command.arg("--allusers");
    }

    let output = command.output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "sacct exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut runs: Vec<Run> = stdout.lines().filter_map(Run::parse).collect();

    // sacct reports oldest first; the interesting end of a recent list is the
    // other one.
    runs.reverse();
    Ok(runs)
}

/// Start polling `sacct` in the background. The request is the scope, matching
/// squeue so the `m` toggle means the same thing in both views.
pub fn poll(only_me: bool) -> Poller<bool, Vec<Run>> {
    Poller::spawn(INTERVAL, only_me, fetch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_finished_job() {
        let run =
            Run::parse("310011|dev|train-a|alice|FAILED|00:00:14|2026-08-28T19:56:24|1:0")
                .expect("parsed");

        assert_eq!(run.id, "310011");
        assert_eq!(run.name, "train-a");
        assert_eq!(run.state, "FAILED");
        assert_eq!(run.end, "08-28 19:56");
        assert_eq!(run.exit, "1:0");
    }

    #[test]
    fn drops_the_uid_from_a_cancelled_state() {
        let run = Run::parse(
            "311750|8gpus|train-b|alice|CANCELLED by 12345|00:06:53|2026-08-29T02:58:11|0:0",
        )
        .expect("parsed");

        assert_eq!(run.state, "CANCELLED");
    }

    #[test]
    fn keeps_a_timestamp_it_cannot_shorten() {
        assert_eq!(short_time("Unknown"), "Unknown");
    }

    #[test]
    fn rejects_a_short_line() {
        assert!(Run::parse("310011|dev|train-a").is_none());
    }
}
