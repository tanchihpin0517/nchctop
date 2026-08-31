use std::io;
use std::process::Command;
use std::time::Duration;

use chrono::NaiveDateTime;

use crate::logfile;
use crate::poller::Poller;
use crate::tres;

/// The sacct fields we ask for. The leading ones are in the order the table
/// renders them, with `AllocTRES` expanding into the CPU, GPU and memory
/// columns. The trailing three are not columns of their own: `Start` feeds the
/// cost line, and `WorkDir` and `StdOut` feed the log column between them.
const FORMAT: &str =
    "JobID,Partition,JobName,User,State,AllocTRES,Elapsed,End,ExitCode,Start,WorkDir,StdOut";

/// How far back the recent view looks. Slurm's time specification understands
/// weeks but not months, so a month is spelled out in days.
const WINDOW: &str = "now-30days";

/// How often to re-run `sacct`. Slower than squeue, because the accounting
/// database is the heavier thing to ask and a finished job never changes
/// again — but not so slow that a job leaving the queue seems to fall into a
/// gap before it reappears here. The pane is one user's own jobs over a month,
/// which is hundreds of rows rather than the cluster's hundreds of thousands,
/// and measures in tens of milliseconds.
///
/// [`Poller`] waits this long *between* fetches rather than running on a
/// period, so a cluster where the query really is slow backs itself off
/// instead of queueing up overlapping runs.
const INTERVAL: Duration = Duration::from_secs(5);

/// One job from the accounting database, finished or still going.
///
/// Most fields are kept as the strings the table prints. The three the cost
/// line adds up are kept as numbers as well, because a total cannot be taken
/// of `"1"` and `"04:00:14"`.
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
    pub gpu_count: u32,
    pub runtime: Duration,
    /// `None` for a job that never started, which has nothing to bill.
    pub start: Option<NaiveDateTime>,
    /// Where the job wrote its output, or `-` for one that wrote no file.
    pub log: String,
}

impl Run {
    /// Parse one `--parsable2` line. Returns `None` for a line that does not
    /// have every field, so one malformed row cannot take the whole refresh
    /// down.
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let fields: Vec<&str> = line.splitn(12, '|').collect();
        let [
            id,
            partition,
            name,
            user,
            state,
            tres,
            elapsed,
            end,
            exit,
            start,
            workdir,
            stdout,
        ] = fields[..]
        else {
            return None;
        };

        // A job that asked for no GPUs has no gres/gpu entry at all, rather
        // than a zero.
        let resource = |key| tres::value(tres, key).unwrap_or("-").to_string();

        let job = logfile::Job {
            id: id.trim(),
            name: name.trim(),
            user: user.trim(),
        };

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
            gpu_count: tres::value(tres, "gres/gpu")
                .and_then(|gpus| gpus.parse().ok())
                .unwrap_or(0),
            runtime: runtime(elapsed.trim()),
            start: timestamp(start.trim()),
            log: logfile::path(stdout, workdir, &job).unwrap_or_else(|| "-".to_string()),
        })
    }
}

/// Parse sacct's `Elapsed`, which is `HH:MM:SS` until a job has run for a day
/// and `D-HH:MM:SS` after that. Read from the right, so the shorter `MM:SS`
/// that Slurm documents still lands in the units it means.
fn runtime(elapsed: &str) -> Duration {
    let (days, clock) = match elapsed.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().unwrap_or(0), clock),
        None => (0, elapsed),
    };

    let mut seconds = days * 24 * 60 * 60;
    for (unit, part) in [1, 60, 60 * 60].into_iter().zip(clock.rsplit(':')) {
        seconds += unit * part.parse::<u64>().unwrap_or(0);
    }

    Duration::from_secs(seconds)
}

/// Parse a Slurm timestamp. A job that has not started reports `None` or
/// `Unknown` rather than a time, and neither is one.
fn timestamp(stamp: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H:%M:%S").ok()
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

/// Run `sacct` once over the recent window, newest job first.
///
/// Always the current user's own jobs, which is sacct's own default: the pane
/// is there to answer what you have been running and what it cost, and the
/// costs beside it are read against your own balance.
pub fn fetch() -> io::Result<Vec<Run>> {
    let mut command = Command::new("sacct");
    command.args([
        "--parsable2",
        "--noheader",
        // Whole jobs only, not their .batch and .extern steps.
        "--allocations",
        &format!("--starttime={WINDOW}"),
        &format!("--format={FORMAT}"),
    ]);

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

/// Start polling `sacct` in the background. There is nothing to vary from one
/// fetch to the next, so the request carries nothing.
pub fn poll() -> Poller<(), Vec<Run>> {
    Poller::spawn(INTERVAL, (), |()| fetch())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_finished_job() {
        let run = Run::parse(
            "310011|dev|train-a|alice|FAILED|billing=12,cpu=12,gres/gpu=1,mem=200G,node=1|00:00:14|2026-08-28T19:56:24|1:0|2026-08-28T19:56:10|/work/alice/amtpp|slurm-%j.out",
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
        assert_eq!(run.gpu_count, 1);
        assert_eq!(run.runtime, Duration::from_secs(14));
        assert_eq!(run.start, timestamp("2026-08-28T19:56:10"));
        // sacct reports the pattern; the column reports the file.
        assert_eq!(run.log, "/work/alice/amtpp/slurm-310011.out");
    }

    #[test]
    fn drops_the_uid_from_a_cancelled_state() {
        let run = Run::parse(
            "311750|8gpus|train-b|alice|CANCELLED by 12345|billing=2,cpu=2,mem=16G,node=1|00:06:53|2026-08-29T02:58:11|0:0|2026-08-29T02:51:18|/work/alice|slurm-%j.out",
        )
        .expect("parsed");

        assert_eq!(run.state, "CANCELLED");
    }

    /// A CPU-only job has no gres/gpu entry, which has to read as "none"
    /// rather than as a missing row.
    #[test]
    fn marks_a_job_that_asked_for_no_gpus() {
        let run = Run::parse(
            "312000|dev|cpu-only|alice|COMPLETED|billing=2,cpu=2,mem=16G,node=1|00:01:00|2026-08-29T02:58:11|0:0|2026-08-29T02:57:11|/work/alice|",
        )
        .expect("parsed");

        assert_eq!(run.cpus, "2");
        assert_eq!(run.gpus, "-");
        assert_eq!(run.mem, "16G");
        assert_eq!(run.gpu_count, 0);
        // The same row was launched with srun, which writes no file.
        assert_eq!(run.log, "-");
    }

    /// A job that never ran has no start time to place it in a cost window.
    #[test]
    fn marks_a_job_that_never_started() {
        let run = Run::parse(
            "312500|dev|never-ran|alice|CANCELLED|billing=2,cpu=2,mem=16G,node=1|00:00:00|Unknown|0:0|None|/work/alice|slurm-%j.out",
        )
        .expect("parsed");

        assert_eq!(run.start, None);
        assert_eq!(run.runtime, Duration::ZERO);
    }

    #[test]
    fn reads_an_elapsed_time() {
        assert_eq!(runtime("00:00:14"), Duration::from_secs(14));
        assert_eq!(runtime("04:00:14"), Duration::from_secs(4 * 3600 + 14));
        // Past a day sacct prefixes the days, and past a week it keeps
        // counting in days rather than moving on to weeks.
        assert_eq!(
            runtime("7-06:45:10"),
            Duration::from_secs(7 * 86_400 + 6 * 3600 + 45 * 60 + 10)
        );
        // Anything unrecognised reads as no time at all rather than taking the
        // refresh down.
        assert_eq!(runtime("INVALID"), Duration::ZERO);
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
