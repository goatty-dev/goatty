//! Installs as jobs that outlive the request that started them (#102):
//! what each one is doing ([`api::InstallProgress`]), and a way to cancel
//! it before it's activated.
//!
//! [`Jobs`] is shared by every clone of a `HostService`, so goattyd answers
//! `Installs` and `CancelInstall` from any connection while the install
//! runs on another one's thread. Finished installs stay listed for
//! [`KEEP_FINISHED`], so a client that lost its connection meanwhile still
//! learns how it ended.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use goatty_plugin::api::{Changed, InstallProgress, Step};
use goatty_plugin::fetch::Cancelled;

/// How long a finished install stays in the list.
pub const KEEP_FINISHED: Duration = Duration::from_secs(15 * 60);

/// One install.
#[derive(Debug)]
pub struct Job {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    progress: InstallProgress,
    finished: Option<Instant>,
}

impl Job {
    fn new(id: &str, name: &str, version: &str) -> Job {
        Job {
            state: Mutex::new(State {
                progress: InstallProgress {
                    id: id.into(),
                    name: name.into(),
                    version: version.into(),
                    step: Step::Waiting,
                    cancelling: false,
                },
                finished: None,
            }),
        }
    }

    /// A job nobody else sees (the CLI, tests).
    pub fn untracked() -> Job {
        Job::new("", "", "")
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn progress(&self) -> InstallProgress {
        self.lock().progress.clone()
    }

    /// Stop with [`Cancelled`] if a cancel was asked for.
    pub fn check(&self) -> Result<()> {
        if self.lock().progress.cancelling {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    /// Move on to `step` (a cancellable one). Stops with [`Cancelled`] if a
    /// cancel was asked for.
    pub fn step(&self, step: Step) -> Result<()> {
        let mut s = self.lock();
        if s.progress.cancelling {
            return Err(Cancelled.into());
        }
        s.progress.step = step;
        Ok(())
    }

    /// Bytes downloaded so far of the current package; `false`: cancelled.
    pub fn downloaded(&self, so_far: u64) -> bool {
        let mut s = self.lock();
        if let Step::Downloading { bytes, .. } = &mut s.progress.step {
            *bytes = so_far;
        }
        !s.progress.cancelling
    }

    /// From here on the install can't be cancelled. Fails with [`Cancelled`]
    /// if it was cancelled first: checked and switched under one lock, so a
    /// cancel either wins or is refused.
    pub fn activate(&self) -> Result<()> {
        let mut s = self.lock();
        if s.progress.cancelling {
            return Err(Cancelled.into());
        }
        s.progress.step = Step::Activating;
        Ok(())
    }

    /// Ask the install to stop. Refused once it's activating or finished.
    pub fn cancel(&self) -> Result<()> {
        let mut s = self.lock();
        let p = &mut s.progress;
        if !p.step.cancellable() {
            if p.step.finished() {
                bail!("{} isn't being installed any more", p.name);
            }
            bail!("{} is being activated; it's too late to cancel", p.name);
        }
        p.cancelling = true;
        Ok(())
    }

    fn finish(&self, result: &Result<Changed>) {
        let mut s = self.lock();
        s.progress.step = match result {
            Ok(c) => Step::Done(c.clone()),
            Err(e) if Cancelled::is(e) => Step::Cancelled,
            Err(e) => Step::Failed { error: format!("{e:#}") },
        };
        s.finished = Some(Instant::now());
    }
}

/// The installs on one host.
#[derive(Debug, Clone, Default)]
pub struct Jobs(Arc<Mutex<Vec<Arc<Job>>>>);

impl Jobs {
    fn lock(&self) -> MutexGuard<'_, Vec<Arc<Job>>> {
        let mut list = self.0.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|j| j.lock().finished.is_none_or(|t| t.elapsed() < KEEP_FINISHED));
        list
    }

    /// Run the install of `id` as a job; refused while another one of the
    /// same plugin runs. The job's outcome is kept for [`Jobs::list`].
    pub fn run(&self, id: &str, name: &str, version: &str, f: impl FnOnce(&Job) -> Result<Changed>) -> Result<Changed> {
        let job = {
            let mut list = self.lock();
            if list.iter().any(|j| j.progress().id == id && !j.progress().step.finished()) {
                bail!("{name} is already being installed");
            }
            // The newest outcome per plugin is enough.
            list.retain(|j| j.progress().id != id);
            let job = Arc::new(Job::new(id, name, version));
            list.push(job.clone());
            job
        };
        let result = f(&job);
        job.finish(&result);
        result
    }

    /// Running and recently finished installs, oldest first.
    pub fn list(&self) -> Vec<InstallProgress> {
        self.lock().iter().map(|j| j.progress()).collect()
    }

    /// Ask the running install of `id` to stop.
    pub fn cancel(&self, id: &str) -> Result<()> {
        let list = self.lock();
        let Some(job) = list.iter().find(|j| j.progress().id == id) else { bail!("{id} isn't being installed") };
        job.cancel()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_wins_before_activation_and_is_refused_after() {
        let jobs = Jobs::default();
        let r = jobs.run("a", "A", "1", |job| {
            job.step(Step::Downloading { package: "a".into(), index: 1, count: 1, bytes: 0, total: 9 })?;
            assert!(jobs.list()[0].step.cancellable());
            jobs.cancel("a").unwrap();
            assert!(!job.downloaded(5), "the download sees the cancel");
            job.activate()?;
            unreachable!("cancelled before activation")
        });
        assert!(Cancelled::is(&r.unwrap_err()));
        assert_eq!(jobs.list()[0].step, Step::Cancelled);

        let r = jobs.run("a", "A", "1", |job| {
            job.activate()?;
            assert!(jobs.cancel("a").unwrap_err().to_string().contains("too late"));
            Ok(Changed { lines: vec!["Installed A 1".into()], ..Changed::default() })
        });
        assert!(r.is_ok());
        let list = jobs.list();
        assert_eq!(list.len(), 1, "only the newest outcome per plugin");
        assert!(matches!(&list[0].step, Step::Done(c) if c.lines == ["Installed A 1"]));
        assert!(jobs.cancel("a").unwrap_err().to_string().contains("isn't being installed any more"));
    }

    #[test]
    fn a_plugin_installs_once_at_a_time() {
        let jobs = Jobs::default();
        let r = jobs.run("a", "A", "1", |_| {
            let again = jobs.run("a", "A", "1", |_| Ok(Changed::default()));
            assert!(again.unwrap_err().to_string().contains("already being installed"));
            Ok(Changed::default())
        });
        assert!(r.is_ok());
    }
}
