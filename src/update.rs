//! Keeping the installed binary current.
//!
//! One check when the app starts, on its own thread: the draw loop must never
//! wait on the network, and a login node with no route out has to open the
//! same screen it always does. The work itself is left to the install script,
//! so there is one update path rather than two that can disagree.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// The script that knows how to fetch, verify and replace the binary, built in
/// rather than downloaded: the only code this runs is code that shipped with
/// it, and a binary that is running is proof its installer arrived intact.
const INSTALLER: &str = include_str!("../install.sh");

/// What the check came to.
///
/// None of it interrupts the session: a new binary is renamed into place
/// beside the running one, which keeps the file it started from until it is
/// next started.
pub enum Outcome {
    /// The running version is the one the installer would install.
    Current,
    /// A newer release is now on disk, and takes over at the next start.
    Installed(String),
    /// Why the check got nowhere. Not an error worth stopping for — an old
    /// binary still shows the same jobs.
    Failed(String),
}

/// The startup check, and whatever it has reported.
pub struct Check {
    /// Sends exactly once, then disconnects.
    result: Receiver<Outcome>,
    outcome: Option<Outcome>,
}

impl Check {
    /// Start checking in the background.
    pub fn start() -> Self {
        let (sender, result) = mpsc::channel();

        thread::spawn(move || {
            // A send that fails only means the app quit first.
            let _ = sender.send(match update() {
                Ok(Some(version)) => Outcome::Installed(version),
                Ok(None) => Outcome::Current,
                Err(err) => Outcome::Failed(err.to_string()),
            });
        });

        Self {
            result,
            outcome: None,
        }
    }

    /// A check that never runs and never reports, for `--no-update`.
    pub fn idle() -> Self {
        let (_, result) = mpsc::channel();

        Self {
            result,
            outcome: None,
        }
    }

    /// Take the result if it has arrived, reporting whether it did — the
    /// caller redraws on a change rather than on a timer.
    pub fn apply_update(&mut self) -> bool {
        match self.result.try_recv() {
            Ok(outcome) => {
                self.outcome = Some(outcome);
                true
            }
            Err(_) => false,
        }
    }

    /// What to say about the check, once there is anything to say.
    pub fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }
}

/// Update now, with the installer's own progress on the terminal, and report
/// whether it worked. This is `nchctop update`: the same script the startup
/// check runs, in the foreground, where watching it is the point.
///
/// Unlike the startup check this updates a development build too — typing the
/// command is asking for it.
pub fn run() -> io::Result<bool> {
    let status = run_installer(&install_dir()?, Stdio::inherit(), Stdio::inherit())?;

    // The script has already said what went wrong, in more detail than a
    // summary line here could add.
    Ok(status.status.success())
}

/// Run the installer over the running binary, and report the version now in
/// its place when that is not the version running.
fn update() -> io::Result<Option<String>> {
    // A development build lives in target/, where dropping a released binary
    // would leave the next `cargo run` starting something nobody built.
    if cfg!(debug_assertions) {
        return Ok(None);
    }

    let dir = install_dir()?;
    let finished = run_installer(&dir, Stdio::null(), Stdio::piped())?;
    if !finished.status.success() {
        return Err(io::Error::other(reason(&finished.stderr)));
    }

    // The file the installer wrote, which is not necessarily the one running:
    // a copy can have been renamed.
    let installed = version_of(&dir.join("nchctop"))?;
    Ok((installed != env!("CARGO_PKG_VERSION")).then_some(installed))
}

/// Where the installer should write, which is wherever the binary now running
/// came from — updating some other copy would leave this one in place.
fn install_dir() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;

    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("No directory to install into"))
}

/// Run the installer against `dir`, with its two output streams wherever the
/// caller wants them.
fn run_installer(dir: &Path, stdout: Stdio, stderr: Stdio) -> io::Result<Output> {
    // Fed over stdin rather than written out and run, so there is no temporary
    // file to clean up or to race. The directory goes through the environment,
    // which spares us quoting a path into a command line.
    let mut sh = Command::new("sh")
        .env("NCHCTOP_INSTALL_DIR", dir)
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?;

    // The handle is dropped at the end of the statement, which is the EOF that
    // lets sh finish reading.
    sh.stdin
        .take()
        .ok_or_else(|| io::Error::other("No stdin"))?
        .write_all(INSTALLER.as_bytes())?;

    sh.wait_with_output()
}

/// The last thing the installer said, which is the part that says why it
/// stopped. Its own name is dropped from the front: the line is shown in a
/// footer, where the columns are better spent on the reason.
fn reason(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().trim_start_matches("install.sh: ").to_string())
        .unwrap_or_else(|| "The installer failed".to_string())
}

fn version_of(binary: &Path) -> io::Result<String> {
    let printed = Command::new(binary).arg("--version").output()?;

    parse_version(&String::from_utf8_lossy(&printed.stdout))
        .ok_or_else(|| io::Error::other("Could not read the installed version"))
}

/// The version out of `--version` output, which clap prints as `nchctop <v>`.
fn parse_version(printed: &str) -> Option<String> {
    Some(
        printed
            .lines()
            .next()?
            .split_whitespace()
            .nth(1)?
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_version_clap_prints() {
        assert_eq!(parse_version("nchctop 0.2.0\n").as_deref(), Some("0.2.0"));
    }

    /// Anything else is a binary that did not answer the question, which is
    /// not a version to compare against.
    #[test]
    fn reads_no_version_from_anything_else() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("nchctop\n"), None);
    }

    #[test]
    fn takes_the_installers_last_word_as_the_reason() {
        let stderr = b"Downloading\ninstall.sh: Checksum mismatch\n\n";

        assert_eq!(reason(stderr), "Checksum mismatch");
    }

    #[test]
    fn falls_back_when_the_installer_said_nothing() {
        assert_eq!(reason(b""), "The installer failed");
    }

    /// The installer is shell, built in as text, so a syntax error in it
    /// would otherwise survive every check this build makes and only show up
    /// as an update that quietly never works.
    #[test]
    fn the_embedded_installer_parses() {
        let mut sh = Command::new("sh")
            .arg("-n")
            .stdin(Stdio::piped())
            .spawn()
            .expect("sh");

        sh.stdin
            .take()
            .expect("stdin")
            .write_all(INSTALLER.as_bytes())
            .expect("write");

        assert!(sh.wait().expect("wait").success());
    }

    /// `--no-update` has to leave a check that stays quiet, not one that
    /// reports a failure it never attempted.
    #[test]
    fn an_idle_check_reports_nothing() {
        let mut check = Check::idle();

        assert!(!check.apply_update());
        assert!(check.outcome().is_none());
    }
}
