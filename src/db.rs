use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::domain::{Category, Photo, Video};

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS videos(
  id TEXT PRIMARY KEY,
  url TEXT,
  url_norm TEXT UNIQUE,
  rel_path TEXT UNIQUE,
  abs_cache TEXT,
  size INTEGER,
  mtime INTEGER,
  fingerprint TEXT,
  title TEXT NOT NULL,
  channel TEXT,
  duration_s INTEGER,
  status TEXT NOT NULL CHECK(status IN ('link','ok','missing','downloading','error')),
  thumb_key TEXT,
  added_at TEXT NOT NULL,
  last_seen_at TEXT
);

CREATE TABLE IF NOT EXISTS categories(
  id TEXT PRIMARY KEY,
  parent_id TEXT REFERENCES categories(id) ON DELETE SET NULL,
  name TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_cat_sibling
  ON categories(COALESCE(parent_id, ''), name COLLATE NOCASE);

CREATE TABLE IF NOT EXISTS video_categories(
  video_id TEXT NOT NULL REFERENCES videos(id) ON DELETE CASCADE,
  category_id TEXT NOT NULL REFERENCES categories(id) ON DELETE CASCADE,
  PRIMARY KEY(video_id, category_id)
);

CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
CREATE INDEX IF NOT EXISTS ix_videos_status ON videos(status);
CREATE INDEX IF NOT EXISTS ix_videos_fp ON videos(fingerprint);
CREATE INDEX IF NOT EXISTS ix_vc_cat ON video_categories(category_id);

CREATE TABLE IF NOT EXISTS photos(
  id TEXT PRIMARY KEY,
  rel_path TEXT UNIQUE,
  abs_cache TEXT,
  size INTEGER,
  mtime INTEGER,
  fingerprint TEXT,
  title TEXT NOT NULL,
  width INTEGER,
  height INTEGER,
  status TEXT NOT NULL CHECK(status IN ('ok','missing')),
  thumb_key TEXT,
  added_at TEXT NOT NULL,
  last_seen_at TEXT
);

CREATE INDEX IF NOT EXISTS ix_photos_status ON photos(status);
CREATE INDEX IF NOT EXISTS ix_photos_fp ON photos(fingerprint);

CREATE TABLE IF NOT EXISTS photo_categories(
  id TEXT PRIMARY KEY,
  parent_id TEXT REFERENCES photo_categories(id) ON DELETE SET NULL,
  name TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_photo_cat_sibling
  ON photo_categories(COALESCE(parent_id, ''), name COLLATE NOCASE);

CREATE TABLE IF NOT EXISTS photo_category_links(
  photo_id TEXT NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
  category_id TEXT NOT NULL REFERENCES photo_categories(id) ON DELETE CASCADE,
  PRIMARY KEY(photo_id, category_id)
);

CREATE INDEX IF NOT EXISTS ix_pcl_cat ON photo_category_links(category_id);
";

pub fn open_connection(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_default()
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: open_connection(path)?,
        })
    }

    pub fn list_videos(&self) -> Result<Vec<Video>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, rel_path, abs_cache, status, url, channel, duration_s, thumb_key
             FROM videos
             ORDER BY title COLLATE NOCASE",
        )?;

        let rows = stmt.query_map([], |row| {
            let abs_cache: Option<String> = row.get(3)?;
            Ok(Video {
                id: row.get(0)?,
                title: row.get(1)?,
                rel_path: row.get(2)?,
                abs_cache: abs_cache.map(PathBuf::from),
                status: row.get(4)?,
                url: row.get(5)?,
                channel: row.get(6)?,
                duration_s: row.get(7)?,
                thumb_key: row.get(8)?,
            })
        })?;

        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_categories(&self) -> Result<Vec<Category>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, parent_id, name FROM categories")?;
        let rows = stmt.query_map([], |row| {
            Ok(Category {
                id: row.get(0)?,
                parent_id: row.get(1)?,
                name: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_assignments(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT video_id, category_id FROM video_categories")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn create_category(&mut self, parent_id: Option<&str>, name: &str) -> Result<String> {
        let name = validate_category_name(name)?;
        let duplicate: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM categories
                 WHERE IFNULL(parent_id, '') = ?1 AND name = ?2 COLLATE NOCASE",
                params![parent_id.unwrap_or(""), name],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            bail!("A category with that name already exists at this level");
        }

        let id = Uuid::now_v7().to_string();
        self.conn.execute(
            "INSERT INTO categories(id, parent_id, name, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, parent_id, name, timestamp()],
        )?;
        Ok(id)
    }

    pub fn rename_category(&mut self, id: &str, name: &str) -> Result<()> {
        let name = validate_category_name(name)?;
        let parent: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT parent_id FROM categories WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(parent) = parent else {
            bail!("The category no longer exists");
        };

        let duplicate: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM categories
                 WHERE IFNULL(parent_id, '') = ?1
                   AND name = ?2 COLLATE NOCASE
                   AND id <> ?3",
                params![parent.as_deref().unwrap_or(""), name, id],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            bail!("A category with that name already exists at this level");
        }

        let changed = self.conn.execute(
            "UPDATE categories SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        if changed == 0 {
            bail!("The category no longer exists");
        }
        Ok(())
    }

    pub fn delete_category_subtree(&mut self, id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let exists: Option<String> = tx
            .query_row(
                "SELECT id FROM categories WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            bail!("The category no longer exists");
        }

        tx.execute(
            "WITH RECURSIVE subtree(id) AS (
               SELECT id FROM categories WHERE id = ?1
               UNION ALL
               SELECT c.id FROM categories c JOIN subtree s ON c.parent_id = s.id
             )
             DELETE FROM video_categories
             WHERE category_id IN (SELECT id FROM subtree)",
            params![id],
        )?;
        tx.execute(
            "WITH RECURSIVE subtree(id) AS (
               SELECT id FROM categories WHERE id = ?1
               UNION ALL
               SELECT c.id FROM categories c JOIN subtree s ON c.parent_id = s.id
             )
             DELETE FROM categories
             WHERE id IN (SELECT id FROM subtree)",
            params![id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_thumb_key(&self, video_id: &str, thumb_key: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE videos SET thumb_key = ?1 WHERE id = ?2",
            params![thumb_key, video_id],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn assign_category_path(&mut self, video_id: &str, path: &str) -> Result<()> {
        let now = timestamp();
        let tx = self.conn.transaction()?;
        let mut parent: Option<String> = None;

        for segment in path
            .split('/')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
        {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM categories
                     WHERE IFNULL(parent_id, '') = ?1 AND name = ?2 COLLATE NOCASE",
                    params![parent.as_deref().unwrap_or(""), segment],
                    |row| row.get(0),
                )
                .optional()?;

            let category_id = match existing {
                Some(id) => id,
                None => {
                    let id = Uuid::now_v7().to_string();
                    tx.execute(
                        "INSERT INTO categories(id, parent_id, name, created_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![id, parent, segment, now],
                    )?;
                    id
                }
            };

            parent = Some(category_id);
        }

        if let Some(category_id) = parent {
            tx.execute(
                "INSERT OR IGNORE INTO video_categories(video_id, category_id)
                 VALUES (?1, ?2)",
                params![video_id, category_id],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn add_video_category(&self, video_id: &str, category_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO video_categories(video_id, category_id)
             VALUES (?1, ?2)",
            params![video_id, category_id],
        )?;
        Ok(())
    }

    pub fn remove_video_category(&self, video_id: &str, category_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM video_categories WHERE video_id = ?1 AND category_id = ?2",
            params![video_id, category_id],
        )?;
        Ok(())
    }

    pub fn clear_url(&self, video_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE videos SET url = NULL, url_norm = NULL WHERE id = ?1",
            params![video_id],
        )?;
        Ok(())
    }

    pub fn delete_video(&self, video_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM videos WHERE id = ?1", params![video_id])?;
        Ok(())
    }

    pub fn all_meta(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM meta")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<HashMap<String, String>>>()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn list_photos(&self) -> Result<Vec<Photo>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, rel_path, abs_cache, status, width, height, thumb_key
             FROM photos
             ORDER BY title COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| {
            let abs_cache: Option<String> = row.get(3)?;
            let width: Option<i64> = row.get(5)?;
            let height: Option<i64> = row.get(6)?;
            Ok(Photo {
                id: row.get(0)?,
                title: row.get(1)?,
                rel_path: row.get(2)?,
                abs_cache: abs_cache.map(PathBuf::from),
                status: row.get(4)?,
                width: width.and_then(|value| u32::try_from(value).ok()),
                height: height.and_then(|value| u32::try_from(value).ok()),
                thumb_key: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_photo(&self, photo_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM photos WHERE id = ?1", params![photo_id])?;
        Ok(())
    }

    pub fn set_photo_thumb_key(&self, photo_id: &str, thumb_key: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET thumb_key = ?1 WHERE id = ?2",
            params![thumb_key, photo_id],
        )?;
        Ok(())
    }

    pub fn list_photo_categories(&self) -> Result<Vec<Category>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, parent_id, name FROM photo_categories")?;
        let rows = stmt.query_map([], |row| {
            Ok(Category {
                id: row.get(0)?,
                parent_id: row.get(1)?,
                name: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_photo_assignments(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT photo_id, category_id FROM photo_category_links")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn create_photo_category(&mut self, parent_id: Option<&str>, name: &str) -> Result<String> {
        let name = validate_category_name(name)?;
        let duplicate: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM photo_categories
                 WHERE IFNULL(parent_id, '') = ?1 AND name = ?2 COLLATE NOCASE",
                params![parent_id.unwrap_or(""), name],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            bail!("A category with that name already exists at this level");
        }

        let id = Uuid::now_v7().to_string();
        self.conn.execute(
            "INSERT INTO photo_categories(id, parent_id, name, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, parent_id, name, timestamp()],
        )?;
        Ok(id)
    }

    pub fn rename_photo_category(&mut self, id: &str, name: &str) -> Result<()> {
        let name = validate_category_name(name)?;
        let parent: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT parent_id FROM photo_categories WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(parent) = parent else {
            bail!("The category no longer exists");
        };

        let duplicate: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM photo_categories
                 WHERE IFNULL(parent_id, '') = ?1
                   AND name = ?2 COLLATE NOCASE
                   AND id <> ?3",
                params![parent.as_deref().unwrap_or(""), name, id],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            bail!("A category with that name already exists at this level");
        }

        let changed = self.conn.execute(
            "UPDATE photo_categories SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        if changed == 0 {
            bail!("The category no longer exists");
        }
        Ok(())
    }

    pub fn delete_photo_category_subtree(&mut self, id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let exists: Option<String> = tx
            .query_row(
                "SELECT id FROM photo_categories WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            bail!("The category no longer exists");
        }

        tx.execute(
            "WITH RECURSIVE subtree(id) AS (
               SELECT id FROM photo_categories WHERE id = ?1
               UNION ALL
               SELECT c.id FROM photo_categories c JOIN subtree s ON c.parent_id = s.id
             )
             DELETE FROM photo_category_links
             WHERE category_id IN (SELECT id FROM subtree)",
            params![id],
        )?;
        tx.execute(
            "WITH RECURSIVE subtree(id) AS (
               SELECT id FROM photo_categories WHERE id = ?1
               UNION ALL
               SELECT c.id FROM photo_categories c JOIN subtree s ON c.parent_id = s.id
             )
             DELETE FROM photo_categories
             WHERE id IN (SELECT id FROM subtree)",
            params![id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn add_photo_category(&self, photo_id: &str, category_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO photo_category_links(photo_id, category_id)
             VALUES (?1, ?2)",
            params![photo_id, category_id],
        )?;
        Ok(())
    }

    pub fn remove_photo_category(&self, photo_id: &str, category_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM photo_category_links WHERE photo_id = ?1 AND category_id = ?2",
            params![photo_id, category_id],
        )?;
        Ok(())
    }
}

fn validate_category_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("Category name cannot be empty");
    }
    if name.contains('/') {
        bail!("Category name cannot contain «/»");
    }
    Ok(name.to_string())
}

pub fn url_exists(conn: &Connection, url_norm: &str) -> Result<bool> {
    let found: Option<String> = conn
        .query_row(
            "SELECT id FROM videos WHERE url_norm = ?1",
            params![url_norm],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

#[allow(clippy::too_many_arguments)]
pub fn insert_url_video(
    conn: &Connection,
    id: &str,
    url: &str,
    url_norm: &str,
    title: &str,
    channel: Option<&str>,
    duration_s: Option<i64>,
    thumb_key: Option<&str>,
) -> Result<bool> {
    let now = timestamp();
    conn.execute(
        "INSERT OR IGNORE INTO videos
           (id, url, url_norm, title, channel, duration_s, status, thumb_key, added_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'link', ?7, ?8, ?8)",
        params![id, url, url_norm, title, channel, duration_s, thumb_key, now],
    )?;
    Ok(conn.changes() == 1)
}

pub fn set_downloaded(
    conn: &Connection,
    id: &str,
    rel_path: &str,
    abs_path: &str,
    size: i64,
    mtime: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE videos
         SET rel_path = ?1, abs_cache = ?2, size = ?3, mtime = ?4,
             status = 'ok', last_seen_at = ?5
         WHERE id = ?6",
        params![rel_path, abs_path, size, mtime, timestamp(), id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Db;
    use uuid::Uuid;

    #[test]
    fn assigns_and_removes_hierarchical_categories() {
        let base = std::env::temp_dir().join(format!("video-player-db-{}", Uuid::now_v7()));
        let path = base.join("library.db");
        let mut db = Db::open(&path).unwrap();

        db.conn
            .execute(
                "INSERT INTO videos(id, title, status, added_at)
                 VALUES ('v1', 'Video', 'link', '0')",
                [],
            )
            .unwrap();

        db.assign_category_path("v1", "Pelis/Accion").unwrap();
        db.assign_category_path("v1", "Pelis/Drama").unwrap();
        db.assign_category_path("v1", "pelis/accion").unwrap();

        assert_eq!(db.list_categories().unwrap().len(), 3);
        let assignments = db.list_assignments().unwrap();
        assert_eq!(assignments.len(), 2);

        let leaf = assignments
            .iter()
            .find_map(|(_, category_id)| {
                db.list_categories()
                    .unwrap()
                    .into_iter()
                    .find(|category| &category.id == category_id && category.name == "Accion")
            })
            .expect("no se encontró la categoría Accion");

        db.remove_video_category("v1", &leaf.id).unwrap();
        assert_eq!(db.list_assignments().unwrap().len(), 1);

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn adds_video_category_without_creating_it() {
        let base = std::env::temp_dir().join(format!("video-player-db-{}", Uuid::now_v7()));
        let path = base.join("library.db");
        let mut db = Db::open(&path).unwrap();

        db.conn
            .execute(
                "INSERT INTO videos(id, title, status, added_at)
                 VALUES ('v1', 'Video', 'link', '0')",
                [],
            )
            .unwrap();

        let category_id = db.create_category(None, "Pelis").unwrap();

        assert!(db.add_video_category("v1", "inexistente").is_err());
        db.add_video_category("v1", &category_id).unwrap();
        db.add_video_category("v1", &category_id).unwrap();
        assert_eq!(
            db.list_assignments().unwrap(),
            vec![("v1".to_string(), category_id.clone())]
        );

        db.remove_video_category("v1", &category_id).unwrap();
        assert!(db.list_assignments().unwrap().is_empty());
        assert_eq!(db.list_categories().unwrap().len(), 1);

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn creates_renames_and_deletes_category_trees() {
        let base = std::env::temp_dir().join(format!("video-player-db-{}", Uuid::now_v7()));
        let path = base.join("library.db");
        let mut db = Db::open(&path).unwrap();

        db.conn
            .execute(
                "INSERT INTO videos(id, title, status, added_at)
                 VALUES ('v1', 'Uno', 'link', '0'), ('v2', 'Dos', 'link', '0')",
                [],
            )
            .unwrap();

        db.assign_category_path("v1", "Pelis/Accion/Rapida")
            .unwrap();
        db.assign_category_path("v2", "Pelis/Drama").unwrap();

        let categories = db.list_categories().unwrap();
        let pelis = categories
            .iter()
            .find(|category| category.name == "Pelis")
            .unwrap();
        let accion = categories
            .iter()
            .find(|category| category.name == "Accion")
            .unwrap();
        let drama = categories
            .iter()
            .find(|category| category.name == "Drama")
            .unwrap();

        assert!(db.create_category(Some(&pelis.id), "accion").is_err());
        db.create_category(Some(&accion.id), "Lenta").unwrap();
        db.rename_category(&drama.id, "Drama2").unwrap();

        let categories = db.list_categories().unwrap();
        assert!(categories.iter().any(|category| category.name == "Lenta"));
        assert!(categories.iter().any(|category| category.name == "Drama2"));

        db.delete_category_subtree(&accion.id).unwrap();

        let categories = db.list_categories().unwrap();
        let names: Vec<&str> = categories
            .iter()
            .map(|category| category.name.as_str())
            .collect();
        assert!(names.contains(&"Pelis"));
        assert!(names.contains(&"Drama2"));
        assert!(!names.contains(&"Accion"));
        assert!(!names.contains(&"Rapida"));
        assert!(!names.contains(&"Lenta"));

        let assignments = db.list_assignments().unwrap();
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].0, "v2");

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn manages_photo_categories() {
        let base = std::env::temp_dir().join(format!("video-player-db-{}", Uuid::now_v7()));
        let path = base.join("library.db");
        let mut db = Db::open(&path).unwrap();

        db.conn
            .execute(
                "INSERT INTO photos(id, title, status, added_at)
                 VALUES ('p1', 'Foto', 'ok', '0'), ('p2', 'Otra', 'ok', '0')",
                [],
            )
            .unwrap();

        let viajes = db.create_photo_category(None, "Viajes").unwrap();
        let playa = db.create_photo_category(Some(&viajes), "Playa").unwrap();
        assert!(db.create_photo_category(Some(&viajes), "playa").is_err());

        db.add_photo_category("p1", &playa).unwrap();
        db.add_photo_category("p2", &playa).unwrap();
        assert_eq!(db.list_photo_assignments().unwrap().len(), 2);

        db.remove_photo_category("p1", &playa).unwrap();
        assert_eq!(db.list_photo_assignments().unwrap().len(), 1);

        db.rename_photo_category(&playa, "Mar").unwrap();
        assert!(
            db.list_photo_categories()
                .unwrap()
                .iter()
                .any(|category| category.name == "Mar")
        );

        db.delete_photo_category_subtree(&viajes).unwrap();
        assert!(db.list_photo_categories().unwrap().is_empty());
        assert!(db.list_photo_assignments().unwrap().is_empty());
        assert_eq!(db.list_photos().unwrap().len(), 2);

        std::fs::remove_dir_all(&base).unwrap();
    }
}
