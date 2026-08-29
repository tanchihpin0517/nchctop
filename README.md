# nchctop

A top-like terminal view of Slurm jobs, built for the NCHC nano4 cluster.

## Refresh

Each pane is fed by its own background thread, so a slow cluster never blocks
the interface. Press `r` to refresh both at once.

| Pane | Command | Interval |
| --- | --- | --- |
| queue | `squeue` | 1s |
| last 30d | `sacct`, jobs started since `now-30days` | 30s |

The interval is the gap between fetches rather than a fixed period: a command
that takes a while to return backs itself off instead of piling up overlapping
runs.

## Credits

Inspired by:

- **turm** — a Slurm TUI that polls `squeue` every two seconds.
- **slurmtop** — a terminal dashboard for Slurm clusters.
