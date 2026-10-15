//! Persistent file-ID index shared by the CLI and Windows service.
#[cfg(windows)]
pub mod ipc;
#[cfg(windows)]
pub mod journal;
#[cfg(windows)]
pub mod scanner;
#[cfg(windows)]
pub mod service;
pub mod store;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub parent: Option<i64>,
    pub file_id: u64,
    pub parent_file_id: u64,
    pub path: String,
    pub is_dir: bool,
    pub apparent: u64,
    pub allocated: u64,
    pub attributes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tree {
    pub path: PathBuf,
    pub size: u64,
    pub children: Vec<Tree>,
    pub depth: usize,
}

pub fn path_key(path: &Path) -> Result<String> {
    let s = path.to_str().ok_or("index requires a valid Unicode path")?;
    // Do not fold case: NTFS directories can opt in to case-sensitive names.
    Ok(s.strip_prefix(r"\\?\")
        .unwrap_or(s)
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_owned())
}

pub fn default_database(scope: &Path) -> Result<PathBuf> {
    let key = path_key(scope)?;
    // Stable FNV-1a, independent of Rust's randomized HashMap implementation.
    let hash = key.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Ok(base.join("dust").join(format!("index-{hash:016x}.sqlite")))
}

pub fn query(scope: &Path, apparent: bool, depth: usize) -> Result<Tree> {
    #[cfg(windows)]
    {
        let scope = std::fs::canonicalize(scope)?;
        let scope_key = path_key(&scope)?;
        for candidate in scope.ancestors() {
            let db = default_database(candidate)?;
            if !db.is_file() {
                continue;
            }
            let store = store::Store::open_existing(&db)?;
            let indexed_scope = store.scope()?;
            if scope_key == indexed_scope
                || scope_key
                    .strip_prefix(&indexed_scope)
                    .is_some_and(|rest| rest.starts_with('\\'))
            {
                return store.query(&scope, apparent, depth);
            }
        }
        Err("no SQLite index covers this path; use --build-index first".into())
    }
    #[cfg(not(windows))]
    {
        let _ = (scope, apparent, depth);
        Err("NTFS indexing is only available on Windows".into())
    }
}

#[cfg(not(windows))]
pub fn build(_scope: &Path, _database: &Path) -> Result<()> {
    Err("NTFS indexing is only available on Windows".into())
}

#[cfg(windows)]
pub fn build(scope: &Path, database: &Path) -> Result<()> {
    let scope = std::fs::canonicalize(scope)?;
    let mut store = store::Store::open(database)?;
    let journal = journal::Journal::open(&scope).ok();
    let before = journal.as_ref().map(|j| j.state()).transpose()?;
    let entries = scanner::scan(&scope, None, 1)?;
    store.replace(&path_key(&scope)?, &entries, before.as_ref())?;
    if let Some(journal) = journal {
        journal.synchronize(&mut store)?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn refresh(database: &Path) -> Result<()> {
    let mut store = store::Store::open_existing(database)?;
    let scope = store.scope()?;
    if store.cursor()?.is_none() {
        drop(store);
        return build(Path::new(&scope), database);
    }
    let journal = journal::Journal::open(Path::new(&scope))?;
    journal.synchronize(&mut store)
}
