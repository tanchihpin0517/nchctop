use std::io;
use std::process::Command;
use std::time::Duration;

use crate::poller::Poller;
use crate::tres;

/// The sacct fields we ask for, in the order the table renders them.
/// `AllocTRES` expands into the CPU, GPU and memory columns.
const FORMAT: &str = "JobID,Partition,JobName,User,State,AllocTRES,Elapsed,End,ExitCode";

/// How far back the recent view looks. Slurm's time specification understands
/// weeks but not months, so a month is spelled out in days.
const WINDOW: &str = "now-30days";

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
    pub cpus: String,
    pub gpus: String,
    pub mem: String,
    pub elapsed: String,
    pub end: String,
    pub exit: String,
}

impl Run {
    /// Parse one `--parsable2` line. Returns `None` for a line that does not
    /// have every field, so one malformed row cannot take the whole refresh
    /// down.
    fn parse(line: &str) -> Option<Self> {
        let fields: Vec<&str> = line.splitn(9, '|').collect();
        let [id, partition, name, user, state, tres, elapsed, end, exit] = fields[..] else {
            return None;
        };

        // A job that asked for no GPUs has no gres/gpu entry at all, rather
        // than a zero.
        let resource = |key| tres::value(tres, key).unwrap_or("-").to_string();

        Some(Self {
            id: id.trim().to_string(),
            partition: partition.trim().to_string(),
            name: name.trim().to_string(),
            user: user.trim().to_string(),
            state: state_name(state.trim()),
            cpus: resource("cpu"),
            gpus: resource("gres/gpu"),
            mem: tres::value(tres, "mem").map_or("-".to_string(), tres::gigabytes),
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
        let run = Run::parse(
            "310011|dev|train-a|alice|FAILED|billing=12,cpu=12,gres/gpu=1,mem=200G,node=1|00:00:14|2026-08-28T19:56:24|1:0",
        )
        .expect("parsed");

        assert_eq!(run.id, "310011");
        assert_eq!(run.name, "train-a");
        assert_eq!(run.state, "FAILED");
        assert_eq!(run.cpus, "12");
        assert_eq!(run.gpus, "1");
        assert_eq!(run.mem, "200G");
        assert_eq!(run.end, "08-28 19:56");
        assert_eq!(run.exit, "1:0");
    }

    #[test]
    fn drops_the_uid_from_a_cancelled_state() {
        let run = Run::parse(
            "311750|8gpus|train-b|alice|CANCELLED by 12345|billing=2,cpu=2,mem=16G,node=1|00:06:53|2026-08-29T02:58:11|0:0",
        )
        .expect("parsed");

        assert_eq!(run.state, "CANCELLED");
    }

    /// A CPU-only job has no gres/gpu entry, which has to read as "none"
    /// rather than as a missing row.
    #[test]
    fn marks_a_job_that_asked_for_no_gpus() {
        let run = Run::parse(
            "312000|dev|cpu-only|alice|COMPLETED|billing=2,cpu=2,mem=16G,node=1|00:01:00|2026-08-29T02:58:11|0:0",
        )
        .expect("parsed");

        assert_eq!(run.cpus, "2");
        assert_eq!(run.gpus, "-");
        assert_eq!(run.mem, "16G");
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
