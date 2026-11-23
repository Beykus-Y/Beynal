// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};

const ENTRY_LIMIT: usize = 50_000;

pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub directory: bool,
}

impl Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .directory
            .cmp(&self.directory)
            .then(self.name.cmp(&other.name))
            .then(self.path.cmp(&other.path))
    }
}

fn ignored_directory(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | "target" | "node_modules" | ".venv")
    )
}

struct IndexedFile {
    path: PathBuf,
    name: String,
    searchable: String,
}

impl IndexedFile {
    fn new(root: &Path, path: &Path) -> Self {
        let relative = path.strip_prefix(root).unwrap_or(path);
        let name = relative.to_string_lossy().into_owned();
        Self {
            path: path.to_path_buf(),
            searchable: name.to_lowercase(),
            name,
        }
    }

    fn cmp(&self, other: &Self) -> Ordering {
        self.name.cmp(&other.name).then(self.path.cmp(&other.path))
    }
}

pub struct Index {
    children: HashMap<PathBuf, Vec<Arc<Entry>>>,
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
                let name = item.file_name();
                if kind.is_dir() && ignored_directory(&name) {
                    continue;
                }
                let name = name.to_string_lossy().into_owned();
                let path = item.path();
                if kind.is_dir() {
                    dirs.push(path.clone());
                } else if kind.is_file() || (kind.is_symlink() && path.is_file()) {
                    index.files.push(IndexedFile::new(root, &path));
                } else {
                    continue;
                }
                entries.push(Arc::new(Entry {
                    path,
                    name,
                    directory: kind.is_dir(),
                }));
            }
            entries.sort_unstable_by(|a, b| a.cmp(b));
            index.children.insert(dir, entries);
            if index.limited {
                break;
            }
        }
        index.files.sort_unstable_by(IndexedFile::cmp);
        Ok(index)
    }

    pub fn visible_tree(
        &self,
        root: &Path,
        expanded: &HashSet<PathBuf>,
    ) -> Vec<(usize, Arc<Entry>)> {
        let children = |path: &Path| self.children.get(path).into_iter().flatten();
        let mut levels = vec![children(root)];
        let mut visible = Vec::new();
        while let Some(level) = levels.last_mut() {
            if let Some(entry) = level.next() {
                visible.push((levels.len() - 1, Arc::clone(entry)));
                if entry.directory && expanded.contains(&entry.path) {
                    levels.push(children(&entry.path));
                }
            } else {
                levels.pop();
            }
        }
        visible
    }

    fn add_saved_file(&mut self, root: &Path, path: &Path) -> bool {
        let Some((parent, name)) = path.parent().zip(path.file_name()) else {
            return false;
        };
        // A parent may have been created since the last scan. Populate only
        // the ancestors on this file's path and keep the same exclusions.
        let Ok(relative) = parent.strip_prefix(root) else {
            return false;
        };
        if relative
            .components()
            .any(|part| ignored_directory(part.as_os_str()))
        {
            return false;
        }
        let mut directory = root.to_path_buf();
        for part in relative.components() {
            let child = directory.join(part);
            if !self.children.contains_key(&child) {
                let Some(siblings) = self.children.get_mut(&directory) else {
                    return false;
                };
                let entry = Entry {
                    path: child.clone(),
                    name: part.as_os_str().to_string_lossy().into_owned(),
                    directory: true,
                };
                if let Err(slot) = siblings.binary_search_by(|item| item.cmp(&entry)) {
                    siblings.insert(slot, Arc::new(entry));
                }
                self.children.insert(child.clone(), Vec::new());
            }
            directory = child;
        }
        let Some(entries) = self.children.get_mut(parent) else {
            return false;
        };
        let file = IndexedFile::new(root, path);
        let Err(file_slot) = self.files.binary_search_by(|item| item.cmp(&file)) else {
            return false;
        };
        let entry = Entry {
            path: path.to_path_buf(),
            name: name.to_string_lossy().into_owned(),
            directory: false,
        };
        let Err(entry_slot) = entries.binary_search_by(|item| item.cmp(&entry)) else {
            return false;
        };
        entries.insert(entry_slot, Arc::new(entry));
        self.files.insert(file_slot, file);
        true
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
    saved_during_scan: HashSet<PathBuf>,
}

impl Project {
    pub fn new(root: PathBuf) -> Self {
        let mut project = Self {
            root,
            index: None,
            job: None,
            saved_during_scan: HashSet::new(),
        };
        project.refresh();
        project
    }

    pub fn refresh(&mut self) {
        self.saved_during_scan.clear();
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

    pub fn add_saved_file(&mut self, path: &Path) -> bool {
        if !path.starts_with(&self.root) {
            return false;
        }
        if self.loading() {
            self.saved_during_scan.insert(path.to_path_buf());
        }
        self.index
            .as_mut()
            .is_some_and(|index| index.add_saved_file(&self.root, path))
    }

    pub fn poll(&mut self) -> Option<io::Result<()>> {
        let result = match self.job.as_ref()?.0.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err(io::Error::other("project scan stopped")),
        };
        self.job = None;
        let saved = std::mem::take(&mut self.saved_during_scan);
        Some(result.map(|mut index| {
            for path in saved {
                index.add_saved_file(&self.root, &path);
            }
            self.index = Some(index);
        }))
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
    #[test]
    fn a_stale_scan_cannot_drop_a_file_saved_during_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (done, reply) = mpsc::channel();
        let (keep, cancel) = mpsc::channel();
        let mut project = Project {
            root: root.clone(),
            index: Some(Index::scan(&root, &cancel, ENTRY_LIMIT).unwrap()),
            job: Some((reply, keep)),
            saved_during_scan: HashSet::new(),
        };
        let stale = Index::scan(&root, &cancel, ENTRY_LIMIT).unwrap();
        let path = root.join("new/nested/file.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();
        assert!(project.add_saved_file(&path));
        assert!(!project.add_saved_file(&path));
        done.send(Ok(stale)).unwrap();
        project.poll().unwrap().unwrap();
        let index = project.index.unwrap();
        assert_eq!(
            index.find("file.rs", 10),
            [(path.clone(), "new/nested/file.rs".into())]
        );
        let expanded = HashSet::from([root.join("new"), root.join("new/nested")]);
        let tree = index.visible_tree(&root, &expanded);
        assert_eq!(tree.len(), 3);
        assert_eq!((tree[2].0, &tree[2].1.path), (2, &path));
    }

    pub(crate) fn synthetic_project(file_count: usize) -> Project {
        let root: PathBuf = "/benchmark".into();
        let directory = root.join("src");
        let folder = Arc::new(Entry {
            path: directory.clone(),
            name: "src".into(),
            directory: true,
        });
        let files = (0..file_count)
            .map(|i| {
                let name = format!("file-{i:05}.rs");
                Arc::new(Entry {
                    path: directory.join(&name),
                    name,
                    directory: false,
                })
            })
            .collect();
        Project {
            root: root.clone(),
            index: Some(Index {
                children: HashMap::from([(root, vec![folder]), (directory, files)]),
                files: vec![],
                limited: false,
            }),
            job: None,
            saved_during_scan: HashSet::new(),
        }
    }
}
