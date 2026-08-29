use std::io;
use std::process::Command;
use std::time::Duration;

use crate::poller::Poller;

/// The squeue fields we ask for, in the order the table renders them.
const FORMAT: &str = "%i|%P|%j|%u|%T|%M|%D|%R";

/// One row of `squeue` output.
pub struct Job {
    pub id: String,
    pub partition: String,
    pub name: String,
    pub user: String,
    pub state: String,
    pub time: String,
    pub nodes: String,
    pub reason: String,
}

impl Job {
    /// Parse one `FORMAT` line. Returns `None` for a line that does not have
    /// every field, so one malformed row cannot take the whole refresh down.
    fn parse(line: &str) -> Option<Self> {
        // splitn keeps any '|' inside the trailing nodelist/reason field.
        let fields: Vec<&str> = line.splitn(8, '|').collect();
        let [id, partition, name, user, state, time, nodes, reason] = fields[..] else {
            return None;
        };

        Some(Self {
            id: id.trim().to_string(),
            partition: partition.trim().to_string(),
            name: name.trim().to_string(),
            user: user.trim().to_string(),
            state: state.trim().to_string(),
            time: time.trim().to_string(),
            nodes: nodes.trim().to_string(),
            reason: reason.trim().to_string(),
        })
    }
}

/// Run `squeue` once and parse every job it reports. With `only_me`, asks for
/// just the current user's jobs.
pub fn fetch(only_me: bool) -> io::Result<Vec<Job>> {
    let mut command = Command::new("squeue");
    command.args(["--noheader", &format!("--format={FORMAT}")]);
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
