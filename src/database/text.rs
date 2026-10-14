//! Length-aware UTF-8 operations. SQLite's text length and substring functions
//! stop at embedded NUL, which is an ordinary character in a Cypher string.

use rusqlite::{
    Connection,
    functions::{Context, FunctionFlags},
};

pub(super) fn register(connection: &Connection) -> rusqlite::Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    // Generated SQL handles nulls and argument kinds before calling these.
    connection.create_scalar_function("crabnet_text_size", 1, flags, |context| {
        Ok(context.get_raw(0).as_str()?.chars().count() as i64)
    })?;
    connection.create_scalar_function("crabnet_substring", 3, flags, |context| {
        let text = context.get_raw(0).as_str()?;
        let start = bound(context, 1)?;
        // Only an omitted Cypher length reaches this function as SQL NULL.
        let length = if matches!(context.get_raw(2), rusqlite::types::ValueRef::Null) {
            usize::MAX
        } else {
            bound(context, 2)?
        };
        Ok(substring(text, start, length).to_owned())
    })?;
    connection.create_scalar_function("crabnet_right", 2, flags, |context| {
        let text = context.get_raw(0).as_str()?;
        let count = bound(context, 1)?;
        let start = if count == 0 {
            text.len()
        } else {
            text.char_indices()
                .rev()
                .nth(count - 1)
                .map_or(0, |(index, _)| index)
        };
        Ok(text[start..].to_owned())
    })?;
    connection.create_scalar_function("crabnet_ends_with", 2, flags, |context| {
        Ok(context
            .get_raw(0)
            .as_str()?
            .ends_with(context.get_raw(1).as_str()?))
    })?;
    // SQLite treats a NUL-prefixed search string as empty.
    connection.create_scalar_function("crabnet_replace", 3, flags, |context| {
        let text = context.get_raw(0).as_str()?;
        let search = context.get_raw(1).as_str()?;
        let replacement = context.get_raw(2).as_str()?;
        // Preserve the existing empty-search behavior; Rust's replace would
        // otherwise insert the replacement at every character boundary.
        if search.is_empty() {
            return Ok(text.to_owned());
        }
        // SAFETY: the non-owning connection reference stays in this callback.
        // A negative new value only reads the limit; no SQL or mutation occurs.
        let limit = unsafe {
            rusqlite::ffi::sqlite3_limit(
                context.get_connection()?.handle(),
                rusqlite::ffi::SQLITE_LIMIT_LENGTH,
                -1,
            )
        } as usize;
        let matches = text.matches(search).count();
        let retained = text.len() - matches * search.len();
        let length = matches
            .checked_mul(replacement.len())
            .and_then(|added| retained.checked_add(added))
            .filter(|length| *length <= limit)
            .ok_or_else(|| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_TOOBIG),
                    // Supplying a message makes rusqlite call result_error
                    // after result_error_code, overwriting SQLITE_TOOBIG.
                    None,
                )
            })?;
        // Check expansion before allocating, preserving SQLite's length limit.
        let mut result = String::with_capacity(length);
        let mut end = 0;
        for (start, matched) in text.match_indices(search) {
            result.push_str(&text[end..start]);
            result.push_str(replacement);
            end = start + matched.len();
        }
        result.push_str(&text[end..]);
        Ok(result)
    })?;
    Ok(())
}

fn bound(context: &Context<'_>, index: usize) -> rusqlite::Result<usize> {
    let value = context.get::<i64>(index)?;
    if value < 0 {
        return Err(rusqlite::Error::UserFunctionError(
            format!(
                "{}string bounds must not be negative",
                crate::error::ARGUMENT_ERROR_TAG
            )
            .into(),
        ));
    }
    // An index larger than the address space is beyond every possible string.
    Ok(usize::try_from(value).unwrap_or(usize::MAX))
}

fn substring(text: &str, start: usize, length: usize) -> &str {
    let start = text
        .char_indices()
        .nth(start)
        .map_or(text.len(), |(index, _)| index);
    let suffix = &text[start..];
    let end = suffix
        .char_indices()
        .nth(length)
        .map_or(suffix.len(), |(index, _)| index);
    &suffix[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_checks_the_connection_length_limit_before_expansion() {
        let connection = Connection::open_in_memory().unwrap();
        register(&connection).unwrap();
        // SAFETY: this test owns the live connection and changes its limit
        // before preparing or executing any statement.
        unsafe {
            rusqlite::ffi::sqlite3_limit(
                connection.handle(),
                rusqlite::ffi::SQLITE_LIMIT_LENGTH,
                4096,
            );
        }
        let source = "\0".repeat(256);
        let replacement = "x".repeat(16);
        let exact: String = connection
            .query_row(
                "SELECT crabnet_replace(?1, ?2, ?3)",
                rusqlite::params![source, "\0", replacement],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exact, "x".repeat(4096));
        let result: rusqlite::Result<String> = connection.query_row(
            "SELECT crabnet_replace(?1, ?2, ?3)",
            rusqlite::params!["\0".repeat(257), "\0", replacement],
            |row| row.get(0),
        );
        let error = result.unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::TooBig),
            "{error:?}"
        );
        let retry: String = connection
            .query_row(
                "SELECT crabnet_replace(?1, ?2, ?3)",
                rusqlite::params!["a\0b", "\0", "x"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retry, "axb");
    }
}
