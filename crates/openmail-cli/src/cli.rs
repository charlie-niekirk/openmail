//! Command-line arguments.

use clap::{Parser, Subcommand};

/// Openmail developer CLI: exercise the Gmail client against a real account
/// without the desktop UI.
#[derive(Debug, Parser)]
#[command(name = "openmail-cli", version)]
pub struct Cli {
    /// The account to use, by email address. Defaults to the only signed-in one.
    #[arg(long, global = true, value_name = "EMAIL")]
    pub account: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

/// What to do.
#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Sign in to a Gmail account in the browser and keep its refresh token in
    /// the OS keychain.
    Login,
    /// Forget an account's refresh token.
    Logout,
    /// List system and user labels with message and unread counts.
    Labels,
    /// List threads in a label, newest first.
    Threads {
        /// The label: an ID such as `INBOX` or `Label_12`, or a name such as
        /// `Drafts` or `Work/Projects`.
        label: String,
        /// How many threads to list.
        #[arg(
            long,
            short = 'n',
            default_value_t = 20,
            value_parser = clap::value_parser!(u32).range(1..)
        )]
        limit: u32,
        /// A Gmail search query to narrow the list, such as `from:ada is:unread`.
        #[arg(long, short = 'q', value_name = "Q")]
        query: Option<String>,
    },
    /// Print every message in a thread: headers, text body and attachment names.
    Show {
        /// The thread ID, as printed by `threads`.
        thread_id: String,
    },
    /// Download labels and thread metadata into the local store: Inbox
    /// threads, then the rest of the mailbox, back to a cutoff. Interrupted
    /// syncs resume.
    Sync {
        /// How many days of mail to sync.
        #[arg(
            long,
            default_value_t = 90,
            value_parser = clap::value_parser!(u32).range(1..),
            conflicts_with = "all"
        )]
        days: u32,
        /// Sync the whole mailbox instead of the last `--days`.
        #[arg(long)]
        all: bool,
        /// Also fetch the bodies of the N newest Inbox messages.
        #[arg(long, value_name = "N")]
        prefetch: Option<usize>,
    },
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap::error::ErrorKind;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("openmail-cli").chain(args.iter().copied()))
    }

    #[test]
    fn definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_login_and_logout() {
        let cli = parse(&["login"]).unwrap();
        assert_eq!(cli.command, Command::Login);
        assert_eq!(cli.account, None);

        let cli = parse(&["logout", "--account", "ada@example.com"]).unwrap();
        assert_eq!(cli.command, Command::Logout);
        assert_eq!(cli.account.as_deref(), Some("ada@example.com"));
    }

    #[test]
    fn account_is_global() {
        for args in [
            ["--account", "ada@example.com", "labels"],
            ["labels", "--account", "ada@example.com"],
        ] {
            let cli = parse(&args).unwrap();
            assert_eq!(cli.command, Command::Labels);
            assert_eq!(cli.account.as_deref(), Some("ada@example.com"));
        }
    }

    #[test]
    fn threads_defaults() {
        let cli = parse(&["threads", "INBOX"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Threads {
                label: "INBOX".to_owned(),
                limit: 20,
                query: None,
            }
        );
    }

    #[test]
    fn threads_with_limit_and_query() {
        let cli = parse(&[
            "threads",
            "Work/Projects",
            "--limit",
            "5",
            "--query",
            "from:ada is:unread",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Command::Threads {
                label: "Work/Projects".to_owned(),
                limit: 5,
                query: Some("from:ada is:unread".to_owned()),
            }
        );

        let cli = parse(&["threads", "SENT", "-n", "3", "-q", "has:attachment"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Threads {
                label: "SENT".to_owned(),
                limit: 3,
                query: Some("has:attachment".to_owned()),
            }
        );
    }

    #[test]
    fn threads_rejects_a_zero_or_invalid_limit() {
        for limit in ["0", "-1", "many"] {
            let err = parse(&["threads", "INBOX", "--limit", limit]).unwrap_err();
            assert!(
                matches!(
                    err.kind(),
                    ErrorKind::ValueValidation | ErrorKind::UnknownArgument
                ),
                "{limit}: {:?}",
                err.kind()
            );
        }
    }

    #[test]
    fn threads_requires_a_label() {
        let err = parse(&["threads"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_show() {
        let cli = parse(&["show", "18c2f0a1b2c3d4e5"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Show {
                thread_id: "18c2f0a1b2c3d4e5".to_owned(),
            }
        );
    }

    #[test]
    fn requires_a_command() {
        let err = parse(&[]).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn parses_sync() {
        let cli = parse(&["sync"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Sync {
                days: 90,
                all: false,
                prefetch: None,
            }
        );

        let cli = parse(&["sync", "--days", "7", "--prefetch", "50"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Sync {
                days: 7,
                all: false,
                prefetch: Some(50),
            }
        );

        let cli = parse(&["sync", "--all"]).unwrap();
        assert!(matches!(cli.command, Command::Sync { all: true, .. }));
    }

    #[test]
    fn sync_rejects_zero_days_and_days_with_all() {
        let err = parse(&["sync", "--days", "0"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
        let err = parse(&["sync", "--days", "7", "--all"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn rejects_unknown_commands() {
        let err = parse(&["frobnicate"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidSubcommand);
    }
}
