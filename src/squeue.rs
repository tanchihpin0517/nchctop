use std::io;
use std::process::Command;
use std::time::Duration;

use crate::poller::Poller;
use crate::tres;

/// The squeue fields we ask for, in the order the table renders them, in the
/// long `--Format` spelling: the short `%b` and `%m` report GPUs and memory
/// per node, while `tres-alloc` reports the totals that sacct also reports.
/// Each `:|` is a field width of "as wide as needed" plus a separator.
const FORMAT: &str = "JobID:|,Partition:|,Name:|,UserName:|,State:|,tres-alloc:|,\
                      TimeUsed:|,NumNodes:|,NodeList:|,Reason:";

/// One row of `squeue` output.
pub struct Job {
    pub id: String,
    pub partition: String,
    pub name: String,
    pub user: String,
    pub state: String,
    pub cpus: String,
    pub gpus: String,
    pub mem: String,
    pub time: String,
    pub nodes: String,
    pub reason: String,
}

impl Job {
    /// Parse one `FORMAT` line. Returns `None` for a line that does not have
    /// every field, so one malformed row cannot take the whole refresh down.
    fn parse(line: &str) -> Option<Self> {
        // splitn keeps any '|' inside the trailing reason field.
        let fields: Vec<&str> = line.splitn(10, '|').collect();
        let [
            id,
            partition,
            name,
            user,
            state,
            tres,
            time,
            nodes,
            nodelist,
            reason,
        ] = fields[..]
        else {
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
            state: state.trim().to_string(),
            cpus: resource("cpu"),
            gpus: resource("gres/gpu"),
            mem: tres::value(tres, "mem").map_or("-".to_string(), tres::gigabytes),
            time: time.trim().to_string(),
            nodes: nodes.trim().to_string(),
            // What the short format's %R gives in one field: where the job is
            // running, or why it is not.
            reason: match nodelist.trim() {
                "" => format!("({})", reason.trim()),
                nodelist => nodelist.to_string(),
            },
        })
    }
}

/// Run `squeue` once and parse every job it reports. With `only_me`, asks for
/// just the current user's jobs.
pub fn fetch(only_me: bool) -> io::Result<Vec<Job>> {
    let mut command = Command::new("squeue");
    command.args(["--noheader", &format!("--Format={FORMAT}")]);
    if only_me {
        command.arg("--me");
    }

    let output = command.output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "squeue exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().filter_map(Job::parse).collect())
}

/// How often to re-run `squeue`. Cheap enough to sit at the front of the
/// refresh order, since the job list is what actually changes minute to minute.
const INTERVAL: Duration = Duration::from_secs(1);

/// Start polling `squeue` in the background. The request is the `--me` scope,
/// so a toggle takes effect on the next fetch and tags what comes back.
pub fn poll(only_me: bool) -> Poller<bool, Vec<Job>> {
    Poller::spawn(INTERVAL, only_me, fetch)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = "308208|32gpus|train-a|bob|RUNNING|\
        cpu=96,mem=4800G,node=3,billing=96,gres/gpu=24|22:46:39|3|gpn[001-003]|None";

    /// The format string is written across two lines; the continuation must
    /// not smuggle whitespace into what squeue receives.
    #[test]
    fn format_has_no_spaces() {
        assert!(!FORMAT.contains(' '));
        assert!(FORMAT.contains("tres-alloc"));
    }

    #[test]
    fn parses_a_running_job() {
        let job = Job::parse(RUNNING).expect("parsed");

        assert_eq!(job.id, "308208");
        assert_eq!(job.name, "train-a");
        assert_eq!(job.cpus, "96");
        assert_eq!(job.gpus, "24");
        assert_eq!(job.mem, "4800G");
        assert_eq!(job.nodes, "3");
        // A running job shows where it landed, not why.
        assert_eq!(job.reason, "gpn[001-003]");
    }

    /// A pending job has no nodes yet, so the column carries the reason in
    /// brackets the way squeue's own %R does.
    #[test]
    fn shows_the_reason_when_a_job_is_pending() {
        let job = Job::parse(
            "311567|8gpus|sample|bob|PENDING|cpu=8,mem=200G,node=1,gres/gpu=1|0:00|1||Dependency",
        )
        .expect("parsed");

        assert_eq!(job.reason, "(Dependency)");
        assert_eq!(job.gpus, "1");
    }

    #[test]
    fn marks_a_job_that_asked_for_no_gpus() {
        let job = Job::parse(
            "312000|dev|cpu-only|alice|RUNNING|cpu=2,mem=16G,node=1|0:05|1|n01|None",
        )
        .expect("parsed");

        assert_eq!(job.gpus, "-");
        assert_eq!(job.mem, "16G");
    }

    #[test]
    fn rejects_a_short_line() {
        assert!(Job::parse("312000|dev|cpu-only").is_none());
    }
}
