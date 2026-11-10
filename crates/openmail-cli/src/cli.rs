//! Command-line arguments.

use clap::builder::TypedValueParser as _;
use clap::{Args, Parser, Subcommand};
use openmail_gmail::api::{DEFAULT_BATCH_CONCURRENCY, DEFAULT_BATCH_SIZE};

/// Openmail developer CLI: exercise the Gmail client against a real account
/// without the desktop UI.
#[derive(Debug, Parser)]
#[command(name = "openmail-cli", version)]
pub struct Cli {
    /// The account to use, by email address. Defaults to the only signed-in one.
    #[arg(long, global = true, value_name = "EMAIL")]
    pub account: Option<String>,

    /// Your Google Cloud project's Gmail "Units per minute per user" quota
    /// (6,000 by default for new projects; also read from
    /// OPENMAIL_GMAIL_QUOTA_PER_MINUTE). Openmail uses 80% of it.
    #[arg(
        long,
        global = true,
        value_name = "UNITS",
        value_parser = clap::value_parser!(u32).range(60..)
    )]
    pub quota_per_minute: Option<u32>,

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
        /// Calls per Gmail batch request (1 to 50), for tuning throughput.
        #[arg(
            long,
            value_name = "N",
            default_value_t = DEFAULT_BATCH_SIZE,
            value_parser = clap::value_parser!(u16).range(1..=50).map(usize::from)
        )]
        batch_size: usize,
        /// Batch requests in flight at once (1 to 16), for tuning throughput.
        #[arg(
            long,
            value_name = "N",
            default_value_t = DEFAULT_BATCH_CONCURRENCY,
            value_parser = clap::value_parser!(u16).range(1..=16).map(usize::from)
        )]
        batch_concurrency: usize,
        /// Spend Gmail quota at a fixed N units a second (1 to 250), instead
        /// of adapting the rate to Gmail's rate limiting.
        #[arg(
            long,
            value_name = "N",
            value_parser = clap::value_parser!(u32).range(1..=250)
        )]
        rate: Option<u32>,
    },
    /// Keep the local store in sync until Ctrl-C: finish the initial sync if
    /// needed, then poll Gmail's history, printing a line per poll.
    Watch {
        /// Seconds between polls.
        #[arg(
            long,
            value_name = "S",
            default_value_t = 30,
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        interval: u64,
    },
    /// Archive threads (remove them from the Inbox), then send the change.
    Archive(ThreadIds),
    /// Star threads, then send the change.
    Star(ThreadIds),
    /// Unstar threads, then send the change.
    Unstar(ThreadIds),
    /// Mark threads read, then send the change.
    Read(ThreadIds),
    /// Mark threads unread, then send the change.
    Unread(ThreadIds),
    /// Add labels to and remove labels from threads, then send the change.
    Label {
        #[command(flatten)]
        threads: ThreadIds,
        /// A label to add: an ID such as `STARRED` or `Label_12`, or a name
        /// such as `Work/Projects`. Repeatable.
        #[arg(long, value_name = "LABEL")]
        add: Vec<String>,
        /// A label to remove, as for `--add`. Repeatable.
        #[arg(long, value_name = "LABEL")]
        remove: Vec<String>,
    },
}

/// The threads an action applies to.
#[derive(Debug, PartialEq, Eq, Args)]
pub struct ThreadIds {
    /// Thread IDs, as printed by `threads`.
    #[arg(required = true, value_name = "THREAD_ID")]
    pub ids: Vec<String>,
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
                batch_size: DEFAULT_BATCH_SIZE,
                batch_concurrency: DEFAULT_BATCH_CONCURRENCY,
                rate: None,
            }
        );

        let cli = parse(&[
            "sync",
            "--days",
            "7",
            "--prefetch",
            "50",
            "--batch-size",
            "20",
            "--batch-concurrency",
            "3",
            "--rate",
            "120",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Command::Sync {
                days: 7,
                all: false,
                prefetch: Some(50),
                batch_size: 20,
                batch_concurrency: 3,
                rate: Some(120),
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
        for size in ["0", "51"] {
            let err = parse(&["sync", "--batch-size", size]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ValueValidation, "{size}");
        }
        for rate in ["0", "251"] {
            let err = parse(&["sync", "--rate", rate]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ValueValidation, "{rate}");
        }
    }

    #[test]
    fn quota_per_minute_is_global() {
        let cli = parse(&["sync", "--quota-per-minute", "15000"]).unwrap();
        assert_eq!(cli.quota_per_minute, Some(15_000));
        let cli = parse(&["--quota-per-minute", "6000", "watch"]).unwrap();
        assert_eq!(cli.quota_per_minute, Some(6_000));
        assert_eq!(parse(&["watch"]).unwrap().quota_per_minute, None);
        let err = parse(&["watch", "--quota-per-minute", "0"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn parses_watch() {
        let cli = parse(&["watch"]).unwrap();
        assert_eq!(cli.command, Command::Watch { interval: 30 });
        let cli = parse(&["watch", "--interval", "5"]).unwrap();
        assert_eq!(cli.command, Command::Watch { interval: 5 });
        let err = parse(&["watch", "--interval", "0"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn parses_thread_actions() {
        let ids = |ids: &[&str]| ThreadIds {
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        };
        let cli = parse(&["archive", "t1", "t2"]).unwrap();
        assert_eq!(cli.command, Command::Archive(ids(&["t1", "t2"])));
        assert_eq!(
            parse(&["star", "t1"]).unwrap().command,
            Command::Star(ids(&["t1"]))
        );
        assert_eq!(
            parse(&["unstar", "t1"]).unwrap().command,
            Command::Unstar(ids(&["t1"]))
        );
        assert_eq!(
            parse(&["read", "t1"]).unwrap().command,
            Command::Read(ids(&["t1"]))
        );
        assert_eq!(
            parse(&["unread", "t1"]).unwrap().command,
            Command::Unread(ids(&["t1"]))
        );
        let cli = parse(&[
            "label", "t1", "--add", "Work", "--add", "STARRED", "--remove", "INBOX",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Command::Label {
                threads: ids(&["t1"]),
                add: vec!["Work".to_owned(), "STARRED".to_owned()],
                remove: vec!["INBOX".to_owned()],
            }
        );
        let err = parse(&["archive"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn rejects_unknown_commands() {
        let err = parse(&["frobnicate"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidSubcommand);
    }
}
