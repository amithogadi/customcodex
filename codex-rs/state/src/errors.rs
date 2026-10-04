use std::borrow::Cow;

pub(crate) fn classify_error(err: &anyhow::Error) -> &'static str {
    for cause in err.chain() {
        if let Some(sqlx_err) = cause.downcast_ref::<sqlx::Error>() {
            return classify_sqlx_error(sqlx_err);
        }
        if cause
            .downcast_ref::<sqlx::migrate::MigrateError>()
            .is_some()
        {
            return "migration";
        }
        if cause.downcast_ref::<serde_json::Error>().is_some() {
            return "serde";
        }
        if cause.downcast_ref::<std::io::Error>().is_some() {
            return "io";
        }
    }
    if crate::is_sqlite_corruption_error(err) {
        "corrupt"
    } else {
        "unknown"
    }
}

fn classify_sqlx_error(err: &sqlx::Error) -> &'static str {
    match err {
        sqlx::Error::Database(database_error) => {
            let code = database_error
                .code()
                .unwrap_or(Cow::Borrowed("none"))
                .to_string();
            classify_sqlite_code(code.as_str())
        }
        sqlx::Error::PoolTimedOut => "pool_timeout",
        sqlx::Error::Io(_) => "io",
        sqlx::Error::ColumnDecode { source, .. } if source.is::<serde_json::Error>() => "serde",
        sqlx::Error::Decode(source) if source.is::<serde_json::Error>() => "serde",
        _ => "unknown",
    }
}

fn classify_sqlite_code(code: &str) -> &'static str {
    // SQLite result codes are documented at https://www.sqlite.org/rescode.html.
    // Extended codes preserve the primary code in the low byte.
    let primary_code = code.parse::<i32>().ok().map(|code| code & 0xff);
    match primary_code {
        Some(5) => "busy",
        Some(6) => "locked",
        Some(8) => "readonly",
        Some(10) => "io",
        Some(11) => "corrupt",
        Some(13) => "full",
        Some(14) => "cantopen",
        Some(17) => "schema",
        Some(19) => "constraint",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn classifies_extended_sqlite_codes() {
        assert_eq!(classify_sqlite_code("5"), "busy");
        assert_eq!(classify_sqlite_code("6"), "locked");
        assert_eq!(classify_sqlite_code("2067"), "constraint");
    }
}
