#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerVersionNoticeKind {
    Older,
    Different,
}

/// Stable clients compare release precedence. Prerelease clients only order versions
/// within the same release line; local builds and other release lines compare identity.
pub(crate) fn server_version_notice_kind(
    client: &str,
    server: &str,
) -> Option<ServerVersionNoticeKind> {
    let client = semver::Version::parse(client).ok()?;
    let server = semver::Version::parse(server).ok()?;
    let client_release = (client.major, client.minor, client.patch);
    let server_release = (server.major, server.minor, server.patch);
    let client_is_local = client_release == (0, 0, 0) || !client.build.is_empty();
    if client_is_local || (!client.pre.is_empty() && client_release != server_release) {
        return (client != server).then_some(ServerVersionNoticeKind::Different);
    }
    (server.build.is_empty() && server_release != (0, 0, 0) && client > server)
        .then_some(ServerVersionNoticeKind::Older)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn stable_clients_only_warn_for_older_releases() {
        for (client, server, expected) in [
            ("0.152.1", "0.152.0", Some(ServerVersionNoticeKind::Older)),
            ("0.153.0", "0.152.1", Some(ServerVersionNoticeKind::Older)),
            (
                "0.153.0",
                "0.153.0-alpha.10.1",
                Some(ServerVersionNoticeKind::Older),
            ),
            (
                "0.156.0",
                "0.155.0-alpha.12",
                Some(ServerVersionNoticeKind::Older),
            ),
            ("0.153.0", "0.153.0", None),
            ("0.153.0", "0.154.0", None),
            ("0.153.0", "0.154.0-alpha.1", None),
            ("0.153.0", "0.0.0", None),
            ("0.153.0", "0.0.0-alpha.1", None),
            ("0.153.0", "0.152.0+dev", None),
        ] {
            assert_eq!(server_version_notice_kind(client, server), expected);
        }
    }

    #[test]
    fn prerelease_clients_compare_within_the_same_release_line() {
        for (newer, older) in [
            ("0.155.0-alpha.23", "0.155.0-alpha.22"),
            ("0.155.0-alpha.24", "0.155.0-alpha.23"),
            ("0.153.0-alpha.10", "0.153.0-alpha.9"),
            ("0.153.0-alpha.10.1", "0.153.0-alpha.9.2"),
            ("0.153.0-alpha.10.10", "0.153.0-alpha.10.9"),
            ("0.153.0-alpha.10.1", "0.153.0-alpha.10"),
            ("0.155.0", "0.155.0-alpha.23"),
        ] {
            assert_eq!(
                server_version_notice_kind(newer, older),
                Some(ServerVersionNoticeKind::Older)
            );
            assert_eq!(server_version_notice_kind(older, newer), None);
            assert_eq!(server_version_notice_kind(newer, newer), None);
        }
    }

    #[test]
    fn prerelease_clients_on_other_release_lines_and_local_clients_warn_for_mismatches() {
        for (client, server) in [
            ("0.155.0-alpha.23", "0.156.0"),
            ("0.155.0-alpha.12", "0.154.0"),
            ("0.0.0", "0.153.0"),
            ("0.0.0", "0.153.0-alpha.10"),
            ("0.153.0+dev", "0.153.0"),
            ("0.153.0-alpha.10", "0.0.0"),
        ] {
            assert_eq!(
                server_version_notice_kind(client, server),
                Some(ServerVersionNoticeKind::Different)
            );
            assert_eq!(server_version_notice_kind(client, client), None);
        }
    }

    #[test]
    fn unknown_or_malformed_versions_do_not_produce_notices() {
        for version in [
            "unknown",
            "dev",
            "0.0.0.0",
            "0.153",
            "0.153.0.1",
            " 0.153.0",
            "+0.153.0",
            "0.0153.0",
            "0.153.0-alpha.01",
            "0.153.0-alpha..1",
        ] {
            for release in ["0.153.0", "0.153.0-alpha.10", "0.0.0"] {
                assert_eq!(server_version_notice_kind(version, release), None);
                assert_eq!(server_version_notice_kind(release, version), None);
            }
        }
    }
}
