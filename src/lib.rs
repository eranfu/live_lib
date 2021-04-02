#![feature(once_cell)]

use std::collections::{HashMap, LinkedList};
use std::collections::hash_map::Entry;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::Duration;

use ::error_chain::*;
use libloading::Library;
pub use libloading::Symbol;
use notify::{DebouncedEvent, RecommendedWatcher, RecursiveMode, Watcher};

error_chain! {
    errors {
        PostLoadError
        PreUnloadError
    }
}

pub struct Loader<D, R: ResultExt<()>> {
    file_watcher: RecommendedWatcher,
    file_receiver: Receiver<DebouncedEvent>,
    origin_path_to_lib: HashMap<PathBuf, Lib>,
    name_to_origin_path: HashMap<String, PathBuf>,
    post_load: fn(&mut D, &Lib) -> R,
    pre_unload: fn(&mut D, &Lib) -> R,
    search_dirs: Vec<PathBuf>,
    pending_remove: LinkedList<PathBuf>,
}

impl<D, R: ResultExt<()>> Loader<D, R> {
    pub fn new(
        additional_search_dirs: Vec<PathBuf>,
        post_load: fn(&mut D, &Lib) -> R,
        pre_unload: fn(&mut D, &Lib) -> R,
    ) -> Result<Self> {
        let mut search_dirs = additional_search_dirs;
        let mut exe_dir = std::env::current_exe().chain_err(|| "Failed to get current_exe")?;
        exe_dir.pop();
        #[cfg(test)]
            {
                exe_dir.pop();
            }
        search_dirs.push(exe_dir);

        let (sender, receiver) = channel();
        Ok(Self {
            file_watcher: notify::watcher(sender, Duration::from_secs(2))
                .chain_err(|| "Failed to watch file change")?,
            file_receiver: receiver,
            origin_path_to_lib: Default::default(),
            name_to_origin_path: Default::default(),
            post_load,
            pre_unload,
            search_dirs,
            pending_remove: Default::default(),
        })
    }

    pub fn add_library(&mut self, lib_name: &str, owner_data: &mut D) -> Result<()> {
        let (origin_path, load_path) = match self.name_to_origin_path.entry(lib_name.to_owned()) {
            Entry::Occupied(_occupied) => return Ok(()),
            Entry::Vacant(vacant_origin_path) => {
                let (origin_path, load_path) = Self::search(&self.search_dirs, lib_name)
                    .chain_err(|| format!("Library not exists. name: {}", lib_name))?;
                (vacant_origin_path.insert(origin_path).clone(), load_path)
            }
        };
        self.add(
            lib_name.to_owned(),
            origin_path.clone(),
            load_path,
            owner_data,
        )?;
        self.file_watcher
            .watch(&origin_path, RecursiveMode::NonRecursive)
            .chain_err(|| format!("Failed to watch file. path: {:?}", origin_path))
    }

    pub fn remove_library(&mut self, lib_name: &str, owner_data: &mut D) -> Result<()> {
        if let Some(origin_path) = self.name_to_origin_path.remove(lib_name) {
            self.file_watcher
                .unwatch(&origin_path)
                .chain_err(|| format!("Failed to unwatch file. path: {:?}", origin_path))?;
            self.remove(&origin_path, owner_data)?;
        }

        Ok(())
    }

    pub fn update(&mut self, owner_data: &mut D) -> Result<()> {
        while let Some(to_remove) = self.pending_remove.front() {
            if std::fs::remove_file(&to_remove).is_err() {
                break;
            } else {
                self.pending_remove.pop_front();
            };
        }

        loop {
            let event = self.file_receiver.try_recv();
            match event {
                Ok(event) => match event {
                    DebouncedEvent::NoticeWrite(path)
                    | DebouncedEvent::Create(path)
                    | DebouncedEvent::Write(path) => {
                        if let Some(error) = self.remove(&path, owner_data).err() {
                            eprint!(
                                "Failed to remove library. origin_path: {:?}, error: {}",
                                path,
                                error.display_chain()
                            )
                        }

                        let mut load_path = path.clone();
                        let file_name: &str = load_path
                            .file_name()
                            .and_then(|file_name| file_name.to_str())
                            .chain_err(|| {
                                format!("Failed to get filename. path: {:?}", load_path)
                            })?;
                        let lib_name = utils::extract_lib_name(file_name).chain_err(|| {
                            format!("Failed to extract lib_name. file_name: {}", file_name)
                        })?;
                        let live_file_name = utils::get_load_path(&OsString::from(&lib_name));
                        load_path.set_file_name(live_file_name);
                        self.add(lib_name, path, load_path, owner_data)?;
                    }

                    DebouncedEvent::NoticeRemove(path)
                    | DebouncedEvent::Remove(path)
                    | DebouncedEvent::Rename(path, _) => {
                        self.remove(&path, owner_data)?;
                    }

                    DebouncedEvent::Chmod(_)
                    | DebouncedEvent::Rescan
                    | DebouncedEvent::Error(_, _) => {}
                },
                Err(error) => match error {
                    TryRecvError::Empty => {
                        break;
                    }
                    TryRecvError::Disconnected => {
                        bail!("File watcher disconnected")
                    }
                },
            }
        }

        Ok(())
    }

