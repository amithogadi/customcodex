use clap::error::ErrorKind;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn exec_server_help_documents_direct_transports() {
    let error = MultitoolCli::try_parse_from(["codex", "exec-server", "--help"])
        .expect_err("help should exit before running the executor");
    assert_eq!(error.kind(), ErrorKind::DisplayHelp);
    let help = error.to_string();
    assert!(help.contains("--listen"));
    assert!(!help.contains("--environment-id"));
    assert!(!help.contains("--remote-transport"));
}

#[test]
fn exec_server_rejects_hosted_registration_options() {
    for option in [
        "--remote-transport",
        "--environment-id",
        "--use-agent-identity-auth",
        "--aws-sigv4",
    ] {
        let error = MultitoolCli::try_parse_from(["codex", "exec-server", option])
            .expect_err("hosted registration options are no longer supported");
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }
}
