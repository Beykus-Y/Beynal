// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc,
};

const ENTRY_LIMIT: usize = 50_000;

pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub directory: bool,
}

struct IndexedFile {
    path: PathBuf,
    name: String,
    searchable: String,
}

pub struct Index {
    pub children: HashMap<PathBuf, Vec<Entry>>,
    files: Vec<IndexedFile>,
    pub limited: bool,
}

impl Index {
    fn scan(root: &Path, alive: &mpsc::Receiver<()>, limit: usize) -> io::Result<Self> {
        let mut index = Self {
            children: HashMap::new(),
            files: Vec::new(),
            limited: false,
        };
        let mut dirs = vec![root.to_path_buf()];
        let mut remaining = limit;
        while let Some(dir) = dirs.pop() {
            if matches!(alive.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                break;
            }
            let listing = match fs::read_dir(&dir) {
                Ok(listing) => listing,
                Err(error) if dir == root => return Err(error),
                Err(_) => continue,
            };
            let mut entries = Vec::new();
            for item in listing {
                if remaining == 0 {
                    index.limited = true;
                    break;
                }
                remaining -= 1;
                let Ok(item) = item else { continue };
                let Ok(kind) = item.file_type() else { continue };
                let name = item.file_name().to_string_lossy().into_owned();
                if kind.is_dir()
                    && matches!(name.as_str(), ".git" | "target" | "node_modules" | ".venv")
                {
                    continue;
                }
                let path = item.path();
                if kind.is_dir() {
                    dirs.push(path.clone());
                } else if kind.is_file() || (kind.is_symlink() && path.is_file()) {
                    let relative = path.strip_prefix(root).unwrap_or(&path);
                    let name = relative.to_string_lossy().into_owned();
                    index.files.push(IndexedFile {
                        path: path.clone(),
                        searchable: name.to_lowercase(),
                        name,
                    });
                } else {
                    continue;
                }
                entries.push(Entry {
                    path,
                    name,
                    directory: kind.is_dir(),
                });
            }
            entries.sort_unstable_by(|a, b| {
                b.directory
                    .cmp(&a.directory)
                    .then(a.name.cmp(&b.name))
                    .then(a.path.cmp(&b.path))
            });
            index.children.insert(dir, entries);
            if index.limited {
                break;
            }
        }
        index
            .files
            .sort_unstable_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
        Ok(index)
    }

    pub fn find(&self, query: &str, limit: usize) -> Vec<(PathBuf, String)> {
        let query = query.to_lowercase();
        self.files
            .iter()
            .filter(|file| file.searchable.contains(&query))
            .take(limit)
            .map(|file| (file.path.clone(), file.name.clone()))
            .collect()
    }
}

pub struct Project {
    pub root: PathBuf,
    pub index: Option<Index>,
    job: Option<(mpsc::Receiver<io::Result<Index>>, mpsc::Sender<()>)>,
}

impl Project {
    pub fn new(root: PathBuf) -> Self {
        let mut project = Self {
            root,
            index: None,
            job: None,
        };
        project.refresh();
        project
    }

    pub fn refresh(&mut self) {
        let root = self.root.clone();
        let (tx, rx) = mpsc::channel();
        let (keep_alive, canceled) = mpsc::channel();
        let worker = tx.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("bedit-index".into())
            .spawn(move || {
                let _ = worker.send(Index::scan(&root, &canceled, ENTRY_LIMIT));
            })
        {
            let _ = tx.send(Err(error));
        }
        self.job = Some((rx, keep_alive));
    }

    pub fn loading(&self) -> bool {
        self.job.is_some()
    }

    pub fn poll(&mut self) -> Option<io::Result<()>> {
        let result = match self.job.as_ref()?.0.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err(io::Error::other("project scan stopped")),
        };
        self.job = None;
        Some(result.map(|index| self.index = Some(index)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_finds_nested_unicode_and_avoids_directory_loops() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/sub")).unwrap();
        fs::create_dir_all(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("src/sub/Код.rs"), "").unwrap();
        fs::write(dir.path().join("target/hidden.rs"), "").unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("src/loop")).unwrap();
        let (_keep, alive) = mpsc::channel();
        let index = Index::scan(dir.path(), &alive, ENTRY_LIMIT).unwrap();
        assert_eq!(index.find("код", 10)[0].1, "src/sub/Код.rs");
        assert_eq!(index.find("", 10).len(), 1);
        assert_eq!(index.children[dir.path()].len(), 1);
    }
    #[test]
    fn index_bounds_work_for_large_directories() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..1500 {
            fs::File::create(dir.path().join(format!("file-{i}.rs"))).unwrap();
        }
        let (_keep, alive) = mpsc::channel();
        let partial = Index::scan(dir.path(), &alive, 1000).unwrap();
        assert!(partial.limited);
        assert_eq!(partial.children[dir.path()].len(), 1000);
        assert_eq!(partial.find(".RS", 128).len(), 128);
    }
}
