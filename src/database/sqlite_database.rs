//! SqliteDatabase is the internal component that handles everything SQL related
//! to the `sqlite.db` database.
use crate::data::tag_file::TagFile;
use crate::database::tag_database::get_database_path;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use log::*;
use rusqlite::Connection;
use std::collections::HashSet;
use std::fs::{self, File, ReadDir};
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

/// Serialises `HashSet<String>` (used for file tags) into JSON,
/// then converts it into bytes (`Vec<u8>`) for `rusqlite` to store it in the database.
fn serialise_tags(hash_set: &HashSet<String>) -> Result<Vec<u8>> {
    let serialised = serde_json::to_string(&hash_set)
        .context("Serialising tags failed: Couldn't serialise HashSet to JSON")?;
    Ok(serialised.into_bytes())
}

/// Deserialises JSON as raw bytes back into a `HashSet<String>`. Used for retrieving
/// tags from the database.
fn deserialise_tags(vec: &Vec<u8>) -> Result<HashSet<String>> {
    let string =
        std::str::from_utf8(vec).context("Deserialising tags failed: Invalid UTF-8 sequence")?;
    let deserialised: HashSet<String> = serde_json::from_str(string)
        .context("Deserialising tags failed: Couldn't deseralise JSON back to HashSet")?;
    Ok(deserialised)
}

/// Object used to handle a file search in the database.
/// It contains unserialised raw data and is handled exclusively
/// in `get_file_from_hash`. Do not use this.
#[derive(Debug)]
pub struct TagFileSqlite {
    pub id: u64,
    pub file_name: String,
    pub file_path_string: String,
    pub file_hash_blob: Vec<u8>,
    pub upload_date: DateTime<Utc>,
    pub tags_blob: Option<Vec<u8>>,
}

/** The database object containing the connection.

It has 2 types of tables:

1) The `_files` table, which contains information about the individually
    uploaded files. A row would have the following information: `file_name`,
    `file_path`, `file_hash`, `upload_date`, and `tags`.
2) One table for each tag. Each row in these tables contain the hashes of
    the files which are linked to this tag. These tables are used for searching.
*/
pub struct SqliteDatabase {
    db: Connection,
}

impl SqliteDatabase {
    /// Opens the connection to the database at a given path. The `name` path is the name
    /// of the parent folder which will contain `sqlite.db` (and the uploaded files).
    pub fn initialise(name: &str, custom_path: Option<PathBuf>) -> Result<SqliteDatabase> {
        info!("SqliteDatabase - initialise_default() - Initialising default database");

        let mut path: PathBuf =
            get_database_path(custom_path).context("Couldn't find database path")?;
        path.push(name);
        path.push("sqlite.db");
        debug!(
            "SqliteDatabase - initialise_default() - Opening connection to database at path {}",
            &path.display()
        );

        let db =
            Connection::open(&path).context("Couldn't open a connection to SQLite database")?;

        debug!("SqliteDatabase - initialise_default() - Creating _files table if not exists");

        db.execute(
            "CREATE TABLE IF NOT EXISTS _files (
                id          INTEGER PRIMARY KEY,
                file_name   TEXT NOT NULL,
                file_path   TEXT UNIQUE,
                file_hash   BLOB NOT NULL UNIQUE,
                upload_date TIMESTAMP NOT NULL,
                tags        BLOB
            )",
            (),
        )
        .context("Couldn't create '_files' table for database")?;

