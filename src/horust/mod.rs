mod error;
mod formats;
mod reaper;

pub use self::error::HorustError;
use self::error::Result;
use self::formats::{RestartStrategy, ServiceStatus};
pub use formats::Service;
use libc::STDOUT_FILENO;
use libc::{prctl, PR_SET_CHILD_SUBREAPER};
use nix::sys::signal::kill;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, SIGINT, SIGTERM};
use nix::unistd::{fork, getppid, ForkResult};
use nix::unistd::{getpid, Pid};
use std::ffi::{c_void, CStr, CString, OsStr};
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct SignalSafe;

impl SignalSafe {
    fn print(s: &str) {
        unsafe {
            libc::write(STDOUT_FILENO, s.as_ptr() as *const c_void, s.len());
        }
    }
}

static mut SIGTERM_RECEIVED: bool = false;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceHandler {
    service: Service,
    status: ServiceStatus,
    pid: Option<Pid>,
}

impl From<Service> for ServiceHandler {
    fn from(service: Service) -> Self {
        ServiceHandler {
            service,
            status: ServiceStatus::Initial,
            pid: None,
        }
    }
}

impl From<ServiceHandler> for Service {
    fn from(sh: ServiceHandler) -> Self {
        sh.service
    }
}

impl ServiceHandler {
    fn start_after(&self) -> &Vec<String> {
        self.service.start_after.as_ref()
    }
    fn name(&self) -> &str {
        self.service.name.as_str()
    }
    fn set_pid(&mut self, pid: Pid) {
        self.status = ServiceStatus::Running;
        self.pid = Some(pid);
    }
    pub fn is_running(&self) -> bool {
        self.status == ServiceStatus::Running
    }
    pub fn is_finished(&self) -> bool {
        match self.status {
            ServiceStatus::Finished | ServiceStatus::FinishedFailed => true,
            _ => false,
        }
    }
    pub fn set_status_by_exit_code(&mut self, exit_code: i32) {
        let has_failed = exit_code != 0;
        match self.service.restart_strategy {
            RestartStrategy::Never => {
                debug!("Pid successfully exited.");
                // Will never be restarted, even if failed:
                self.status = if has_failed {
                    ServiceStatus::FinishedFailed
                } else {
                    ServiceStatus::Finished
                };
            }
            RestartStrategy::OnFailure => {
                self.status = if has_failed {
                    ServiceStatus::Initial
                } else {
                    ServiceStatus::Finished
                };
                debug!("Going to rerun the process because it failed!");
            }
            RestartStrategy::Always => {
                self.status = ServiceStatus::Initial;
            }
        };
        self.pid = None;
    }
}

#[derive(Debug)]
pub struct Horust {
    supervised: Arc<Mutex<Vec<ServiceHandler>>>,
}

impl Horust {
    pub fn new(services: Vec<Service>) -> Self {
        Horust {
            //TODO: change to map [service_name: service]
            supervised: Arc::new(Mutex::new(
                services.into_iter().map(ServiceHandler::from).collect(),
            )),
        }
    }

