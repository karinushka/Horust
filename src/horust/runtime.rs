use crate::horust::error::Result;
use crate::horust::formats::{Dispatcher, Event, Service, ServiceStatus, UpdatesQueue};
use crate::horust::service_handler::{ServiceHandler, ServiceRepository};
use crate::horust::{healthcheck, reaper, signal_handling};
use libc::{prctl, PR_SET_CHILD_SUBREAPER};
use nix::sys::signal::kill;
use nix::sys::signal::SIGTERM;
use nix::unistd::{fork, getppid, ForkResult};
use nix::unistd::{getpid, Pid};
use shlex;
use std::ffi::{CStr, CString, OsStr};
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, thread};

#[derive(Debug)]
pub struct Horust {
    service_repository: ServiceRepository,
    services_dir: Option<PathBuf>,
    dispatcher: Dispatcher,
}

impl Horust {
    fn new(services: Vec<Service>, services_dir: Option<PathBuf>) -> Self {
        let mut dispatcher = Dispatcher::new();
        Horust {
            service_repository: ServiceRepository::new(services, dispatcher.add_component()),
            services_dir,
            dispatcher,
        }
    }
    pub fn from_command(command: String) -> Self {
        Self::new(vec![Service::from_command(command)], None)
    }

    /// Create a new horust instance from a path of services.
    pub fn from_services_dir<P>(path: &P) -> Result<Self>
    where
        P: AsRef<Path> + ?Sized + AsRef<OsStr> + Debug,
    {
        let services = fetch_services(path)?;
        debug!("Services found: {:?}", services);
        Ok(Horust::new(services, None))
    }

    fn check_is_shutting_down(&mut self) {
        if signal_handling::is_sigterm_received()
            && self.service_repository.is_any_service_running()
        {
            println!("Going to stop all services..");
            self.stop_all_services();
        }
    }

