use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::DaemonSettings;
use super::MAX_SHUTDOWN_GRACE_SECONDS;

#[tokio::test]
async fn shutdown_grace_accepts_zero_through_five_minutes() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("settings.json");
    assert_eq!(
        DaemonSettings::load(&path).await.expect("missing settings"),
        DaemonSettings::default()
    );
    assert_eq!(
        DaemonSettings::load_for_stop(&path)
            .await
            .shutdown_grace_seconds,
        60
    );

    for seconds in [
        0,
        MAX_SHUTDOWN_GRACE_SECONDS,
        MAX_SHUTDOWN_GRACE_SECONDS + 1,
    ] {
        tokio::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"shutdownGraceSeconds": seconds}))
                .expect("serialize settings"),
        )
        .await
        .expect("write settings");
        let expected = (seconds <= MAX_SHUTDOWN_GRACE_SECONDS).then_some(seconds);
        assert_eq!(
            DaemonSettings::load(&path)
                .await
                .ok()
                .map(|settings| settings.shutdown_grace_seconds),
            expected
        );
        assert_eq!(
            DaemonSettings::load_for_stop(&path)
                .await
                .shutdown_grace_seconds,
            expected.unwrap_or(60)
        );
    }

    tokio::fs::write(&path, r#"{"shutdownGraceSeconds":"unlimited"}"#)
        .await
        .expect("write invalid settings");
    assert!(DaemonSettings::load(&path).await.is_err());
    assert_eq!(
        DaemonSettings::load_for_stop(&path)
            .await
            .shutdown_grace_seconds,
        60
    );
}
