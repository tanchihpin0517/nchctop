# nchctop

A top-like terminal view of Slurm jobs, built for NCHC clusters.

## Install

    curl -LsSf https://raw.githubusercontent.com/tanchihpin0517/nchctop/main/install.sh | sh

That drops a statically linked binary in `~/.local/bin`, so a login node with no
Rust toolchain and an old glibc still runs it. The script checks the published
sha256, and says so if `~/.local/bin` is not on your `PATH`.

Somewhere else, or an older release — note the `-s --`, which is how a piped
script is given arguments:

    curl -LsSf https://raw.githubusercontent.com/tanchihpin0517/nchctop/main/install.sh | sh -s -- --dir ~/bin --version 0.1.0

`NCHCTOP_INSTALL_DIR` and `NCHCTOP_VERSION` do the same, and `--help` lists
everything.

Or, from source:

    cargo install --git https://github.com/tanchihpin0517/nchctop

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

## Cost

The right of the header totals what the recent jobs cost, over the last day,
week and month:

    wallet · MST000000 12345.6 SU        cost · 1d 147.7 SU · 7d 267.9 SU · 30d 267.9 SU

The balances are measured out first, so a terminal too narrow for both drops the
longest window rather than crowding them: `30d` goes, then `7d`, then the total
altogether.

A job is charged **30 SU per GPU-hour** — its GPUs held for as long as it ran,
with a job still going counted for the time it has run so far. A job lands in a
window by when it started, matching how `sacct` was asked for the list, so a
long job that straddles a boundary counts whole in the windows containing its
start.

This is an estimate, not the ledger. A job that asked for no GPUs bills against
a rate this does not model and shows as free, and `m` widens the total to every
user's jobs along with the panes. `wallet` remains the authority on what has
actually been charged.

## Credits

Inspired by:

- **turm** — a Slurm TUI that polls `squeue` every two seconds.
- **slurmtop** — a terminal dashboard for Slurm clusters.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
