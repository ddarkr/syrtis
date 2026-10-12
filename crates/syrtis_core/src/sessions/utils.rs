//! Shared parsing helpers for session logs.

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::cell::Cell;
use std::io::{self, BufRead, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

thread_local! {
    static SOURCE_READ_FAILED: Cell<Option<bool>> = const { Cell::new(None) };
}

struct SourceReadGuard(Option<bool>);

impl Drop for SourceReadGuard {
    fn drop(&mut self) {
        SOURCE_READ_FAILED.set(self.0);
    }
}

/// Observe one synchronous source parse without changing the legacy Vec APIs.
/// Enter inside each worker, never around a rayon/thread dispatch. Nested scopes
/// and unwinding restore the enclosing observation rather than leaking state.
pub(crate) fn source_read_scope<T>(_path: &Path, parse: impl FnOnce() -> T) -> Result<T, String> {
    let _guard = SourceReadGuard(SOURCE_READ_FAILED.replace(Some(false)));
    let value = parse();
    if SOURCE_READ_FAILED.get() == Some(true) {
        Err("session source read incomplete".to_string())
    } else {
        Ok(value)
    }
}

fn source_read_failed() {
    SOURCE_READ_FAILED.with(|state| {
        if state.get().is_some() {
            state.set(Some(true));
        }
    });
}

fn hard_io_error(error: &io::Error) -> bool {
    !matches!(
        error.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::Interrupted
    )
}

fn observe_io<T>(result: io::Result<T>, optional: bool) -> io::Result<T> {
    if let Err(error) = &result {
        if hard_io_error(error) && !(optional && error.kind() == io::ErrorKind::NotFound) {
            source_read_failed();
        }
    }
    result
}

/// File reads are observed at the actual Read/Seek boundary, including reads
/// hidden inside BufReader, serde and decompression. No source reread is needed.
pub(crate) struct SourceReader<R>(R, bool);

impl<R: Read> Read for SourceReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.1 {
            return Ok(0);
        }
        let result = self.0.read(buffer);
        // Legacy line loops often continue on Err. A failed guarded source must
        // return so the caller can abort publication, not spin on repeated EIO.
        if result.as_ref().is_err_and(hard_io_error) && SOURCE_READ_FAILED.get().is_some() {
            self.1 = true;
        }
        observe_io(result, false)
    }
}

impl<R: Seek> Seek for SourceReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        observe_io(self.0.seek(position), false)
    }
}

impl SourceReader<std::fs::File> {
    pub(crate) fn metadata(&self) -> io::Result<std::fs::Metadata> {
        observe_io(self.0.metadata(), false)
    }
}

pub(crate) fn open_source_file(path: &Path) -> io::Result<SourceReader<std::fs::File>> {
    observe_io(std::fs::File::open(path), false).map(|file| SourceReader(file, false))
}

pub(crate) fn open_optional_source_file(path: &Path) -> io::Result<SourceReader<std::fs::File>> {
    observe_io(std::fs::File::open(path), true).map(|file| SourceReader(file, false))
}

pub(crate) fn read_source_file(path: &Path) -> io::Result<Vec<u8>> {
    observe_io(std::fs::read(path), false)
}

pub(crate) fn read_source_text(path: &Path) -> io::Result<String> {
    observe_io(std::fs::read_to_string(path), false)
}

pub(crate) fn read_optional_source_text(path: &Path) -> io::Result<String> {
    observe_io(std::fs::read_to_string(path), true)
}

pub(crate) fn read_optional_file_or_none(path: &Path) -> Option<Vec<u8>> {
    observe_io(std::fs::read(path), true).ok()
}

