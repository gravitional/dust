use crate::node::Node;
use std::path::{Path, PathBuf};

#[cfg(windows)]
pub fn build(path: &Path) -> Result<PathBuf, String> {
    let scope = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    let database = dust_indexer::default_database(&scope).map_err(|e| e.to_string())?;
    dust_indexer::build(path, &database).map_err(|e| e.to_string())?;
    Ok(database)
}

#[cfg(not(windows))]
pub fn build(_path: &Path) -> Result<PathBuf, String> {
    Err("NTFS indexing is only available on Windows".into())
}

#[cfg(windows)]
pub fn load(path: &Path, apparent: bool, depth: usize, local: bool) -> Result<Vec<Node>, String> {
    let tree = dust_indexer::ipc::query_service(path, apparent, depth)
        .or_else(|error| {
            if local {
                dust_indexer::query(path, apparent, depth)
            } else {
                Err(error)
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(vec![convert(tree)])
}

#[cfg(not(windows))]
pub fn load(
    _path: &Path,
    _apparent: bool,
    _depth: usize,
    _local: bool,
) -> Result<Vec<Node>, String> {
    Err("NTFS indexing is only available on Windows".into())
}

#[cfg(windows)]
fn convert(tree: dust_indexer::Tree) -> Node {
    Node {
        name: tree.path,
        size: tree.size,
        children: tree.children.into_iter().map(convert).collect(),
        inode_device: None,
        depth: tree.depth,
    }
}
