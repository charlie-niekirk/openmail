//! Openmail desktop binary: wires the UI to the mail service and starts the app.

fn main() {
    println!("{}", version_line());
}

/// The one-line version banner, e.g. `openmail 0.1.0`.
fn version_line() -> String {
    format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::version_line;

    #[test]
    fn version_line_has_name_and_version() {
        assert_eq!(
            version_line(),
            concat!("openmail ", env!("CARGO_PKG_VERSION"))
        );
    }
}