pub(crate) fn optional_source_is_file(path: &Path) -> bool {
    observe_io(std::fs::metadata(path), true)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

pub(crate) fn optional_source_exists(path: &Path) -> bool {
    observe_io(std::fs::metadata(path), true).is_ok()
}

fn sqlite_read_failed(error: &rusqlite::Error) -> bool {
    !matches!(
        error,
        rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::Utf8Error(..)
            | rusqlite::Error::InvalidColumnIndex(..)
            | rusqlite::Error::InvalidColumnName(..)
            | rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::QueryReturnedNoRows
    )
}

/// Conversion/invalid-row errors retain their old skip semantics; SQLite
/// execution errors (including a failure after a successful prefix) do not.
pub(crate) fn observe_sqlite<T>(result: rusqlite::Result<T>) -> rusqlite::Result<T> {
    if result.as_ref().is_err_and(sqlite_read_failed) {
        source_read_failed();
    }
    result
}

pub(crate) fn source_rows<T>(
    rows: impl Iterator<Item = rusqlite::Result<T>>,
) -> impl Iterator<Item = rusqlite::Result<T>> {
    rows.inspect(|row| {
        if row.as_ref().is_err_and(sqlite_read_failed) {
            source_read_failed();
        }
    })
}

/// Only a verified absent table/column permits the original schema fallback.
/// Schema checks run on a failed prepare only and never read historical rows.
pub(crate) fn prepare_source_sqlite<'a>(
    conn: &'a Connection,
    query: &str,
    schema: &[(&str, &[&str])],
) -> rusqlite::Result<rusqlite::Statement<'a>> {
    let result = conn.prepare(query);
    if let Err(error) = &result {
        let details = match error {
            rusqlite::Error::SqliteFailure(error, message) => {
                Some((error.code, message.as_deref()))
            }
            rusqlite::Error::SqlInputError { error, msg, .. } => {
                Some((error.code, Some(msg.as_str())))
            }
            _ => None,
        };
        let absent = details.is_some_and(|(code, message)| {
            code == rusqlite::ErrorCode::Unknown
                && message.is_some_and(|message| {
                    if let Some(missing) = message.strip_prefix("no such table: ") {
                        schema.iter().any(|(table, _)| {
                            missing.eq_ignore_ascii_case(table)
                                && matches!(source_schema_present(conn, table, &[]), Ok(false))
                        })
                    } else if let Some(missing) = message.strip_prefix("no such column: ") {
                        let column = missing.rsplit('.').next().unwrap_or(missing);
                        schema.iter().any(|(table, columns)| {
                            columns.iter().any(|name| column.eq_ignore_ascii_case(name))
                                && matches!(
                                    source_schema_present(conn, table, &[column]),
                                    Ok(false)
                                )
                        })
                    } else {
                        false
                    }
                })
        });
        if !absent {
            source_read_failed();
        }
    }
    result
}

fn source_schema_present(
    conn: &Connection,
    table: &str,
    columns: &[&str],
) -> rusqlite::Result<bool> {
    // Names are fixed parser schema declarations, never source input.
    let mut statement = observe_sqlite(conn.prepare(&format!("PRAGMA table_info(\"{table}\")")))?;
    let mut rows = observe_sqlite(statement.query([]))?;
    let mut found = vec![false; columns.len()];
    let mut present = false;
    while let Some(row) = observe_sqlite(rows.next())? {
        present = true;
        let name: String = row.get(1)?;
        for (index, expected) in columns.iter().enumerate() {
            if name.eq_ignore_ascii_case(expected) {
                found[index] = true;
            }
        }
    }
    Ok(present && found.into_iter().all(|value| value))
}

/// Iterate a reader line by line without letting one undecodable byte discard
/// the rest of the stream (upstream `cfe1304a`, #1031).
///
/// `BufRead::lines()` yields `Err(InvalidData)` for any line that is not valid
/// UTF-8, and the `map_while(Result::ok)` spelling turns that into
/// end-of-iteration: a single stray byte anywhere in a multi-megabyte session
/// log silently dropped every record after it (upstream #1031 measured ~2% of
/// an 83MB Grok `updates.jsonl` surviving). Reading raw bytes up to each
/// newline and decoding them lossily keeps the cost of a bad byte local to its
/// own line.
///
/// Line endings match `lines()`: the trailing `\n` and any preceding `\r` are
/// stripped, and a final line without a newline is still yielded. A UTF-8 BOM
/// at the very start of the stream is stripped.
pub(crate) fn lossy_lines<R: BufRead>(reader: R) -> LossyLines<R> {
    LossyLines {
        reader,
        buf: Vec::new(),
        at_start: true,
    }
}

