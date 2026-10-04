use clap::Args;
use clap::FromArgMatches;
use clap::Parser;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cli::ApprovalModeCliArg;
use codex_utils_cli::CliConfigOverrides;
use codex_utils_cli::SharedCliOptions;

#[derive(Parser, Clone, Debug)]
#[command(version)]
pub struct Cli {
    /// Optional user prompt to start the session.
    #[arg(value_name = "PROMPT", value_hint = clap::ValueHint::Other)]
    pub prompt: Option<String>,

    /// Error out when config.toml contains fields that are not recognized by this version of Codex.
    #[arg(long = "strict-config", default_value_t = false)]
    pub strict_config: bool,

    // Internal controls set by the top-level `codex resume` subcommand.
    // These are not exposed as user flags on the base `codex` command.
    #[clap(skip)]
    pub resume_picker: bool,

    #[clap(skip)]
    pub resume_last: bool,

    /// Internal: resume a specific recorded session by id (UUID). Set by the
    /// top-level `codex resume <SESSION_ID>` wrapper; not exposed as a public flag.
    #[clap(skip)]
    pub resume_session_id: Option<String>,

    /// Internal: show all sessions (disables cwd filtering and shows CWD column).
    #[clap(skip)]
    pub resume_show_all: bool,

    /// Internal: include non-interactive sessions in resume listings.
    #[clap(skip)]
    pub resume_include_non_interactive: bool,

    /// Internal: open the daemon-wide agents overview instead of starting a thread.
    #[clap(skip)]
    pub agents_overview: bool,

    // Internal controls set by the top-level `codex fork` subcommand.
    // These are not exposed as user flags on the base `codex` command.
    #[clap(skip)]
    pub fork_picker: bool,

    #[clap(skip)]
    pub fork_last: bool,

    /// Internal: fork a specific recorded session by id (UUID). Set by the
    /// top-level `codex fork <SESSION_ID>` wrapper; not exposed as a public flag.
    #[clap(skip)]
    pub fork_session_id: Option<String>,

    /// Internal: show all sessions (disables cwd filtering and shows CWD column).
    #[clap(skip)]
    pub fork_show_all: bool,

    #[clap(flatten)]
    pub shared: TuiSharedCliOptions,

    /// Configure when the model requires human approval before executing a command.
    #[arg(long = "ask-for-approval", short = 'a')]
    pub approval_policy: Option<ApprovalModeCliArg>,

    /// Enable live web search. When enabled, the native Responses `web_search` tool is available to the model (no per‑call approval).
    #[arg(long = "search", default_value_t = false)]
    pub web_search: bool,

    /// Disable alternate screen mode
    ///
    /// Runs the TUI in inline mode, preserving terminal scrollback history.
    #[arg(long = "no-alt-screen", default_value_t = false)]
    pub no_alt_screen: bool,

    /// Use the shared background server, starting it if configured and eligible.
    #[arg(long, conflicts_with = "no_daemon")]
    pub daemon: bool,

    /// Run without the shared background server (the default), even if it is already running.
    #[arg(long, conflicts_with = "daemon")]
    pub no_daemon: bool,

    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,
}

impl Cli {
    /// Validate after merging root and subcommand flags, before any server side effects.
    pub fn validate_daemon_options(&self, has_remote: bool) -> Result<(), &'static str> {
        if self.daemon && self.no_daemon {
            return Err("--daemon cannot be used with --no-daemon.");
        }
        if self.daemon && has_remote {
            return Err("--daemon cannot be used with --remote.");
        }
        if self.no_daemon && self.agents_overview {
            return Err(
                "--no-daemon cannot be used with codex agents. The agents overview requires a shared server. Use codex --no-daemon to work without it.",
            );
        }
        if self.no_daemon && has_remote {
            return Err("--no-daemon cannot be used with --remote.");
        }
        Ok(())
    }

    /// Only interactive local sessions default to embedded mode. Explicit shared-server
    /// commands and remote endpoints retain their own server selection.
    pub(crate) fn apply_interactive_daemon_default(&mut self, has_remote: bool) {
        if !self.daemon && !self.agents_overview && !has_remote {
            self.no_daemon = true;
        }
    }
}

impl std::ops::Deref for Cli {
    type Target = SharedCliOptions;

    fn deref(&self) -> &Self::Target {
        &self.shared.0
    }
}

impl std::ops::DerefMut for Cli {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.shared.0
    }
}

#[derive(Clone, Debug, Default)]
pub struct TuiSharedCliOptions(SharedCliOptions);

impl TuiSharedCliOptions {
    pub fn into_inner(self) -> SharedCliOptions {
        self.0
    }
}

impl std::ops::Deref for TuiSharedCliOptions {
    type Target = SharedCliOptions;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for TuiSharedCliOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Args for TuiSharedCliOptions {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        mark_tui_args(SharedCliOptions::augment_args(cmd))
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        mark_tui_args(SharedCliOptions::augment_args_for_update(cmd))
    }
}

impl FromArgMatches for TuiSharedCliOptions {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        SharedCliOptions::from_arg_matches(matches).map(Self)
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches(matches)
    }
}

fn mark_tui_args(cmd: clap::Command) -> clap::Command {
    cmd.mut_arg("dangerously_bypass_approvals_and_sandbox", |arg| {
        arg.conflicts_with("approval_policy")
    })
    .mut_arg("auto_review", |arg| arg.conflicts_with("approval_policy"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn interactive_launches_default_to_embedded() {
        for args in [vec!["codex"], vec!["codex", "--no-daemon"]] {
            for action in ["new", "resume", "fork"] {
                let mut cli = Cli::parse_from(&args);
                cli.resume_picker = action == "resume";
                cli.fork_picker = action == "fork";
                assert_eq!(cli.validate_daemon_options(false), Ok(()));
                cli.apply_interactive_daemon_default(false);
                assert!(cli.no_daemon, "{args:?} {action}");
                assert_eq!(
                    crate::daemon_startup::exclusion(
                        &cli,
                        &[],
                        &codex_config::LoaderOverrides::default(),
                        false,
                        None,
                    ),
                    Some("--no-daemon"),
                );
            }
        }
    }

    #[test]
    fn explicit_shared_server_launches_keep_their_policy() {
        for (args, remote, agents) in [
            (vec!["codex", "--daemon"], false, false),
            (vec!["codex"], true, false),
            (vec!["codex"], false, true),
        ] {
            let mut cli = Cli::parse_from(args);
            cli.agents_overview = agents;
            assert_eq!(cli.validate_daemon_options(remote), Ok(()));
            cli.apply_interactive_daemon_default(remote);
            assert!(!cli.no_daemon);
        }
    }

    #[test]
    fn daemon_flags_reject_conflicting_server_selection() {
        assert!(Cli::try_parse_from(["codex", "--daemon", "--no-daemon"]).is_err());
        for flag in ["--daemon", "--no-daemon"] {
            let cli = Cli::parse_from(["codex", flag]);
            assert_eq!(
                cli.validate_daemon_options(true),
                Err(if flag == "--daemon" {
                    "--daemon cannot be used with --remote."
                } else {
                    "--no-daemon cannot be used with --remote."
                }),
            );
        }
        let mut cli = Cli::parse_from(["codex", "--no-daemon"]);
        cli.agents_overview = true;
        assert!(cli.validate_daemon_options(false).is_err());
    }
}
