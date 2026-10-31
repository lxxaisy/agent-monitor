use super::{store, ActivityError};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub(super) fn read_only(path: &Path) -> Result<Connection, ActivityError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_millis(750))?;
    Ok(connection)
}

pub(super) fn utc(value: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(value, 0)
        .map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true))
}

pub(super) fn utc_ms(value: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(value)
        .map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true))
}

pub(super) fn timestamp(value: &Value) -> Option<String> {
    value.as_str().and_then(|text| {
        DateTime::parse_from_rfc3339(text).ok().map(|date| {
            date.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        })
    })
}

pub(super) fn duration(start: &Option<String>, end: &Option<String>) -> Option<u64> {
    let start = DateTime::parse_from_rfc3339(start.as_ref()?).ok()?;
    let end = DateTime::parse_from_rfc3339(end.as_ref()?).ok()?;
    (end - start).num_milliseconds().try_into().ok()
}

pub(super) fn jsonl_files(root: &Path, result: &mut Vec<PathBuf>) -> Result<(), ActivityError> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            jsonl_files(&entry.path(), result)?;
        } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            result.push(entry.path());
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Cursor<T> {
    inode: u64,
    offset: u64,
    state: T,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct FileVersion {
    inode: u64,
    size: u64,
    modified_ns: u128,
}

pub(super) fn sqlite_version(path: &Path) -> Result<Vec<Option<FileVersion>>, ActivityError> {
    [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
    ]
    .iter()
    .map(|path| match std::fs::metadata(path) {
        Ok(metadata) => {
            #[cfg(unix)]
            let inode = {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            };
            #[cfg(not(unix))]
            let inode = 0;
            Ok(Some(FileVersion {
                inode,
                size: metadata.len(),
                modified_ns: metadata
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
                    .as_nanos(),
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    })
    .collect()
}

pub(super) fn jsonl<T, F>(
    db: &Connection,
    path: &Path,
    initial_offset: u64,
    mut visit: F,
) -> Result<usize, ActivityError>
where
    T: Serialize + DeserializeOwned + Default,
    F: FnMut(&Connection, &Value, &mut T) -> Result<(), ActivityError>,
{
    if path.extension().is_some_and(|extension| extension == "zst") {
        return compressed_jsonl(db, path, visit);
    }
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        metadata.ino()
    };
    #[cfg(not(unix))]
    let inode = 0;
    let key = format!("cursor-v2:{}", path.display());
    let previous = store::metadata::<Cursor<T>>(db, &key)?;
    let mut cursor = match previous {
        Some(previous) if previous.inode == inode && previous.offset <= metadata.len() => previous,
        _ => Cursor {
            inode,
            offset: initial_offset.min(metadata.len()),
            state: T::default(),
        },
    };
    let error_key = format!("invalid:{}", path.display());
    if cursor.inode == inode && cursor.offset == metadata.len() {
        return Ok(store::metadata::<usize>(db, &error_key)?.unwrap_or(0));
    }
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(cursor.offset))?;
    let mut line = Vec::new();
    let mut invalid = 0;
    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 || line.last() != Some(&b'\n') {
            break;
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(value) => visit(db, &value, &mut cursor.state)?,
            Err(_) => invalid += 1,
        }
        cursor.offset += count as u64;
    }
    store::save_metadata(db, &key, &cursor)?;
    let previous = store::metadata::<usize>(db, &error_key)?.unwrap_or(0);
    store::save_metadata(db, &error_key, &(previous + invalid))?;
    Ok(previous + invalid)
}

fn compressed_jsonl<T, F>(
    db: &Connection,
    path: &Path,
    mut visit: F,
) -> Result<usize, ActivityError>
where
    T: Serialize + DeserializeOwned + Default,
    F: FnMut(&Connection, &Value, &mut T) -> Result<(), ActivityError>,
{
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        metadata.ino()
    };
    #[cfg(not(unix))]
    let inode = 0;
    let key = format!("compressed:{}", path.display());
    let error_key = format!("compressed-invalid:{}", path.display());
    let previous = store::metadata::<Cursor<T>>(db, &key)?;
    if previous
        .as_ref()
        .is_some_and(|cursor| cursor.inode == inode && cursor.offset == metadata.len())
    {
        return Ok(store::metadata::<usize>(db, &error_key)?.unwrap_or(0));
    }
    // Archived rollouts use compressed bytes; SQLite's rollout offsets refer to decoded data.
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut reader = BufReader::new(decoder);
    let mut state = T::default();
    let mut line = Vec::new();
    let mut invalid = 0;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(value) => visit(db, &value, &mut state)?,
            Err(_) => invalid += 1,
        }
    }
    store::save_metadata(
        db,
        &key,
        &Cursor {
            inode,
            offset: metadata.len(),
            state,
        },
    )?;
    store::save_metadata(db, &error_key, &invalid)?;
    Ok(invalid)
}