    fn copy(origin: &Path, load: &Path) -> Result<()> {
        std::fs::copy(origin, load)
            .chain_err(|| format!("Failed to copy library. from: {:?}, to: {:?}", origin, load))?;
        Ok(())
    }

    fn search(search_dirs: &[PathBuf], lib_name: impl AsRef<OsStr>) -> Option<(PathBuf, PathBuf)> {
        let lib_name = lib_name.as_ref();
        let file_name = libloading::library_filename(lib_name);
        for dir in search_dirs {
            let origin_path = dir.join(&file_name);
            if origin_path.exists() {
                let live_lib_name = utils::get_load_path(dir, lib_name);
                let load_path = ;
                return Some((origin_path, load_path));
            }
        }
        None
    }

    fn add(
        &mut self,
        lib_name: String,
        origin_path: PathBuf,
        load_path: PathBuf,
        owner_data: &mut D,
    ) -> Result<()> {
        Self::copy(&origin_path, &load_path)?;

        let lib = unsafe { Library::new(&load_path) }
            .chain_err(|| format!("Failed to load library. path: {:?}", load_path))?;
        let lib = Lib {
            lib,
            name: lib_name,
            load_path,
            origin_path,
        };
        (self.post_load)(owner_data, &lib).chain_err(|| ErrorKind::PostLoadError)?;
        self.origin_path_to_lib.insert(lib.origin_path.clone(), lib);
        Ok(())
    }
    fn remove(&mut self, origin_path: &Path, owner_data: &mut D) -> Result<()> {
        let lib = self
            .origin_path_to_lib
            .remove(origin_path)
            .chain_err(|| format!("Failed to find lib. origin_path: {:?}", origin_path))?;
        (self.pre_unload)(owner_data, &lib).chain_err(|| ErrorKind::PreUnloadError)?;
        let load_path = lib.load_path.clone();
        drop(lib);
        self.pending_remove.push_back(load_path);
        Ok(())
    }
}

pub struct Lib {
    lib: Library,
    name: String,
    load_path: PathBuf,
    origin_path: PathBuf,
}

impl Lib {
    pub fn name(&self) -> &String {
        &self.name
    }
    pub fn lib(&self) -> &Library {
        &self.lib
    }
}

mod utils {
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    use ::error_chain::error_chain;
    use lazy_static::lazy_static;
    use regex::Regex;

    error_chain! {}

    pub(crate) fn get_load_path(dir: PathBuf, lib_name: &OsStr) -> OsString {
        dir.join(libloading::library_filename(live_lib_name))
        let live_suffix: &OsStr = "_live".as_ref();
        let mut live_file_name = OsString::with_capacity(lib_name.len() + live_suffix.len());
        live_file_name.push(lib_name);
        live_file_name.push(live_suffix);
        live_file_name
    }

    pub(crate) fn extract_lib_name(file_name: &str) -> Result<String> {
        lazy_static! {
            static ref REGEX: Regex = Regex::new(&format!(
                r"^{}(.+){}$",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX,
            ))
            .unwrap();
        }

        REGEX
            .captures(file_name)
            .and_then(|cap| cap.get(1))
            .map(|ma| ma.as_str().to_owned())
            .chain_err(|| format!("Failed to get lib_name. file_name: {:?}", file_name))
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn extract_lib_name() {
            let file_name = format!(
                "{}gl32{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            );
            assert_eq!(crate::utils::extract_lib_name(&file_name).unwrap(), "gl32");

            let file_name = format!(
                "{}gl32{}a",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            );
            assert!(crate::utils::extract_lib_name(&file_name).is_err());
        }
    }
}