pub(crate) struct LossyLines<R> {
    reader: R,
    buf: Vec<u8>,
    at_start: bool,
}

impl<R: BufRead> Iterator for LossyLines<R> {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        self.buf.clear();
        match self.reader.read_until(b'\n', &mut self.buf) {
            Ok(0) => None,
            Ok(_) => {
                if self.buf.last() == Some(&b'\n') {
                    self.buf.pop();
                    if self.buf.last() == Some(&b'\r') {
                        self.buf.pop();
                    }
                }
                let bom = "\u{feff}".as_bytes();
                let start = if std::mem::take(&mut self.at_start) && self.buf.starts_with(bom) {
                    bom.len()
                } else {
                    0
                };
                Some(String::from_utf8_lossy(&self.buf[start..]).into_owned())
            }
            // A hard I/O error (vanished mount, EIO) does not consume input, so
            // retrying would spin on the same failing read forever. Stop instead.
            Err(error) => {
                let _ = observe_io::<()>(Err(error), false);
                None
            }
        }
    }
}

pub(crate) fn extract_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(|val| {
        val.as_i64()
            .or_else(|| val.as_u64().map(|v| v as i64))
            .or_else(|| val.as_str().and_then(|s| s.parse::<i64>().ok()))
    })
}

pub(crate) fn extract_string(value: Option<&Value>) -> Option<String> {
    value.and_then(|val| val.as_str().map(|s| s.to_string()))
}

pub(crate) fn parse_timestamp_value(value: &Value) -> Option<i64> {
    if let Some(ts) = value.as_str() {
        return parse_timestamp_str(ts);
    }

    let numeric = value
        .as_i64()
        .or_else(|| value.as_u64().map(|v| v as i64))?;
    if numeric <= 0 {
        return None;
    }
    if numeric >= 1_000_000_000_000 {
        Some(numeric)
    } else {
        // Seconds -> milliseconds: saturating so a garbage/huge timestamp
        // cannot overflow i64 during the conversion.
        Some(numeric.saturating_mul(1000))
    }
}

pub(crate) fn parse_timestamp_str(value: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(dt.timestamp_millis());
    }

    // Timezone-less ISO-8601 datetimes (e.g. "2026-06-16T12:00:00",
    // "2026-06-16 12:00:00", optional fractional seconds) carry no offset, so
    // `parse_from_rfc3339` rejects them. Interpret them as UTC rather than
    // collapsing to the file mtime, which would scatter the message into the
    // wrong day/month bucket.
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(value, format) {
            return Some(naive.and_utc().timestamp_millis());
        }
    }

    if let Ok(numeric) = value.parse::<i64>() {
        if numeric <= 0 {
            return None;
        }
        if numeric >= 1_000_000_000_000 {
            return Some(numeric);
        }
        // Seconds -> milliseconds: saturating so a garbage/huge timestamp
        // cannot overflow i64 during the conversion.
        return Some(numeric.saturating_mul(1000));
    }

    None
}

/// Modification time in epoch milliseconds, or `None` when the filesystem does
/// not report one.
///
/// `SystemTime::modified` is the one timestamp every tier-1 target supports
/// (unlike `created`, which is absent on many Linux filesystems), so callers
/// that need a portable "when was this last written" anchor use this.
///
/// Returns `None` rather than substituting a value so a caller with its own
/// fallback can reach for it instead of silently anchoring on the wrong
/// instant. Pre-epoch mtimes also collapse to `None`: `duration_since` fails
/// for them, and a negative anchor would bucket the record before 1970.
pub(crate) fn file_modified_timestamp_ms_opt(path: &Path) -> Option<i64> {
    source_metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
}

pub(crate) fn file_modified_timestamp_ms(path: &Path) -> i64 {
    file_modified_timestamp_ms_opt(path).unwrap_or_else(|| chrono::Utc::now().timestamp_millis())
}

