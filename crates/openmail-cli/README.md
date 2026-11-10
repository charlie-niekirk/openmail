# openmail-cli

A developer CLI for trying Openmail's Gmail client, OAuth and MIME parsing
against a real account, without the desktop UI. It is read-only apart from
signing in and out.

## Setup: bring your own OAuth client

Openmail is not verified by Google yet, so you sign in with an OAuth client from
your own Google Cloud project. This takes a few minutes, once.

1. **Create a project.** Open the [Google Cloud console](https://console.cloud.google.com/)
   and create a project (or pick an existing one).
2. **Enable the Gmail API.** Go to *APIs & Services → Library*, search for
   *Gmail API* and click *Enable*.
3. **Configure the OAuth consent screen.** Go to *APIs & Services → OAuth consent
   screen* (in newer consoles, *Google Auth Platform → Branding / Audience*).
   Choose user type **External**, fill in the app name and your email, and leave
   the publishing status as **Testing**.
4. **Add yourself as a test user.** Under *Audience* (or *Test users*), add the
   Gmail address you will sign in with. In Testing mode only test users can sign
   in, and their sign-ins expire after 7 days; run `login` again when they do.
5. **Create a Desktop client ID.** Go to *APIs & Services → Credentials → Create
   credentials → OAuth client ID*, choose application type **Desktop app**, and
   create it.
6. **Export the client ID and secret** it shows you. Google calls the secret of
   a desktop client a secret, but it is not one in a distributed app; PKCE
   protects the sign-in instead. Keep it out of the repository anyway.

   ```bash
   export OPENMAIL_GOOGLE_CLIENT_ID=1234567890-abc.apps.googleusercontent.com
   export OPENMAIL_GOOGLE_CLIENT_SECRET=GOCSPX-...
   ```

Openmail asks for the `gmail.modify` and `gmail.settings.basic` scopes, plus
`openid email` to learn which account signed in. Google warns that the app is
unverified; continue past the warning, since it is your own client.

## Usage

```bash
cargo run -p openmail-cli -- login
cargo run -p openmail-cli -- labels
cargo run -p openmail-cli -- threads INBOX --limit 10
cargo run -p openmail-cli -- threads Work/Projects --query "is:unread"
cargo run -p openmail-cli -- show <thread-id>
cargo run -p openmail-cli -- sync --days 30
cargo run -p openmail-cli -- logout
```

| Command | What it does |
| --- | --- |
| `login` | Opens Google sign-in in the browser (and prints the URL), then keeps the refresh token in the OS keychain and prints the signed-in address. |
| `logout` | Deletes the account's refresh token from the keychain. |
| `labels` | Lists system labels, then user labels nested under their parents, with message and unread counts and label IDs. |
| `threads <label> [-n N] [-q Q]` | Lists the newest threads in a label: thread ID, date, an unread `*`, senders and message count, subject and snippet. The label is an ID (`INBOX`, `Label_12`) or a name (`Drafts`, `Work/Projects`). `--query` takes [Gmail search syntax](https://support.google.com/mail/answer/7190). |
| `show <thread-id>` | Prints each message's headers, its text body (or text converted from the HTML body) and its attachment names. |
| `sync [--days N \| --all] [--prefetch N]` | Runs the initial sync into the account's local store: labels, Inbox threads, then other threads, each with mail in the last `N` days (90 by default; `--all` for the whole mailbox), newest first, with a progress line. `--prefetch` also fetches the bodies of the `N` newest Inbox messages. Ctrl-C stops it; running it again continues where it stopped. Once the sync is complete it does nothing (delete `mail.db` to start over). |

`--account <email>` picks an account when you have signed in with several; with
one, it is used by default. Set `RUST_LOG` for logs on stderr, for example
`RUST_LOG=debug` or `RUST_LOG=openmail_cli=debug,reqwest=debug`.

### Where things are kept

- Refresh tokens: the OS keychain (macOS Keychain, Windows Credential Manager,
  Secret Service on Linux) under the service `openmail-google-oauth`.
- The list of signed-in addresses (no tokens), since the keychain cannot list its
  entries: `cli-accounts` in Openmail's config directory, which is
  `~/Library/Application Support/Openmail` on macOS, `~/.config/openmail` on
  Linux, and `%APPDATA%\Openmail\config` on Windows.
- The local mail store written by `sync`: `accounts/<email>/mail.db` in Openmail's
  data directory (`~/Library/Application Support/Openmail` on macOS,
  `~/.local/share/openmail` on Linux, `%APPDATA%\Openmail\data` on Windows), or
  under `OPENMAIL_DATA_DIR` if set.

Access tokens are kept only in memory.
