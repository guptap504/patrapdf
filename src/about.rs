//! What this app is and who made it, taken from Cargo.toml so the About box,
//! the packages and the binary always agree.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
/// Reverse-DNS id: the macOS bundle id, the Linux desktop file / Wayland app id.
pub const APP_ID: &str = "io.github.guptap504.PdfSigner";

/// The authors' names, without email addresses.
pub fn author() -> String {
    env!("CARGO_PKG_AUTHORS")
        .split(':')
        .map(|a| a.split('<').next().unwrap_or(a).trim())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn metadata_is_filled_in() {
        assert!(!super::VERSION.is_empty());
        assert!(!super::DESCRIPTION.is_empty());
        assert!(super::REPOSITORY.starts_with("https://github.com/"));
        let author = super::author();
        assert!(!author.is_empty() && !author.contains('<') && !author.contains('@'));
    }
}