        Ok(SqliteDatabase { db: db })
    }

    /// Adds an entry of the specified TagFile in the `_files` table of the database.
    /// It does not handle the tables for tags: update_tags_to_file() does.
    pub fn add_file(&self, file: &TagFile) -> Result<()> {
        let db: &Connection = &self.db;

        let file_name: &str = (&file.file_name).as_str();

        let file_hash = &file.file_hash;

        let file_tags = &file.get_tags();
        let file_tags_serialised = serialise_tags(&file_tags)?;

        let file_path_str = &file
            .get_path()
            .clone()
            .into_os_string()
            .into_string()
            .unwrap();

        let now = SystemTime::now();
        let now: DateTime<Utc> = now.into();
        let now = now.to_rfc3339();

        debug!("INSERT INTO _files (file_name, file_hash, file_path, upload_date, tags) VALUES ({}, {}, {}, {}, {:?})",
            &file_name,
            crate::data::tag_util::bytes_to_hex(&file_hash),
            &file_path_str,
            &now,
            &file_tags_serialised);

        db.execute(
            "INSERT INTO _files (file_name, file_hash, file_path, upload_date, tags) VALUES (?1, ?2, ?3, ?4, ?5)",
            (&file_name, &file_hash, &file_path_str, &now, &file_tags_serialised),
        )?;
        Ok(())
    }

    /// Removes a specified TagFile from the database. Contrary to `add_file`, this
    /// affects ALL tables, that includes `_files` and every tag table that it is in.
    /// (Note: The tags from the file are retrieved from the database itself, so the
    /// `_files` table and tags table have to be synchronised to have a clean removal.)
    pub fn remove_file(&self, file: &TagFile) -> Result<()> {
        // Where to remove the file from:
        // 1) The _files file index table
        // 2) Each {tag} table

        let db: &Connection = &self.db;

        // It could be equal to &file.tags, but that might not always be true,
        // moreover we're dealing with the actual entries present in the database
        // and not TagFile stuff
        // Therefore it is called before removing 1), because get_tags_from_hash()
        // looks in that database (that we're about to delete)

        let mut tags_from_hash_result: HashSet<String> = HashSet::new();

        match &self.get_tagfile_from_hash(&file.file_hash).ok() {
            Some(tagfile) => {
                tags_from_hash_result = tagfile.get_tags().to_owned();

                // Remove 1)
                db.execute(
                    "DELETE FROM _files WHERE file_hash IS (?)",
                    [&file.file_hash],
                )
                .with_context(|| {
                    format!(
                        "Couldn't remove file with file hash '{:?}' from _files table",
                        &file.file_hash
                    )
                })?;
            }
            None => {}
        }

        for tag in tags_from_hash_result {
            if (&self.get_hashes_from_tag(&tag)?).contains(&file.file_hash) {
                // Remove 2)
                let query = format!("DELETE FROM {tag} WHERE file_hash IS (?)");
                db.execute(query.as_str(), [&file.file_hash])
                    .with_context(|| {
                        format!(
                            "Couldn't remove file with file hash '{:?}' from tag table {tag}",
                            &file.file_hash
                        )
                    })?;
            }
        }

        Ok(())
    }

    /// Internal function for handling file search in the `_files` table.
    fn get_file_from_hash(&self, hash: &Vec<u8>) -> Result<TagFileSqlite> {
        let db: &Connection = &self.db;

        let mut quer = db.prepare(
            "SELECT id, file_name, file_path, upload_date, tags FROM _files WHERE file_hash IS :hash",
        )?;
        let mut search_result = quer.query_map(&[(":hash", hash)], |row| {
            Ok(TagFileSqlite {
                id: row.get(0)?,
                file_name: row.get(1)?,
                file_path_string: row.get(2)?,
                file_hash_blob: hash.clone(),
                upload_date: row.get(3)?,
                tags_blob: row.get(4)?,
            })
        })?;
        match search_result.nth(0) {
            Some(result) => Ok(result?),
            None => bail!("No file found in database with given hash"),
        }
    }

    /// Retrieves the corresponding TagFile from its hash using the information stored in `_files`.
    /// Returns an `Err` if it cannot find anything, which happens when the hash does not correspond
    /// to any file stored in `_files`.
    pub fn get_tagfile_from_hash(&self, hash: &Vec<u8>) -> Result<TagFile> {
        let maybe_tagfilesqlite: Option<TagFileSqlite> = Self::get_file_from_hash(&self, hash).ok();
        match maybe_tagfilesqlite {
            Some(tagfilesqlite) => {
                let tags: HashSet<String> = deserialise_tags(&tagfilesqlite.tags_blob.unwrap())?;
                let path: PathBuf = PathBuf::from(&tagfilesqlite.file_path_string);
                let file_name: String = (&tagfilesqlite.file_name).to_owned();
                let file_hash = hash.clone();

                let tagfile = TagFile {
                    path: path,
                    file_name: file_name,
                    file_hash: file_hash,
                    tags: tags,
                };

                debug!(
                    "Found TagFile from file hash {} (trimmed): {tagfile}",
                    crate::data::tag_util::trimmed_hash_hex(&hash)?
                );
                return Ok(tagfile);
            }
            None => bail!("No tags found for file with hash '{:?}'", hash),
        }
    }

    /// Retrieves every hash contained in a given tag's table. Used for search.
    pub fn get_hashes_from_tag(&self, tag: &str) -> Result<HashSet<Vec<u8>>> {
        let db: &Connection = &self.db;
        let mut quer = db.prepare(format!("SELECT file_hash FROM {tag}").as_str())
            .with_context(|| format!("SQL search for '{tag}' table failed. The '{tag}' table most likely does not exist"))?;
        let hashes = quer
            .query_map((), |row| Ok(row.get(0)?))
            .context("Hash query map failed")?;
        let mut hashes_hashset: HashSet<Vec<u8>> = HashSet::new();
        for hash in hashes {
            hashes_hashset.insert(hash?);
        }
        Ok(hashes_hashset)
    }

    /// Retrieves every file's hash contained in the `_files` table
    pub fn get_all_file_hashes(&self) -> Result<HashSet<Vec<u8>>> {
        let db: &Connection = &self.db;
        let mut quer = db.prepare("SELECT id, file_hash FROM _files")?;
        let hashes = quer.query_map((), |row| Ok(row.get(1)?))?;
        let mut hashes_hashset: HashSet<Vec<u8>> = HashSet::new();
        for hash in hashes {
            hashes_hashset.insert(hash?);
        }
        Ok(hashes_hashset)
    }

    /// This function does two things:
    /// 1) It iterates the file's tags and for each of them adds the file hash
    /// in the corresponding tag table.
    /// 2) It *updates* (does not add) the `_files` entry which also has an entry
    /// for tags in each individual file.
    pub fn update_tags_to_file(&self, file: &TagFile) -> Result<()> {
        let db: &Connection = &self.db;

        for tag in &file.tags {
            info!(
                "SqliteDatabse - update_tags_to_file() - Creating tag table for {} if not exists",
                &tag
            );
            let query = format!(
                "CREATE TABLE IF NOT EXISTS {tag} (
                id          INTEGER PRIMARY KEY,
                file_hash   BLOB NOT NULL UNIQUE
            )"
            );
            db.execute(query.as_str(), ())
                .with_context(|| format!("SQLite: Couldn't create {tag} table for database"))?;

            info!("SqliteDatabse - update_tags_to_file() - Inserting hash value {:?} into tag table {}", &file.file_hash, &tag);
            let query = format!("INSERT OR IGNORE INTO {tag} (file_hash) VALUES (?)");
            db.execute(query.as_str(), [&file.file_hash])
                .with_context(|| format!("SQLite: Couldn't insert tag into '{tag}' table"))?;
        }

        info!("SqliteDatabse - update_tags_to_file() - Serialising tags");
        let file_tags_serialised = serialise_tags(&file.tags)?;
        info!("SqliteDatabse - update_tags_to_file() - Update _files table");
        db.execute(
            "UPDATE _files SET tags=(?1) WHERE file_hash IS (?2)",
            [&file_tags_serialised, &file.file_hash],
        )?;

        Ok(())
    }
}
