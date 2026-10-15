use crate::{Entry, Result, Tree};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    pub volume_serial: u64,
    pub journal_id: u64,
    pub next_usn: i64,
}

pub struct Store {
    pub(crate) db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::connection(Connection::open(path)?)
    }

    pub fn open_existing(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Err("no SQLite index; use --build-index first".into());
        }
        Self::connection(Connection::open(path)?)
    }

    fn connection(db: Connection) -> Result<Self> {
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS nodes(
              id INTEGER PRIMARY KEY, parent INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
              file_id BLOB NOT NULL CHECK(length(file_id)=8),
              parent_file_id BLOB NOT NULL CHECK(length(parent_file_id)=8),
              path TEXT NOT NULL UNIQUE, is_dir INTEGER NOT NULL,
              apparent INTEGER NOT NULL, allocated INTEGER NOT NULL, attributes INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS nodes_parent ON nodes(parent);
            CREATE INDEX IF NOT EXISTS nodes_file_id ON nodes(file_id);
            CREATE INDEX IF NOT EXISTS nodes_parent_file_id ON nodes(parent_file_id);
            CREATE TABLE IF NOT EXISTS directory_totals(
              node_id INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
              apparent INTEGER NOT NULL, allocated INTEGER NOT NULL, file_count INTEGER NOT NULL);",
        )?;
        Ok(Self { db })
    }

    pub fn scope(&self) -> Result<String> {
        self.meta("scope")?
            .ok_or_else(|| "index has no scope".into())
    }
    pub fn cursor(&self) -> Result<Option<Cursor>> {
        self.meta("cursor")?
            .map(|v| serde_json::from_str(&v).map_err(Into::into))
            .transpose()
    }
    fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT value FROM metadata WHERE key=?", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn replace(
        &mut self,
        scope: &str,
        entries: &[Entry],
        cursor: Option<&Cursor>,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM nodes", [])?;
        insert_entries(&tx, entries)?;
        tx.execute("INSERT OR REPLACE INTO metadata VALUES('scope',?)", [scope])?;
        tx.execute("INSERT OR REPLACE INTO metadata VALUES('schema','2')", [])?;
        write_cursor(&tx, cursor)?;
        recompute_totals(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn entries(&self) -> Result<Vec<Entry>> {
        let mut s = self.db.prepare("SELECT id,parent,file_id,parent_file_id,path,is_dir,apparent,allocated,attributes FROM nodes ORDER BY id")?;
        Ok(s.query_map([], row_entry)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[cfg(windows)]
    pub fn check_access(
        &self,
        path: &Path,
        mut check: impl FnMut(&Path, bool) -> Result<()>,
    ) -> Result<()> {
        let key = crate::path_key(&std::fs::canonicalize(path)?)?;
        let root: i64 = self
            .db
            .query_row("SELECT id FROM nodes WHERE path=?", [&key], |r| r.get(0))?;
        let mut stmt=self.db.prepare("WITH RECURSIVE t(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN t ON n.parent=t.id) SELECT n.path,n.is_dir FROM t JOIN nodes n ON n.id=t.id")?;
        let rows = stmt.query_map([root], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
        })?;
        for row in rows {
            let (p, is_dir) = row?;
            check(Path::new(&p), is_dir)?;
        }
        drop(stmt);
        Ok(())
    }

    pub fn next_id(&self) -> Result<i64> {
        Ok(self
            .db
            .query_row("SELECT coalesce(max(id),0)+1 FROM nodes", [], |r| r.get(0))?)
    }

    /// Atomically replace only changed directory trees and advance the journal
    /// cursor in the same transaction. A crash never commits the cursor alone.
    pub fn apply(&mut self, roots: &[i64], replacements: &[Entry], cursor: &Cursor) -> Result<()> {
        let tx = self.db.transaction()?;
        for root in roots {
            tx.execute("DELETE FROM nodes WHERE id=?", [root])?;
        }
        insert_entries(&tx, replacements)?;
        recompute_totals(&tx)?;
        write_cursor(&tx, Some(cursor))?;
        tx.commit()?;
        Ok(())
    }

    pub fn advance(&mut self, cursor: &Cursor) -> Result<()> {
        let tx = self.db.transaction()?;
        write_cursor(&tx, Some(cursor))?;
        tx.commit()?;
        Ok(())
    }

    pub fn query(&self, path: &Path, apparent: bool, max_depth: usize) -> Result<Tree> {
        let path = crate::path_key(&std::fs::canonicalize(path)?)?;
        self.query_key(&path, apparent, max_depth)
    }

    pub fn query_key(&self, path: &str, apparent: bool, max_depth: usize) -> Result<Tree> {
        let root: i64 = self
            .db
            .query_row("SELECT id FROM nodes WHERE path=?", [path], |r| r.get(0))?;
        let root_apparent: u64 = self.db.query_row(
            "SELECT apparent FROM directory_totals WHERE node_id=?",
            [root],
            |r| r.get(0),
        )?;
        let root_allocated: u64 = self.db.query_row(
            "SELECT allocated FROM directory_totals WHERE node_id=?",
            [root],
            |r| r.get(0),
        )?;
        let mut s = self.db.prepare("WITH RECURSIVE tree(id,depth) AS (
            SELECT ?1,0 UNION ALL SELECT n.id,t.depth+1 FROM nodes n JOIN tree t ON n.parent=t.id WHERE t.depth<?2)
            SELECT n.id,n.parent,n.path,t.depth,d.apparent,d.allocated FROM tree t
            JOIN nodes n ON n.id=t.id JOIN directory_totals d ON d.node_id=n.id ORDER BY t.depth DESC,n.path")?;
        let rows = s.query_map(
            params![root, max_depth.min(i64::MAX as usize) as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, usize>(3)?,
                    r.get::<_, u64>(4)?,
                    r.get::<_, u64>(5)?,
                ))
            },
        )?;
        let mut children = HashMap::<i64, Vec<Tree>>::new();
        let mut result = None;
        for row in rows {
            let (id, parent, path, depth, a, b) = row?;
            let node = Tree {
                path: PathBuf::from(path),
                size: if id == root {
                    if apparent {
                        root_apparent
                    } else {
                        root_allocated
                    }
                } else if apparent {
                    a
                } else {
                    b
                },
                children: children.remove(&id).unwrap_or_default(),
                depth,
            };
            if id == root {
                result = Some(node);
            } else if let Some(parent) = parent {
                children.entry(parent).or_default().push(node);
            }
        }
        result.ok_or_else(|| "query root not found".into())
    }
}

