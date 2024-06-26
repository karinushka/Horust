//! Reaper module
//! This module is concerned with process reaping. After a process has exited, the reaper will
//! collect its exit status. This module will make sure to reap also zombie processes and not only
//! child services.

use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;

use crate::horust::supervisor::repo::Repo;
use crate::horust::Event;

/// Reaps up to `max_iterations` dead processes
///
/// # Safety
///
/// This function must run in isolation with respect to the fork processes to
/// prevent pid reusage.
pub(crate) fn run(repo: &Repo, max_iterations: u32) -> Vec<Event> {
    (0..max_iterations)
        .filter_map(
            |_| match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(wait_status) => match wait_status {
                    WaitStatus::Exited(pid, exit_code) => {
                        debug!("Pid '{}' has exited with status: {}", pid, exit_code);
                        repo.get_service_by_pid(pid)
                            .map(|s_name| (s_name, exit_code))
                    }
                    WaitStatus::Signaled(pid, signal, core_dumped) => {
                        debug!(
                            "Pid '{:?}' has exited due to signal: {:?}. Core dumped: {}",
                            pid, signal, core_dumped
                        );
                        repo.get_service_by_pid(pid).map(|s_name| (s_name, -137))
                    }
                    _ => None,
                },
                Err(err) => {
                    // waitpid() call can fail with:
                    // EINVAL: Options argument is not valid - should not apply.
                    // EINTR:  Possible & common.
                    // ECHILD: The process specified by pid does not exist or is not a child of the
                    // calling process, or the process group specified by pid does not exist or does
                    // not have any member process that is a child of the calling process.
                    // Since we're using -1 as parameter, we should never fall in this branch.
                    if !err.to_string().contains("ECHILD") {
                        // This is the only "actual" error.
                        error!("Error waitpid(): {}", err);
                    }
                    None
                }
            },
        )
        .map(|(sname, exit_code)| {
            debug!("Service '{:?}' has exited.", sname);
            Event::new_service_exited(sname.into(), exit_code)
        })
        .collect()
}
