//! Provider keys live in the selected Codex home's `.env`, independently of account login.
//! Read the file at request time so onboarding can save a key without mutating a
//! multithreaded process's environment or serializing credentials into config.

use std::io;
use std::io::Write;
use std::path::Path;

use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::EnvVarError;

fn read_dotenv(home: &Path) -> io::Result<String> {
    match std::fs::read_to_string(home.join(".env")) {
        Ok(contents) => Ok(contents),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!(
                "Cannot read {}: {}",
                home.join(".env").display(),
                err.kind()
            ),
        )),
    }
}

fn parse_dotenv(contents: &str) -> io::Result<Vec<(String, String)>> {
    dotenvy::from_read_iter(contents.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        // dotenvy's error includes the offending line, which may contain a secret.
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid .env syntax; fix the file and retry",
            )
        })
}

/// Resolve one key, with the same file-over-environment precedence as CLI bootstrap.
/// An explicit empty file entry shadows the process environment and needs setup.
pub fn read_provider_key(home: &Path, env_key: &str) -> io::Result<Option<String>> {
    let entries = parse_dotenv(&read_dotenv(home)?)?;
    Ok(entries
        .into_iter()
        .rev()
        .find_map(|(key, value)| (key == env_key).then_some(value))
        .or_else(|| std::env::var(env_key).ok())
        .filter(|value| !value.trim().is_empty()))
}

/// The common resolver for startup, model selection, catalog identity and inference.
/// Providers created without a home keep their environment-only behavior.
pub fn provider_api_key(
    provider: &ModelProviderInfo,
    home: Option<&Path>,
) -> Result<Option<String>, CodexErr> {
    let (Some(home), Some(env_key)) = (home, provider.env_key.as_deref()) else {
        return provider.api_key();
    };
    read_provider_key(home, env_key)?.map(Some).ok_or_else(|| {
        CodexErr::EnvVar(EnvVarError {
            var: env_key.to_owned(),
            instructions: provider.env_key_instructions.clone(),
        })
    })
}

fn validate_input(env_key: &str, value: &str) -> io::Result<()> {
    let valid_name = !env_key.is_empty()
        && env_key.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
        && !env_key.to_ascii_uppercase().starts_with("CODEX_");
    if !valid_name {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Provider env_key must be an environment variable name outside the reserved CODEX_ namespace",
        ));
    }
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "API key must be nonempty and contain no line breaks or control characters",
        ));
    }
    Ok(())
}

/// Atomically save a missing key, preserving other assignments and comments.
/// Existing nonempty keys are left alone, including keys saved by another launch.
pub fn save_provider_key(home: &Path, env_key: &str, value: &str) -> io::Result<()> {
    validate_input(env_key, value)?;
    let path = home.join(".env");
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The home .env is a symbolic link; edit its key manually",
        ));
    }
    let original = read_dotenv(home)?;
    let entries = parse_dotenv(&original)?;
    if entries
        .iter()
        .rev()
        .find(|(key, _)| key == env_key)
        .is_some_and(|(_, value)| !value.trim().is_empty())
    {
        return Ok(());
    }

    // Group complete dotenv records so unrelated multiline values survive unchanged.
    let mut updated = String::new();
    let mut record = String::new();
    for line in original.split_inclusive('\n') {
        record.push_str(line);
        if let Ok(entries) = parse_dotenv(&record) {
            if !entries.iter().any(|(key, _)| key == env_key) {
                updated.push_str(&record);
            }
            record.clear();
        }
    }
    if !record.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Cannot safely update .env; edit the provider key manually",
        ));
    }
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    let escaped = value
        .trim()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$");
    updated.push_str(&format!("{env_key}=\"{escaped}\"\n"));
    parse_dotenv(&updated)?;
    std::fs::create_dir_all(home)?;
    let mut temporary = tempfile::NamedTempFile::new_in(home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(updated.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const KEY: &str = "CUSTOMCODEX_TEST_PROVIDER_KEY_7913";

    #[test]
    fn missing_save_reload_preserves_unrelated_records() -> io::Result<()> {
        let home = tempfile::tempdir()?;
        assert_eq!(read_provider_key(home.path(), KEY)?, None);
        let original = format!(
            "# keep me\nOTHER='other-secret'\nMULTILINE=\"first\nsecond\"\nexport {KEY}=\" \"\n# keep this too\n"
        );
        std::fs::write(home.path().join(".env"), &original)?;
        assert_eq!(read_provider_key(home.path(), KEY)?, None);
        save_provider_key(home.path(), KEY, "  new-secret  ")?;
        assert_eq!(
            read_provider_key(home.path(), KEY)?,
            Some("new-secret".into())
        );
        let saved = std::fs::read_to_string(home.path().join(".env"))?;
        assert!(saved.contains("# keep me\nOTHER='other-secret'\nMULTILINE=\"first\nsecond\"\n"));
        assert!(saved.contains("# keep this too\n"));
        assert_eq!(saved.matches(KEY).count(), 1);
        save_provider_key(home.path(), KEY, "replacement")?;
        assert_eq!(std::fs::read_to_string(home.path().join(".env"))?, saved);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.path().join(".env"))?
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        Ok(())
    }

    #[test]
    fn quoted_values_round_trip_without_expansion() -> io::Result<()> {
        let home = tempfile::tempdir()?;
        let secret = r#"key-$HOME-${USER}-"quoted"-\slash-'end'"#;
        save_provider_key(home.path(), KEY, secret)?;
        assert_eq!(read_provider_key(home.path(), KEY)?, Some(secret.into()));
        Ok(())
    }

    #[test]
    fn file_precedence_and_last_assignment_match_bootstrap() -> io::Result<()> {
        let home = tempfile::tempdir()?;
        // PATH is already set; no process-global test environment mutation is needed.
        assert_eq!(
            read_provider_key(home.path(), "PATH")?,
            std::env::var("PATH").ok()
        );
        std::fs::write(home.path().join(".env"), "PATH=from-file\n")?;
        assert_eq!(
            read_provider_key(home.path(), "PATH")?,
            Some("from-file".into())
        );
        std::fs::write(home.path().join(".env"), "PATH=from-file\nPATH=\n")?;
        assert_eq!(read_provider_key(home.path(), "PATH")?, None);
        Ok(())
    }

    #[test]
    fn invalid_input_or_file_never_damages_credentials() -> io::Result<()> {
        let home = tempfile::tempdir()?;
        for input in ["", "  ", "key\nother", "key\r", "key\0"] {
            assert!(save_provider_key(home.path(), KEY, input).is_err());
        }
        for name in ["", "BAD=KEY", "CODEX_HOME", "1KEY"] {
            assert!(save_provider_key(home.path(), name, "secret").is_err());
        }
        assert!(!home.path().join(".env").exists());
        let malformed = "OTHER=\"unclosed-secret";
        std::fs::write(home.path().join(".env"), malformed)?;
        let error = save_provider_key(home.path(), KEY, "secret").unwrap_err();
        assert!(!error.to_string().contains("unclosed-secret"));
        assert_eq!(
            std::fs::read_to_string(home.path().join(".env"))?,
            malformed
        );
        assert!(read_provider_key(home.path(), KEY).is_err());
        Ok(())
    }

    #[test]
    fn unreadable_destination_returns_error() -> io::Result<()> {
        let home = tempfile::tempdir()?;
        std::fs::create_dir(home.path().join(".env"))?;
        assert!(read_provider_key(home.path(), KEY).is_err());
        assert!(save_provider_key(home.path(), KEY, "secret").is_err());
        Ok(())
    }
}
