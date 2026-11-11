//! Openmail desktop binary: wires the UI to the mail service and starts the app.

mod connect;

use tracing_subscriber::EnvFilter;

fn main() {
    init_logging();
    gpui_platform::application().run(|cx| {
        openmail_ui::init(cx);
        // Starts the mail service off the main thread; the window shows its
        // progress through the AppModel.
        openmail_ui::AppModel::init(cx, connect::connect);
        if let Err(err) = openmail_ui::open_main_window(cx) {
            eprintln!("openmail: failed to open the main window: {err:#}");
            cx.quit();
        }
    });
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
