//! Openmail desktop binary: wires the UI to the mail service and starts the app.

fn main() {
    gpui_platform::application().run(|cx| {
        openmail_ui::init(cx);
        if let Err(err) = openmail_ui::open_main_window(cx) {
            eprintln!("openmail: failed to open the main window: {err:#}");
            cx.quit();
        }
    });
}
