//! A finished PDF on the clipboard, to paste into an email, a chat or a
//! folder. What other apps take from the clipboard is a file, so the copy is
//! kept in a folder of the app's own: one at a time, the newest.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::App;

use crate::signatures::app_data_dir;

/// How long a copy left by an earlier run is kept: it may still be on the clipboard.
const KEPT_FOR: Duration = Duration::from_secs(24 * 60 * 60);

fn dir() -> PathBuf {
    app_data_dir().join("clipboard")
}

/// Keep `bytes` as the file called `name`, in place of the copy kept before.
/// Only the user can read it.
pub fn keep(name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let dir = dir();
    private_dir(&dir).with_context(|| format!("couldn't create {}", dir.display()))?;
    for old in std::fs::read_dir(&dir)?.flatten() {
        let _ = std::fs::remove_file(old.path());
    }
    // Just the name, whatever it was given: the copy stays in this folder.
    let path = dir.join(Path::new(name).file_name().context("the copy has no name")?);
    write_private(&path, bytes).with_context(|| format!("couldn't write {}", path.display()))?;
    Ok(path)
}

/// Remove copies left by earlier runs, once they are a day old.
pub fn tidy() {
    let Ok(copies) = std::fs::read_dir(dir()) else { return };
    for copy in copies.flatten() {
        let age = copy.metadata().and_then(|m| m.modified()).ok().and_then(|at| at.elapsed().ok());
        if age.is_some_and(|age| age > KEPT_FOR) {
            let _ = std::fs::remove_file(copy.path());
        }
    }
}

#[cfg(unix)]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(bytes)
}

// On Windows the app's data folder is inside the user's profile, which is theirs alone.
#[cfg(not(unix))]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// The system clipboard. Kept for as long as the app runs: on Linux, what is
/// copied is handed out by the app that copied it.
#[cfg(not(test))]
struct System(arboard::Clipboard);

#[cfg(not(test))]
impl gpui::Global for System {}

/// Put the file at `path` on the clipboard, as copying it in a file manager does.
#[cfg(not(test))]
pub fn put(path: &Path, cx: &mut App) -> Result<()> {
    if !cx.has_global::<System>() {
        cx.set_global(System(arboard::Clipboard::new()?));
    }
    put_on(&mut cx.global_mut::<System>().0, path)
}

fn put_on(clipboard: &mut arboard::Clipboard, path: &Path) -> Result<()> {
    Ok(clipboard.set().file_list(&[path])?)
}

/// Tests must never touch the user's real clipboard: they note what would have been put there.
#[cfg(test)]
pub fn put(path: &Path, _: &mut App) -> Result<()> {
    tests::PUT.with(|put| *put.borrow_mut() = Some(path.to_path_buf()));
    Ok(())
}

/// What the test on this thread last put on the clipboard.
#[cfg(test)]
pub fn last_put() -> Option<PathBuf> {
    tests::PUT.with(|put| put.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        pub(super) static PUT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    }

    #[test]
    fn one_copy_is_kept_and_old_ones_are_cleared_out() {
        let first = keep("form-signed.pdf", b"one").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(&first), mode(first.parent().unwrap())), (0o600, 0o700), "for the user alone");
        }

        // The next copy takes its place, wherever its name says it would like to go.
        let second = keep("../../elsewhere/other.pdf", b"two").unwrap();
        assert_eq!(second, dir().join("other.pdf"));
        assert!(!first.exists());
        let again = keep("other.pdf", b"three").unwrap();
        assert_eq!(std::fs::read(&again).unwrap(), b"three");

        // A copy is left for a day, in case it is still on the clipboard.
        tidy();
        assert!(again.exists());
        let long_ago = std::time::SystemTime::now() - 2 * KEPT_FOR;
        std::fs::File::options().write(true).open(&again).unwrap().set_modified(long_ago).unwrap();
        tidy();
        assert!(!again.exists());
    }

    /// Run by hand (`cargo test -- --ignored`): it replaces what is on the clipboard.
    #[test]
    #[ignore = "writes to the real clipboard"]
    fn a_file_goes_onto_the_system_clipboard() {
        let path = keep("clipboard-test.pdf", b"%PDF-1.4\n").unwrap();
        let mut clipboard = arboard::Clipboard::new().unwrap();
        put_on(&mut clipboard, &path).unwrap();
        assert_eq!(clipboard.get().file_list().unwrap(), vec![path.canonicalize().unwrap()]);
    }
}