fn row_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let fid: Vec<u8> = r.get(2)?;
    let pfid: Vec<u8> = r.get(3)?;
    Ok(Entry {
        id: r.get(0)?,
        parent: r.get(1)?,
        file_id: u64::from_le_bytes(fid.try_into().unwrap()),
        parent_file_id: u64::from_le_bytes(pfid.try_into().unwrap()),
        path: r.get(4)?,
        is_dir: r.get(5)?,
        apparent: r.get(6)?,
        allocated: r.get(7)?,
        attributes: r.get(8)?,
    })
}
fn insert_entries(tx: &rusqlite::Transaction<'_>, entries: &[Entry]) -> Result<()> {
    let mut s = tx.prepare("INSERT INTO nodes VALUES(?,?,?,?,?,?,?,?,?)")?;
    for e in entries {
        s.execute(params![
            e.id,
            e.parent,
            e.file_id.to_le_bytes().as_slice(),
            e.parent_file_id.to_le_bytes().as_slice(),
            e.path,
            e.is_dir,
            e.apparent,
            e.allocated,
            e.attributes
        ])?;
    }
    Ok(())
}
fn write_cursor(tx: &rusqlite::Transaction<'_>, cursor: Option<&Cursor>) -> Result<()> {
    if let Some(c) = cursor {
        tx.execute(
            "INSERT OR REPLACE INTO metadata VALUES('cursor',?)",
            [serde_json::to_string(c)?],
        )?;
    } else {
        tx.execute("DELETE FROM metadata WHERE key='cursor'", [])?;
    }
    Ok(())
}
fn recompute_totals(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    // Parent IDs are not necessarily smaller after an incremental replacement.
    let mut s=tx.prepare("WITH RECURSIVE tree(id,depth) AS (
        SELECT id,0 FROM nodes WHERE parent IS NULL UNION ALL
        SELECT n.id,t.depth+1 FROM nodes n JOIN tree t ON n.parent=t.id)
        SELECT n.id,n.parent,n.file_id,n.apparent,n.allocated,n.is_dir,t.depth FROM tree t JOIN nodes n ON n.id=t.id ORDER BY t.depth DESC,n.path")?;
    let rows = s
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, u64>(3)?,
                r.get::<_, u64>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, usize>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut totals = HashMap::<i64, (u64, u64, u64)>::new();
    let mut seen_files = HashSet::new();
    tx.execute("DELETE FROM directory_totals", [])?;
    let mut insert = tx.prepare("INSERT INTO directory_totals VALUES(?,?,?,?)")?;
    for (id, parent, file_id, a, b, is_dir, depth) in rows {
        let file_id = u64::from_le_bytes(file_id.try_into().unwrap());
        let allocated = if is_dir || seen_files.insert(file_id) {
            b
        } else {
            0
        };
        let t = totals.entry(id).or_default();
        t.0 += a;
        t.1 += allocated;
        t.2 += u64::from(!is_dir);
        let t = *t;
        insert.execute(params![id, t.0, t.1, t.2])?;
        if depth == 0 {
            continue;
        }
        if let Some(p) = parent {
            let p = totals.entry(p).or_default();
            p.0 += t.0;
            p.1 += t.1;
            p.2 += t.2;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: i64, parent: Option<i64>, path: &str, size: u64) -> Entry {
        Entry {
            id,
            parent,
            file_id: id as u64,
            parent_file_id: parent.unwrap_or(0) as u64,
            path: path.into(),
            is_dir: parent.is_none(),
            apparent: size,
            allocated: size,
            attributes: 0,
        }
    }
    #[test]
    fn transaction_replaces_changed_subtree_and_advances_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Store::open(&tmp.path().join("index.db")).unwrap();
        let c = Cursor {
            volume_serial: 1,
            journal_id: 2,
            next_usn: 3,
        };
        db.replace(
            "root",
            &[
                entry(1, None, "root", 0),
                entry(2, Some(1), "root\\file", 7),
            ],
            Some(&c),
        )
        .unwrap();
        assert_eq!(db.query_key("root", false, 0).unwrap().size, 7);
        let c2 = Cursor { next_usn: 4, ..c };
        db.apply(&[2], &[entry(3, Some(1), "root\\new", 19)], &c2)
            .unwrap();
        let tree = db.query_key("root", false, 1).unwrap();
        assert_eq!(tree.size, 19);
        assert_eq!(tree.children.len(), 1);
        assert_eq!(db.cursor().unwrap(), Some(c2));
        // Failed replacement must retain both the tree and its cursor.
        assert!(
            db.apply(
                &[3],
                &[entry(4, Some(999), "bad", 5)],
                &Cursor { next_usn: 99, ..c }
            )
            .is_err()
        );
        assert_eq!(db.query_key("root", false, 1).unwrap().size, 19);
        assert_eq!(db.cursor().unwrap().unwrap().next_usn, 4);
    }

    #[test]
    fn depth_limited_query_uses_nested_directory_totals() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Store::open(&tmp.path().join("index.db")).unwrap();
        let entries = vec![
            Entry {
                id: 1,
                parent: None,
                file_id: 1,
                parent_file_id: 0,
                path: "root".into(),
                is_dir: true,
                apparent: 0,
                allocated: 0,
                attributes: 0,
            },
            Entry {
                id: 2,
                parent: Some(1),
                file_id: 2,
                parent_file_id: 1,
                path: "root\\nested".into(),
                is_dir: true,
                apparent: 0,
                allocated: 0,
                attributes: 0,
            },
            Entry {
                id: 3,
                parent: Some(2),
                file_id: 3,
                parent_file_id: 2,
                path: "root\\nested\\file".into(),
                is_dir: false,
                apparent: 17,
                allocated: 11,
                attributes: 0,
            },
        ];
        db.replace("root", &entries, None).unwrap();
        let tree = db.query_key("root", true, 1).unwrap();
        assert_eq!(tree.size, 17);
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].size, 17);
        assert!(tree.children[0].children.is_empty());
        let allocated = db.query_key("root", false, 1).unwrap();
        assert_eq!(allocated.size, 11);
        assert_eq!(allocated.children[0].size, 11);
    }

    #[test]
    fn allocated_totals_deduplicate_file_ids_across_subtrees() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Store::open(&tmp.path().join("index.db")).unwrap();
        let mut first = entry(3, Some(1), "root\\a\\file", 10);
        first.file_id = 99;
        first.allocated = 16;
        let mut second = entry(4, Some(2), "root\\b\\hardlink", 10);
        second.file_id = 99;
        second.allocated = 16;
        db.replace(
            "root",
            &[
                entry(1, None, "root", 0),
                entry(2, Some(1), "root\\a", 0),
                entry(5, Some(1), "root\\b", 0),
                first,
                second,
            ],
            None,
        )
        .unwrap();
        assert_eq!(db.query_key("root", true, 1).unwrap().size, 20);
        assert_eq!(db.query_key("root", false, 1).unwrap().size, 16);
    }
}