pub(crate) fn source_metadata(path: &Path) -> io::Result<std::fs::Metadata> {
    observe_io(std::fs::metadata(path), true)
}

/// Open a SQLite file for read-only access with no mutex (single-threaded parser use).
/// Returns `None` if the file cannot be opened — the caller treats that as "no sessions".
pub(crate) fn open_readonly_sqlite(path: &Path) -> Option<Connection> {
    let conn = observe_sqlite(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ))
    .ok()?;
    observe_sqlite(conn.execute_batch(
        "PRAGMA cache_size=-256; PRAGMA temp.cache_size=-64; PRAGMA temp_store=FILE; PRAGMA mmap_size=0;",
    ))
    .ok()?;
    Some(conn)
}

/// Read a file into bytes, returning `None` on any I/O error instead of propagating.
/// Used by parsers that treat missing/unreadable session files as "no data".
pub(crate) fn read_file_or_none(path: &Path) -> Option<Vec<u8>> {
    read_source_file(path).ok()
}

/// Back-calculate a start anchor from a recorded end timestamp and elapsed
/// duration: `end - duration`. Non-positive candidates fall back to `end` so
/// sessionization does not silently discard corrupt or clock-skewed rows.
pub(crate) fn back_anchor_timestamp(end: i64, duration: i64) -> i64 {
    end.checked_sub(duration)
        .filter(|candidate| *candidate > 0)
        .unwrap_or(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from upstream `cfe1304a`.
    #[test]
    fn lossy_lines_survives_undecodable_bytes_and_strips_a_bom() {
        let raw: &[u8] = b"\xef\xbb\xbffirst\r\nse\xffcond\nthird";
        let lines: Vec<String> = lossy_lines(raw).collect();
        assert_eq!(lines, vec!["first", "se\u{fffd}cond", "third"]);
    }

    // Ported from upstream `cfe1304a`.
    #[test]
    fn lossy_lines_keeps_empty_lines_and_ends_at_eof() {
        let raw: &[u8] = b"a\n\nb\n";
        let lines: Vec<String> = lossy_lines(raw).collect();
        assert_eq!(lines, vec!["a", "", "b"]);
    }

    #[test]
    fn parse_timestamp_value_rejects_zero_and_negative_numbers() {
        assert!(parse_timestamp_value(&serde_json::json!(0)).is_none());
        assert!(parse_timestamp_value(&serde_json::json!(-1000)).is_none());
        assert!(parse_timestamp_value(&serde_json::json!(-1_700_000_000_000_i64)).is_none());
    }

    #[test]
    fn parse_timestamp_value_accepts_positive_numbers() {
        assert_eq!(
            parse_timestamp_value(&serde_json::json!(1_700_000_000_000_i64)),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_timestamp_value(&serde_json::json!(1_700_000_000_i64)),
            Some(1_700_000_000_000)
        );
    }

    #[test]
    fn parse_timestamp_str_rejects_zero_and_negative_strings() {
        assert!(parse_timestamp_str("0").is_none());
        assert!(parse_timestamp_str("-5").is_none());
    }

    #[test]
    fn parse_timestamp_str_accepts_timezone_less_datetimes_as_utc() {
        // "2026-06-16T12:00:00" UTC == 1781611200000 ms.
        assert_eq!(
            parse_timestamp_str("2026-06-16T12:00:00"),
            Some(1_781_611_200_000)
        );
        // Space separator and fractional seconds variants.
        assert_eq!(
            parse_timestamp_str("2026-06-16 12:00:00"),
            Some(1_781_611_200_000)
        );
        assert_eq!(
            parse_timestamp_str("2026-06-16T12:00:00.500"),
            Some(1_781_611_200_500)
        );
        // Offset-bearing input still goes through the rfc3339 path unchanged.
        assert_eq!(
            parse_timestamp_str("2026-06-16T12:00:00Z"),
            Some(1_781_611_200_000)
        );
    }
}

#[cfg(test)]
#[path = "source_read_tests.rs"]
mod source_read_tests;