    pub fn run(&mut self) -> Result<()> {
        unsafe {
            prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        }
        self.setup_signal_handling();
        let supervised = Arc::clone(&self.supervised);
        std::thread::spawn(|| {
            reaper::supervisor_thread(supervised);
        });
        debug!("Going to start services!");
        loop {
            if unsafe { SIGTERM_RECEIVED }
                && self
                    .supervised
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|sh| sh.is_running())
            {
                println!("Going to stop all services..");
                self.stop_all_services();
            }
            let mut superv_services = self.supervised.lock().unwrap();
            *superv_services = superv_services
                .iter()
                .cloned()
                .map(|mut service_handler| {
                    // Check if all dependant services are either running or finished:
                    let check_can_run = |dependencies: &Vec<String>| {
                        let mut can_run = true;
                        for service_name in dependencies {
                            for service in superv_services.iter() {
                                let is_not_started = service.name() == *service_name
                                    && (service.status != ServiceStatus::Running
                                        && service.status != ServiceStatus::Finished);
                                if is_not_started {
                                    can_run = false;
                                    break;
                                }
                            }
                        }
                        can_run
                    };

                    if service_handler.status == ServiceStatus::Initial
                        && check_can_run(service_handler.start_after())
                    {
                        service_handler.status = ServiceStatus::ToBeRun;
                        let supervised_ref = Arc::clone(&self.supervised);
                        let service = service_handler.service.clone();
                        std::thread::spawn(move || {
                            let pid =
                                Horust::run_service(&service).expect("Failed spawning service!");
                            supervised_ref
                                .lock()
                                .unwrap()
                                .iter_mut()
                                .filter(|sh| sh.name() == service.name)
                                .for_each(|sh| {
                                    debug!("Now it's running!");
                                    // TODO: if status was finished, then send a sigterm.
                                    sh.set_pid(pid);
                                });
                        });
                    }
                    service_handler
                })
                .collect();
            let all_finished = superv_services.iter().all(|sh| sh.is_finished());
            if all_finished {
                break;
            }
        }
        Ok(())
    }
    pub fn stop_all_services(&self) {
        self.supervised
            .lock()
            .unwrap()
            .iter_mut()
            .for_each(|service| {
                if let Some(pid) = service.pid {
                    kill(pid, SIGTERM)
                        .map_err(|err| eprintln!("Error: {:?}", err))
                        .unwrap();
                }
                // Removes `Initial` and ToBeRun services.
                service.status = ServiceStatus::Finished
            });
    }

    pub fn run_service(service: &Service) -> Result<Pid> {
        std::thread::sleep(service.start_delay);
        Horust::spawn_process(service)
    }

    /// Create a new horust instance from a path of services.
    pub fn from_services_dir<P>(path: &P) -> Result<Horust>
    where
        P: AsRef<Path> + ?Sized + AsRef<OsStr> + Debug,
    {
        Self::fetch_services(path).map_err(Into::into).map(|servs| {
            debug!("Services found: {:?}", servs);
            Horust::new(servs)
        })
    }

    /// Search for *.toml files in path, and deserialize them into Service.
    pub fn fetch_services<P>(path: &P) -> Result<Vec<Service>>
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
        dir.filter_map(std::result::Result::ok)
            .map(|dir_entry| dir_entry.path())
            .filter(is_toml_file)
            .map(Service::from_file)
            .collect::<Result<Vec<Service>>>()
    }

    pub fn spawn_process(service: &Service) -> Result<Pid> {
        match fork() {
            Ok(ForkResult::Child) => {
                debug!("Child PID: {}, PPID: {}.", getpid(), getppid());
                Horust::exec_service(service);
                unreachable!()
            }

            Ok(ForkResult::Parent { child, .. }) => {
                debug!("Spawned child with PID {}.", child);
                Ok(child)
            }

            Err(err) => Err(HorustError::from(err)),
        }
    }
    pub fn exec_service(service: &Service) {
        debug!("Set cwd: {:?}", &service.working_directory);
        std::env::set_current_dir(&service.working_directory).unwrap();
        let mut chunks: Vec<&str> = service.command.split_whitespace().collect();
        let filename = CString::new(chunks.remove(0)).unwrap();

        let mut arg_cstrings = chunks
            .into_iter()
            .map(|arg| CString::new(arg).map_err(HorustError::from))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        arg_cstrings.insert(0, filename.clone());
        debug!("args: {:?}", arg_cstrings);
        let arg_cptr: Vec<&CStr> = arg_cstrings.iter().map(|c| c.as_c_str()).collect();
        // TODO: clear signal mask if needed.
        nix::unistd::execvp(filename.as_ref(), arg_cptr.as_ref()).expect("Execvp() failed: ");
    }

    fn setup_signal_handling(&self) {
        // To allow auto restart on some syscalls,
        // for example: `waitpid`.
        let flags = SaFlags::SA_RESTART;
        let sig_action = SigAction::new(
            SigHandler::Handler(Horust::handle_sigterm),
            flags,
            SigSet::empty(),
        );

        if let Err(err) = unsafe { sigaction(SIGTERM, &sig_action) } {
            panic!("sigaction() failed: {}", err);
        };
        if let Err(err) = unsafe { sigaction(SIGINT, &sig_action) } {
            panic!("sigaction() failed: {}", err);
        };
    }
    extern "C" fn handle_sigterm(_: libc::c_int) {
        SignalSafe::print("Received SIGTERM.\n");
        unsafe {
            SIGTERM_RECEIVED = true;
        }
        //SignalSafe::exit(1);
    }
}

#[cfg(test)]
mod test {
    use crate::horust::formats::Service;
    use crate::horust::Horust;
    use std::io;
    use tempdir::TempDir;

    //TODO
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
        let res = Horust::fetch_services(tempdir.path()).unwrap();
        assert_eq!(res.len(), 2);
        let names: Vec<String> = res.into_iter().map(|serv| serv.name).collect();
        assert_eq!(vec!["a", "b"], names);

        Ok(())
    }
}
