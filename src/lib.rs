#![feature(map_first_last)]

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::Duration;

use ::error_chain::*;
pub use libloading::Library;
pub use libloading::Symbol;
use notify::{DebouncedEvent, RecommendedWatcher, RecursiveMode, Watcher};

error_chain! {
    errors {
        LoadError
        UnloadError
    }
}

pub struct Loader<P: LibPartner = ()> {
    file_watcher: RecommendedWatcher,
    file_receiver: Receiver<DebouncedEvent>,
    lib_name_to_lib: HashMap<String, Lib<P>>,
    origin_path_to_lib_name: HashMap<PathBuf, String>,
    search_dirs: Vec<PathBuf>,
    pending_remove: BTreeSet<PathBuf>,
}

impl<P: LibPartner> Loader<P> {
    pub fn new(additional_search_dirs: Vec<PathBuf>) -> Result<Self> {
        let mut search_dirs = additional_search_dirs;
        let mut exe_dir = std::env::current_exe().chain_err(|| "Failed to get current_exe")?;
        exe_dir.pop();
        if exe_dir.file_name() == Some(OsStr::new("deps")) {
            exe_dir.pop();
        }
        search_dirs.push(exe_dir);

        let (sender, receiver) = channel();
        Ok(Self {
            file_watcher: notify::watcher(sender, Duration::from_secs(2))
                .chain_err(|| "Failed to watch file change")?,
            file_receiver: receiver,
            lib_name_to_lib: Default::default(),
            origin_path_to_lib_name: Default::default(),
            search_dirs,
            pending_remove: Default::default(),
        })
    }

    pub fn add_library(&mut self, lib_name: &str) -> Result<()> {
        if self.lib_name_to_lib.contains_key(lib_name) {
            return Ok(());
        }

        let (origin_path, load_path) =
            Self::search(&self.search_dirs, lib_name, &mut self.pending_remove)
                .chain_err(|| format!("Library not exists. name: {}", lib_name))?;

        self.add(lib_name.to_owned(), origin_path.clone(), load_path)?;
        self.file_watcher
            .watch(&origin_path, RecursiveMode::NonRecursive)
            .chain_err(|| format!("Failed to watch file. path: {:?}", origin_path))?;
        Ok(())
    }

    pub fn remove_library(&mut self, lib_name: &str) -> Result<()> {
        if let Some(lib) = self.lib_name_to_lib.get(lib_name) {
            self.file_watcher
                .unwatch(&lib.origin_path)
                .chain_err(|| format!("Failed to unwatch file. path: {:?}", lib.origin_path))?;
            let origin_path = lib.origin_path.clone();
            self.remove(&origin_path)?;
        }

        Ok(())
    }

    pub fn get(&self, lib_name: &str) -> Option<(&Library, &P)> {
        self.lib_name_to_lib
            .get(lib_name)
            .map(|lib| (&lib.lib, lib.partner.as_ref().unwrap()))
    }

