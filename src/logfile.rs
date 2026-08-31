//! Where a job writes its output.
//!
//! Slurm keeps the `--output` argument as the user wrote it: a filename
//! pattern like `slurm-%j.out`, read relative to the job's working directory.
//! Neither `squeue` nor `sacct` expands one in the text output we parse — only
//! `sacct --json` does — so the substitution happens here instead, against the
//! two fields we ask both commands for anyway.

/// The job facts a pattern can name.
pub struct Job<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub user: &'a str,
}

/// Expand one output pattern against the job's working directory.
///
/// `None` when the job writes no file at all: `srun` streams to the terminal
/// it was launched from and Slurm records nothing, while `sbatch` records even
/// its default as the pattern `slurm-%j.out`. So an empty field means there is
/// no log to point at, not that we failed to find one.
pub fn path(pattern: &str, workdir: &str, job: &Job<'_>) -> Option<String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return None;
    }

    let expanded = expand(pattern, job);
    let workdir = workdir.trim().trim_end_matches('/');

    let full = match () {
        // `--output=/scratch/x.out` is already where it says.
        _ if expanded.starts_with('/') => expanded,
        _ if workdir.is_empty() => expanded,
        _ => format!("{workdir}/{expanded}"),
    };

    Some(shorten(full))
}

/// Substitute the `%` patterns Slurm documents for output filenames. One it
/// does not know, or one we cannot answer, stays as it was written rather than
/// silently becoming part of a path that does not exist.
fn expand(pattern: &str, job: &Job<'_>) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern.chars();

    while let Some(c) = rest.next() {
        if c != '%' {
            out.push(c);
            continue;
        }

        // Slurm allows a width between the `%` and the letter, which
        // zero-pads the number that follows: `%4j` is `0123`.
        let mut width = String::new();
        let key = loop {
            match rest.next() {
                Some(digit) if digit.is_ascii_digit() => width.push(digit),
                other => break other,
            }
        };

        let Some(key) = key else {
            // A trailing `%`, with nothing after it to be a pattern.
            out.push('%');
            out.push_str(&width);
            break;
        };

        match substitute(key, job) {
            Some(value) => out.push_str(&pad(&value, &width)),
            None => {
                out.push('%');
                out.push_str(&width);
                out.push(key);
            }
        }
    }

    out
}

/// What one pattern letter stands for, or `None` for one we cannot answer —
/// `%N` names the node the job landed on, which is not ours to know here.
fn substitute(key: char, job: &Job<'_>) -> Option<String> {
    // An array task is reported as `masterid_taskid`, which is exactly the
    // pair that `%A` and `%a` want; a plain job is its own master.
    let (array, task) = match job.id.split_once('_') {
        Some((array, task)) => (array, task),
        None => (job.id, "0"),
    };

    Some(match key {
        '%' => "%".to_string(),
        'A' => array.to_string(),
        'a' => task.to_string(),
        'j' | 'J' => job.id.to_string(),
        'x' => job.name.to_string(),
        'u' => job.user.to_string(),
        // The batch script is the first task on the first node of step zero.
        'n' | 's' | 't' => "0".to_string(),
        _ => return None,
    })
}

/// Zero-pad to the width a pattern asked for. No width, or one too large to
/// be a width, leaves the value alone.
fn pad(value: &str, width: &str) -> String {
    match width.parse::<usize>() {
        Ok(width) => format!("{value:0>width$}"),
        Err(_) => value.to_string(),
    }
}

/// Write a path under the home directory the way you would say it.
fn shorten(path: String) -> String {
    match std::env::var("HOME") {
        Ok(home) => under(&path, &home).unwrap_or(path),
        Err(_) => path,
    }
}

