//! What the recent jobs have cost, in the service units the wallet is spent in.
//!
//! NCHC bills a GPU job by the GPU-hour, so a run costs its GPUs held for as
//! long as it ran. This is an estimate rather than the ledger: a job that asked
//! for no GPUs bills against a rate this does not model and shows as free, and
//! `wallet` remains the authority on what has actually been charged.

use chrono::{NaiveDateTime, TimeDelta};

use crate::sacct::Run;

/// Service units per GPU-hour.
const SU_PER_GPU_HOUR: f64 = 30.0;

/// The windows to total, in days. Shortest first, which is the order they are
/// read in and the order they are shown in.
pub const WINDOWS: [i64; 3] = [1, 7, 30];

/// What one run has cost: its GPUs, held for as long as it has run. A job still
/// going is counted for the time it has run so far, which is what its `Elapsed`
/// reports.
fn su(run: &Run) -> f64 {
    let hours = run.runtime.as_secs_f64() / (60.0 * 60.0);
    f64::from(run.gpu_count) * hours * SU_PER_GPU_HOUR
}

/// Total what `runs` cost over each of [`WINDOWS`], counting back from `now`.
///
/// A run lands in a window by when it started, matching how the pane itself was
/// asked for: `sacct` selects on start time, so bucketing on anything else
/// would total a different set of jobs than the one on screen. A long run that
/// straddles a boundary therefore counts whole in the windows containing its
/// start and not at all in the shorter ones.
pub fn totals(runs: &[Run], now: NaiveDateTime) -> [f64; WINDOWS.len()] {
    let mut totals = [0.0; WINDOWS.len()];

    for run in runs {
        // A job that never started has nothing to bill and no time to bill it
        // against.
        let Some(start) = run.start else { continue };
        let age = now - start;
        let cost = su(run);

        for (total, days) in totals.iter_mut().zip(WINDOWS) {
            if age < TimeDelta::days(days) {
                *total += cost;
            }
        }
    }

    totals
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The moment the windows below are measured back from.
    const NOW: &str = "2026-08-29T12:00:00";

    fn now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str(NOW, "%Y-%m-%dT%H:%M:%S").expect("parsed")
    }

    /// One `sacct` line for a job that used `gpus` GPUs for `elapsed`, starting
    /// at `start`. Real lines rather than a hand-built [`Run`], so the cost is
    /// totalled from the same text the cluster sends.
    fn line(gpus: u32, elapsed: &str, start: &str) -> String {
        let gres = if gpus > 0 {
            format!("gres/gpu={gpus},")
        } else {
            String::new()
        };

        format!(
            "310011|dev|job|alice|COMPLETED|billing=12,cpu=12,{gres}mem=200G,node=1|{elapsed}|Unknown|0:0|{start}"
        )
    }

    fn runs(lines: &[String]) -> Vec<Run> {
        lines
            .iter()
            .map(|line| Run::parse(line).expect("parsed"))
            .collect()
    }

    /// The rate itself: one GPU for one hour.
    #[test]
    fn charges_thirty_su_per_gpu_hour() {
        let runs = runs(&[line(1, "01:00:00", "2026-08-29T11:00:00")]);

        assert_eq!(totals(&runs, now()), [30.0, 30.0, 30.0]);
    }

    /// Eight GPUs for a quarter of an hour costs the same as two for an hour.
    #[test]
    fn charges_every_gpu_a_job_held() {
        let runs = runs(&[line(8, "00:15:00", "2026-08-29T11:45:00")]);

        assert_eq!(totals(&runs, now()), [60.0, 60.0, 60.0]);
    }

    /// Each window is a superset of the shorter ones, so an older job appears
    /// in the longer totals only.
    #[test]
    fn sorts_runs_into_the_windows_that_contain_them() {
        let runs = runs(&[
            line(1, "01:00:00", "2026-08-29T09:00:00"),
            line(1, "01:00:00", "2026-08-26T09:00:00"),
            line(1, "01:00:00", "2026-08-10T09:00:00"),
        ]);

        assert_eq!(totals(&runs, now()), [30.0, 60.0, 90.0]);
    }

    /// A CPU-only job bills against a rate this does not model, so it adds
    /// nothing rather than a wrong number.
    #[test]
    fn charges_a_job_with_no_gpus_nothing() {
        let runs = runs(&[line(0, "10:00:00", "2026-08-29T02:00:00")]);

        assert_eq!(totals(&runs, now()), [0.0, 0.0, 0.0]);
    }

    /// A job cancelled before it ran has no start time to place it by.
    #[test]
    fn skips_a_run_that_never_started() {
        let runs = runs(&[line(1, "00:00:00", "None")]);

        assert_eq!(totals(&runs, now()), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn totals_nothing_over_an_empty_list() {
        assert_eq!(totals(&[], now()), [0.0, 0.0, 0.0]);
    }
}