    pub fn update(&mut self) -> Result<()> {
        while let Some(to_remove) = self.pending_remove.first() {
            if to_remove.exists() && std::fs::remove_file(&to_remove).is_err() {
                break;
            } else {
                self.pending_remove.pop_first();
            };
        }

        loop {
            let event = self.file_receiver.try_recv();
            match event {
                Ok(event) => match event {
                    DebouncedEvent::Create(path) | DebouncedEvent::Write(path) => {
                        let lib_name = self.remove(&path)?;
                        let mut dir = path.clone();
                        dir.pop();
                        let load_path =
                            Self::get_load_path(dir, &lib_name, &mut self.pending_remove);
                        self.add(lib_name, path, load_path)?;
                    }

                    DebouncedEvent::NoticeWrite(_)
                    | DebouncedEvent::NoticeRemove(_)
                    | DebouncedEvent::Remove(_)
                    | DebouncedEvent::Rename(_, _)
                    | DebouncedEvent::Chmod(_)
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

    fn search(
        search_dirs: &[PathBuf],
        lib_name: &str,
        pending_remove: &mut BTreeSet<PathBuf>,
    ) -> Option<(PathBuf, PathBuf)> {
        let file_name = libloading::library_filename(lib_name);
        for dir in search_dirs {
            let origin_path = dir.join(&file_name);
            if origin_path.exists() {
                let load_path = Self::get_load_path(dir.clone(), lib_name, pending_remove);
                return Some((origin_path, load_path));
            }
        }
        None
    }

    fn add(&mut self, lib_name: String, origin_path: PathBuf, load_path: PathBuf) -> Result<()> {
        Self::copy(&origin_path, &load_path)?;

        let lib = unsafe { Library::new(&load_path) }
            .chain_err(|| format!("Failed to load library. path: {:?}", load_path))?;
        let mut lib = Lib {
            lib,
            lib_name,
            load_path,
            origin_path,
            partner: None,
        };
        lib.partner = Some(P::load(&lib.lib).chain_err(|| ErrorKind::LoadError)?);
        self.origin_path_to_lib_name
            .insert(lib.origin_path.clone(), lib.lib_name.clone());
        self.lib_name_to_lib.insert(lib.lib_name.clone(), lib);
        Ok(())
    }

    fn remove(&mut self, origin_path: &Path) -> Result<String> {
        let lib_name = self
            .origin_path_to_lib_name
            .remove(origin_path)
            .chain_err(|| format!("Failed to find lib_name. origin_path: {:?}", origin_path))?;
        Self::remove_file(
            self.lib_name_to_lib
                .remove(&lib_name)
                .chain_err(|| format!("Failed to find lib. lib_name: {:?}", lib_name))?,
            &mut self.pending_remove,
        );
        Ok(lib_name)
    }

    fn remove_file(lib: Lib<P>, pending_remove: &mut BTreeSet<PathBuf>) {
        let load_path = lib.load_path.clone();
        drop(lib);
        if std::fs::remove_file(&load_path).is_err() {
            pending_remove.insert(load_path);
        }
    }

    fn get_load_path(
        mut dir: PathBuf,
        lib_name: &str,
        pending_remove: &mut BTreeSet<PathBuf>,
    ) -> PathBuf {
        use std::env::consts::*;
        const LIVE_SUFFIX: &str = "_live";
        let mut i = 0;
        let mut live_file_name = String::with_capacity(
            DLL_PREFIX.len() + lib_name.len() + LIVE_SUFFIX.len() + 3 + DLL_SUFFIX.len(),
        );
        live_file_name += DLL_PREFIX;
        live_file_name += lib_name;
        live_file_name += LIVE_SUFFIX;
        let len = live_file_name.len();
        loop {
            live_file_name += &i.to_string();
            live_file_name += DLL_SUFFIX;
            dir.push(&live_file_name);

            if !dir.exists() {
                return dir;
            }
            if std::fs::remove_file(&dir).is_ok() {
                pending_remove.remove(&dir);
                return dir;
            }

            dir.pop();
            live_file_name.truncate(len);
            i += 1;
        }
    }
}

impl<P: LibPartner> Drop for Loader<P> {
    fn drop(&mut self) {
        self.origin_path_to_lib_name.clear();
        for (_, lib) in self.lib_name_to_lib.drain() {
            Self::remove_file(lib, &mut self.pending_remove);
        }
        for to_remove in self.pending_remove.iter() {
            while to_remove.exists() && std::fs::remove_file(to_remove).is_err() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

pub trait LibPartner: Sized {
    type LoadResult: ResultExt<Self>;
    type UnloadResult: ResultExt<()>;
    fn load(lib: &Library) -> Self::LoadResult;
    fn unload(&mut self, lib: &Library) -> Self::UnloadResult;
}

impl LibPartner for () {
    type LoadResult = Result<Self>;
    type UnloadResult = Result<()>;

    fn load(_lib: &Library) -> Self::LoadResult {
        Ok(())
    }

    fn unload(&mut self, _lib: &Library) -> Self::UnloadResult {
        Ok(())
    }
}

struct Lib<P: LibPartner> {
    lib: Library,
    lib_name: String,
    load_path: PathBuf,
    origin_path: PathBuf,
    partner: Option<P>,
}

impl<P: LibPartner> Drop for Lib<P> {
    fn drop(&mut self) {
        if let Some(err) = self
            .partner
            .take()
            .map(|mut p| p.unload(&self.lib))
            .chain_err(|| ErrorKind::UnloadError)
            .err()
        {
            eprintln!("{}", err.display_chain());
        }
    }
}
