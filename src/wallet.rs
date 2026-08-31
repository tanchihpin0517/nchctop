use std::io;
use std::process::Command;
use std::time::Duration;

use crate::poller::Poller;

/// How often to re-run `wallet`.
///
/// Long, because this is the one command here that is not Slurm. `wallet` is a
/// wrapper that `sudo`s to another account to run the real thing, which asks a
/// billing service: about 80ms of that is the privilege switch and the rest is
/// the query, for some 300 to 600ms all told against the thirty milliseconds
/// `squeue` and `sacct` take. It says itself that it "may take up to 5 seconds
/// or more". Polling it at the rate of the job panes would spend a tenth of
/// the session inside it.
///
/// Asking for every project at once, as [`fetch`] does, is the cheap way round:
/// the per-project part of the answer costs about 80ms, so three projects in
/// one call beat three calls naming one project each by more than twice over.
///
/// It does not need to be shorter. A balance moves when a job finishes, and a
/// minute is the longest you would wait to see that — against a cost line that
/// is only ever an estimate anyway, and against spending a tenth of the
/// session asking.
const INTERVAL: Duration = Duration::from_secs(60);

/// One project's service-unit balance.
pub struct Project {
    pub id: String,
    pub balance: f64,
}

impl Project {
    /// Parse one `PROJECT_ID: …, PROJECT_NAME: …, SU_BALANCE: …` line. The
    /// INFO lines `wallet` also prints return `None`, as does anything else
    /// unexpected, so one odd line cannot take the whole refresh down.
    fn parse(line: &str) -> Option<Self> {
        // Split from the right: a project name is free text and may itself
        // contain a comma.
        let (head, balance) = line.rsplit_once(", SU_BALANCE: ")?;
        let (id, _name) = head
            .strip_prefix("PROJECT_ID: ")?
            .split_once(", PROJECT_NAME: ")?;

        Some(Self {
            id: id.trim().to_string(),
            balance: balance.trim().parse().ok()?,
        })
    }
}

/// Run `wallet` once and read every project it reports.
pub fn fetch() -> io::Result<Vec<Project>> {
    let output = Command::new("wallet").output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "wallet exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().filter_map(Project::parse).collect())
}

/// Start polling `wallet` in the background. There is nothing to vary per
/// fetch, so the request carries no information.
pub fn poll() -> Poller<(), Vec<Project>> {
    Poller::spawn(INTERVAL, (), |()| fetch())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_project_line() {
        let project = Project::parse(
            "PROJECT_ID: MST000000, PROJECT_NAME: some project, SU_BALANCE: 12345.6789",
        )
        .expect("parsed");

        assert_eq!(project.id, "MST000000");
        assert!((project.balance - 12345.6789).abs() < f64::EPSILON);
    }

    #[test]
    fn keeps_a_name_containing_a_comma() {
        let project = Project::parse(
            "PROJECT_ID: ACD000000, PROJECT_NAME: some centre, second clause, SU_BALANCE: -42.5",
        )
        .expect("parsed");

        assert_eq!(project.id, "ACD000000");
        assert!(project.balance < 0.0);
    }

    #[test]
    fn ignores_an_info_line() {
        assert!(
            Project::parse("INFO: If you belong to many projects, it may take longer.").is_none()
        );
    }
}
