//! Openmail desktop binary: wires the UI to the mail service and starts the app.

mod connect;
mod sign_in;

use openmail_ui::{Backend, Connection, SignInError};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

fn main() {
    init_logging();
    gpui_platform::application().run(|cx| {
        openmail_ui::init(cx);
        // Starts the mail service off the main thread; the window shows its
        // progress through the AppModel.
        openmail_ui::AppModel::init(cx, Google);
        if let Err(err) = openmail_ui::open_main_window(cx) {
            eprintln!("openmail: failed to open the main window: {err:#}");
            cx.quit();
        }
    });
}

/// Gmail accounts, signed in with Google OAuth and tokens in the keychain.
struct Google;

impl Backend for Google {
    fn connect(&self) -> Connection {
        connect::connect()
    }

    fn sign_in(&self, cancel: CancellationToken) -> Result<Connection, SignInError> {
        sign_in::sign_in(&cancel)
    }
}

/// Logs to stderr, at `RUST_LOG` or warnings and above.
fn init_logging() {
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::WARN.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