/// `/home/alice/logs/x.out` under `/home/alice` is `~/logs/x.out`. `None` when
/// the path is somewhere else — including `/home/alicia`, which shares a
/// prefix with the home directory without being inside it.
fn under(path: &str, home: &str) -> Option<String> {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return None;
    }

    match path.strip_prefix(home)? {
        "" => Some("~".to_string()),
        rest if rest.starts_with('/') => Some(format!("~{rest}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job<'a>(id: &'a str, name: &'a str) -> Job<'a> {
        Job {
            id,
            name,
            user: "alice",
        }
    }

    /// What `sbatch` records when it was given no `--output` at all.
    #[test]
    fn expands_the_default_pattern_against_the_working_directory() {
        assert_eq!(
            path("slurm-%j.out", "/work/alice/amtpp", &job("319141", "train")),
            Some("/work/alice/amtpp/slurm-319141.out".to_string())
        );
    }

    /// A job launched with `srun` writes to the terminal, not to a file.
    #[test]
    fn reports_no_file_when_slurm_recorded_none() {
        assert_eq!(path("", "/work/alice", &job("309857", "train")), None);
        assert_eq!(path("   ", "/work/alice", &job("309857", "train")), None);
    }

    /// The default for an array job, which numbers by master and task.
    #[test]
    fn expands_an_array_task() {
        assert_eq!(
            path("slurm-%A_%a.out", "/work/alice", &job("319141_7", "sweep")),
            Some("/work/alice/slurm-319141_7.out".to_string())
        );
    }

    /// A job that is not an array is its own master, and has no task number.
    #[test]
    fn treats_a_plain_job_as_its_own_array_master() {
        assert_eq!(
            path("%A-%a.log", "/work/alice", &job("319141", "train")),
            Some("/work/alice/319141-0.log".to_string())
        );
    }

    #[test]
    fn expands_the_name_and_the_user() {
        assert_eq!(
            path("logs/%x-%u-%j.out", "/work/alice", &job("500", "train-a")),
            Some("/work/alice/logs/train-a-alice-500.out".to_string())
        );
    }

    /// An absolute `--output` is not relative to anything.
    #[test]
    fn leaves_an_absolute_pattern_where_it_is() {
        assert_eq!(
            path("/scratch/out/%j.log", "/work/alice", &job("500", "train")),
            Some("/scratch/out/500.log".to_string())
        );
    }

    #[test]
    fn zero_pads_to_the_width_a_pattern_asked_for() {
        assert_eq!(
            path("%8j.out", "/work/alice", &job("319141", "train")),
            Some("/work/alice/00319141.out".to_string())
        );
    }

    /// `%%` is how a pattern spells a literal per cent sign.
    #[test]
    fn keeps_an_escaped_per_cent() {
        assert_eq!(
            path("%%-%j.out", "/work/alice", &job("500", "train")),
            Some("/work/alice/%-500.out".to_string())
        );
    }

    /// `%N` is the node the job landed on. Leaving it visible says the path is
    /// a pattern we could not finish, rather than pointing at a file that was
    /// never written.
    #[test]
    fn leaves_a_pattern_it_cannot_answer_visible() {
        assert_eq!(
            path("%N-%j.out", "/work/alice", &job("500", "train")),
            Some("/work/alice/%N-500.out".to_string())
        );
        assert_eq!(
            path("%q.out", "/work/alice", &job("500", "train")),
            Some("/work/alice/%q.out".to_string())
        );
    }

    /// A trailing `%` is not the start of anything.
    #[test]
    fn keeps_a_dangling_per_cent() {
        assert_eq!(
            path("out-%", "/work/alice", &job("500", "train")),
            Some("/work/alice/out-%".to_string())
        );
    }

    /// A job with no working directory recorded still names its file.
    #[test]
    fn falls_back_to_the_bare_pattern_without_a_working_directory() {
        assert_eq!(
            path("slurm-%j.out", "", &job("500", "train")),
            Some("slurm-500.out".to_string())
        );
    }

    #[test]
    fn writes_a_path_under_home_with_a_tilde() {
        assert_eq!(
            under("/home/alice/logs/x.out", "/home/alice"),
            Some("~/logs/x.out".to_string())
        );
        assert_eq!(under("/home/alice", "/home/alice/"), Some("~".to_string()));
    }

    /// A neighbour's directory shares a prefix without being inside home.
    #[test]
    fn leaves_a_path_that_only_looks_like_home() {
        assert_eq!(under("/home/alicia/x.out", "/home/alice"), None);
        assert_eq!(under("/work/alice/x.out", "/home/alice"), None);
        assert_eq!(under("/home/alice/x.out", ""), None);
    }
}
