mod bus;
mod error;
mod formats;
mod healthcheck;
mod reaper;
mod runtime;
mod signal_handling;

pub use self::error::HorustError;
pub use self::formats::{get_sample_service, ExitStatus};
use crate::horust::bus::Bus;
use crate::horust::error::Result;
use crate::horust::formats::{validate, Service};
pub use formats::Event;
use libc::{prctl, PR_SET_CHILD_SUBREAPER};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use structopt::StructOpt;

#[derive(Debug, StructOpt, Serialize, Deserialize)]
pub struct HorustConfig {
    #[structopt(long)]
    /// Exits with an unsuccessful exit code if any process is in FinishedFailed state
    pub unsuccessful_exit_finished_failed: bool,
}

impl HorustConfig {
    /// Load the config file, and handles the merge with the options defined in the cmdline.
    /// Cmdline defined values have precedence over config based values.
    pub fn load_and_merge(cmd_line: HorustConfig, path: &Path) -> Result<Self> {
        let config_file = if path.exists() {
            let content = std::fs::read_to_string(path)?;
            toml::from_str::<HorustConfig>(content.as_str()).map_err(HorustError::from)?
        } else {
            Default::default()
        };

        let unsuccessful_exit_finished_failed = cmd_line.unsuccessful_exit_finished_failed
            || config_file.unsuccessful_exit_finished_failed;

        Ok(HorustConfig {
            unsuccessful_exit_finished_failed,
        })
    }
}

impl Default for HorustConfig {
    fn default() -> Self {
        Self {
            unsuccessful_exit_finished_failed: false,
        }
    }
}

#[derive(Debug)]
pub struct Horust {
    pub services: Vec<Service>,
    services_dir: Option<PathBuf>,
}

impl Horust {
    fn new(services: Vec<Service>, services_dir: Option<PathBuf>) -> Self {
        Horust {
            services,
            services_dir,
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
        let services = fetch_services(&path)?;
        validate(services)
            .map_err(Into::into)
            .map(|services| Horust::new(services, Some(PathBuf::from(path))))
    }

    pub fn run(&mut self) -> ExitStatus {
        unsafe {
            prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        }
        signal_handling::init();

        let mut dispatcher = Bus::new();
        debug!("Services: {:?}", self.services);
        // Spawn helper threads:
        debug!("Spawning threads:, going to start running services now!");
        reaper::spawn(dispatcher.join_bus());
        healthcheck::spawn(dispatcher.join_bus(), self.services.clone());
        let handle = runtime::spawn(dispatcher.join_bus(), self.services.clone());
        dispatcher.run();
        handle.join().unwrap()
    }
}

/// List files in path, filtering out directories
fn list_files<P: AsRef<Path>>(path: P) -> std::io::Result<Vec<PathBuf>> {
    fs::read_dir(path)?
        .filter_map(|entry| entry.ok())
        .try_fold(vec![], |mut ret, entry| {
            entry.file_type().map(|ftype| {
                if ftype.is_file() {
                    ret.push(entry.path());
                }
                ret
            })
        })
}

/// Search for *.toml files in path, and deserialize them into Service.
fn fetch_services<P>(path: &P) -> Result<Vec<Service>>
where
    P: AsRef<Path> + ?Sized + AsRef<OsStr> + Debug,
{
    debug!("Fetching services from : {:?}", path);
    debug!("Files: {:?}", list_files(path));
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
    let services = dir
        .filter_map(std::result::Result::ok)
        .map(|dir_entry| dir_entry.path())
        .filter(is_toml_file)
        .map(|file| {
            let res = Service::from_file(&file);
            res.map(|mut service| {
                if service.name == "" {
                    let filename = file.file_name().unwrap().to_str().unwrap().to_owned();
                    service.name = filename;
                }
                service
            })
            .map_err(|error| {
                error!("Error loading toml file: {}", error);
                error
            })
        })
        .filter_map(Result::ok)
        .collect::<Vec<Service>>();
    if services.is_empty() {
        error!("Horust: No services found in: {:?}", path);
    }
    Ok(services)
}

#[cfg(test)]
mod test {
    use crate::horust::formats::Service;
    use crate::horust::{fetch_services, list_files};
    use std::io;
    use std::path::PathBuf;
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

    #[test]
    fn test_list_files() -> io::Result<()> {
        let tempdir = TempDir::new("horust").unwrap();
        let files = vec!["a", "b", "c"];
        let files: Vec<PathBuf> = files
            .into_iter()
            .map(|f| tempdir.path().join(f).to_path_buf())
            .collect();

        for f in &files {
            std::fs::write(f, "Hello world")?;
        }
        let dirs = vec!["1", "2", "3"];
        for d in dirs {
            std::fs::create_dir(tempdir.path().join(d))?;
        }
        let mut res = list_files(tempdir.path())?;
        res.sort();
        assert_eq!(res, files);

        Ok(())
    }
}