    /// Main entrypoint
    pub fn run(&mut self) -> Result<()> {
        unsafe {
            prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        }

        signal_handling::init();

        // Spawn helper threads:
        let reaper_repo = ServiceRepository::new(
            self.service_repository.services.clone(),
            self.dispatcher.add_component(),
        );
        reaper::spawn(reaper_repo);

        let healthcheck_repo = ServiceRepository::new(
            self.service_repository.services.clone(),
            self.dispatcher.add_component(),
        );

        healthcheck::spawn(healthcheck_repo);

        self.dispatcher.clone().spawn();

        debug!("Threads spawned, going to start running services now!");

        loop {
            //TODO: a blocking update maybe? This loop should be executed onstatechange.
            self.service_repository.ingest("runtime");
            self.check_is_shutting_down();
            let runnable_services = self.service_repository.get_runnable_services();
            runnable_services.into_iter().for_each(|service_handler| {
                self.service_repository
                    .update_status(service_handler.name(), ServiceStatus::ToBeRun);
                //healthcheck::prepare_service(&service_handler).unwrap();
                run_spawning_thread(
                    service_handler.service().clone(),
                    self.service_repository.clone(),
                );
            });
            if self.service_repository.all_finished() {
                debug!("Result: {:?}", self.service_repository.services);
                debug!("All services have finished, exiting...");
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    }
    /**
    Send a kill signal to all the services in the "Running" state.
    **/
    pub fn stop_all_services(&mut self) {
        self.service_repository
            .mutate_service_status(|mut service| {
                if service.is_running() && service.pid().is_some() {
                    debug!("Going to send SIGTERM signal to pid {:?}", service.pid());
                    // TODO: It might happen that we try to kill something which in the meanwhile has exited.
                    // Thus here we should handle Error: Sys(ESRCH)
                    kill(*service.pid().unwrap(), SIGTERM)
                        .map_err(|err| eprintln!("Error: {:?}", err))
                        .unwrap();
                    service.set_status(ServiceStatus::InKilling);
                    return Some(service);
                }
                if service.is_initial() {
                    debug!(
                        "Never going to run {}, so setting it to finished.",
                        service.name()
                    );
                    service.set_status(ServiceStatus::Finished);
                    return Some(service);
                }
                None
            });
    }
}

/// Run another thread that will wait for the start delay, and handle the fork / exec.
fn run_spawning_thread(service: Service, mut service_repository: ServiceRepository) {
    std::thread::spawn(move || {
        std::thread::sleep(service.start_delay);
        match spawn_process(&service) {
            Ok(pid) => {
                debug!("Setting pid:{} for service: {}", pid, service.name);
                service_repository.update_pid(service.name, pid);
            }
            Err(error) => {
                error!("Failed spawning the process: {}", error);
                service_repository.update_status(service.name.as_ref(), ServiceStatus::Failed);
            }
        }
    });
}

/// Search for *.toml files in path, and deserialize them into Service.
fn fetch_services<P>(path: &P) -> Result<Vec<Service>>
where
    P: AsRef<Path> + ?Sized + AsRef<OsStr> + Debug,
{
    debug!("Fetching services from : {:?}", path);
    let is_toml_file = |path: &PathBuf| {
        let has_toml_extension = |path: &PathBuf| {
            path.extension()
                .unwrap_or_else(|| "".as_ref())
                .to_str()
                .unwrap()
                .ends_with("toml")
        };
        path.is_file() && has_toml_extension(path)
    };
    let dir = fs::read_dir(path)?;

    //TODO: option to decide to not start if the deserialization of any service failed.

    Ok(dir
        .filter_map(std::result::Result::ok)
        .map(|dir_entry| dir_entry.path())
        .filter(is_toml_file)
        .map(Service::from_file)
        .filter(Result::is_ok)
        .map(Result::unwrap)
        .collect())
}

/// Fork the process
fn spawn_process(service: &Service) -> Result<Pid> {
    match fork() {
        Ok(ForkResult::Child) => {
            debug!("Child PID: {}, PPID: {}.", getpid(), getppid());
            exec_service(service);
            unreachable!()
        }
        Ok(ForkResult::Parent { child, .. }) => {
            debug!("Spawned child with PID {}.", child);
            Ok(child)
        }

        Err(err) => Err(Into::into(err)),
    }
}

fn exec_service(service: &Service) {
    let default = PathBuf::from("/");
    let cwd = service.working_directory.as_ref().unwrap_or(&default);
    debug!("Set cwd: {:?}, ", cwd);
    std::env::set_current_dir(cwd).unwrap();
    let chunks: Vec<String> = shlex::split(service.command.as_ref()).unwrap();
    let program_name = CString::new(chunks.get(0).unwrap().as_str()).unwrap();
    let arg_cstrings = chunks
        .into_iter()
        .map(|arg| CString::new(arg).map_err(Into::into))
        .collect::<Result<Vec<_>>>()
        .unwrap();
    //arg_cstrings.insert(0, program_name.clone());
    debug!("args: {:?}", arg_cstrings);
    let arg_cptr: Vec<&CStr> = arg_cstrings.iter().map(|c| c.as_c_str()).collect();
    // TODO: clear signal mask if needed.
    nix::unistd::execvp(program_name.as_ref(), arg_cptr.as_ref()).expect("Execvp() failed: ");
}

#[cfg(test)]
mod test {
    use crate::horust::formats::Service;
    use crate::horust::runtime::fetch_services;
    use std::io;
    use tempdir::TempDir;

    fn create_test_dir() -> io::Result<TempDir> {
        let ret = TempDir::new("horust").unwrap();
        let a = Service::from_name("a");
        let b = Service::start_after("b", vec!["a"]);
        let a_str = toml::to_string(&a).unwrap();
        let b_str = toml::to_string(&b).unwrap();
        std::fs::write(ret.path().join("my-first-service.toml"), a_str)?;
        std::fs::write(ret.path().join("my-second-service.toml"), b_str)?;
        Ok(ret)
    }

    #[test]
    fn test_fetch_services() -> io::Result<()> {
        let tempdir = create_test_dir()?;
        std::fs::write(tempdir.path().join("not-a-service"), "Hello world")?;
        let res = fetch_services(tempdir.path()).unwrap();
        assert_eq!(res.len(), 2);
        let mut names: Vec<String> = res.into_iter().map(|serv| serv.name).collect();
        names.sort();
        assert_eq!(vec!["a", "b"], names);

        Ok(())
    }
}