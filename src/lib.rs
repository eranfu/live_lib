#![feature(once_cell)]

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::lazy::SyncLazy;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
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
    search_paths: Vec<PathBuf>,
}

impl<D, R: ResultExt<()>> Loader<D, R> {
    pub fn new(
        additional_search_paths: Vec<PathBuf>,
        post_load: fn(&mut D, &Lib) -> R,
        pre_unload: fn(&mut D, &Lib) -> R,
    ) -> Result<Self> {
        let mut search_paths = additional_search_paths;
        let mut exe_dir = std::env::current_exe().chain_err(|| "Failed to get current_exe")?;
        exe_dir.pop();
        #[cfg(test)]
        {
            exe_dir.pop();
        }
        search_paths.push(exe_dir);

        let (sender, receiver) = channel();
        Ok(Self {
            file_watcher: notify::watcher(sender, Duration::from_secs(2))
                .chain_err(|| "Failed to watch file change")?,
            file_receiver: receiver,
            origin_path_to_lib: Default::default(),
            name_to_origin_path: Default::default(),
            post_load,
            pre_unload,
            search_paths,
        })
    }

    pub fn add_library(&mut self, lib_name: &str, owner_data: &mut D) -> Result<()> {
        match self.name_to_origin_path.entry(lib_name.to_owned()) {
            Entry::Occupied(_occupied) => Ok(()),
            Entry::Vacant(vacant_origin_path) => {
                let (origin_path, load_path) = Self::search(&self.search_paths, lib_name)
                    .chain_err(|| format!("Library not exists. name: {}", lib_name))?;

                Self::copy(&origin_path, &load_path)?;

                let lib = unsafe { Library::new(&load_path) }
                    .chain_err(|| format!("Failed to load library. path: {:?}", load_path))?;
                self.file_watcher
                    .watch(&origin_path, RecursiveMode::NonRecursive)
                    .chain_err(|| "Failed to add watch")?;
                let lib = Lib {
                    lib,
                    name: lib_name.to_owned(),
                    load_path,
                    origin_path,
                };
                let origin_path = vacant_origin_path.insert(lib.origin_path.clone());
                self.origin_path_to_lib.insert(origin_path.clone(), lib);
                (self.post_load)(owner_data, &self.origin_path_to_lib[origin_path])
                    .chain_err(|| ErrorKind::PostLoadError)
            }
        }
    }

    pub fn remove_library(&mut self, _name: &str) {}

    pub fn update(&self, _owner_data: &mut D) {}

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
                static LIVE_SUFFIX: SyncLazy<&OsStr> = SyncLazy::new(|| "_live".as_ref());
                let live_suffix = *LIVE_SUFFIX;
                let mut live_file_name =
                    OsString::with_capacity(lib_name.len() + live_suffix.len());
                live_file_name.push(lib_name);
                live_file_name.push(live_suffix);
                let load_path = dir.join(libloading::library_filename(live_file_name));
                return Some((origin_path, load_path));
            }
        }
        None
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
