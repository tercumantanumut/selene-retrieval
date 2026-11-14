//! Where the installed app keeps its parts. Windows (and dev builds) keep everything in one
//! install folder next to the exes; a macOS app bundle splits it so code signing can seal it:
//! programs in `Contents/MacOS`, the PDF library in `Contents/Frameworks`, data (the bundled
//! model) in `Contents/Resources`.

use std::path::{Path, PathBuf};

/// Folder with the app's data files (the bundled model): `app_dir` itself, or the bundle's
/// `Contents/Resources` when `app_dir` is a macOS bundle's `Contents/MacOS`.
pub fn resources_dir(app_dir: &Path) -> PathBuf {
    bundle_part(app_dir, "Resources")
}

/// Folder with the shared libraries the app loads by absolute path (pdfium): `app_dir` itself,
/// or the bundle's `Contents/Frameworks` on macOS.
pub fn libraries_dir(app_dir: &Path) -> PathBuf {
    bundle_part(app_dir, "Frameworks")
}

fn bundle_part(app_dir: &Path, part: &str) -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(contents) = app_dir.parent()
        && app_dir.file_name().is_some_and(|n| n == "MacOS")
        && contents.file_name().is_some_and(|n| n == "Contents")
    {
        return contents.join(part);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = part;
    app_dir.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn bundle_layout_on_macos() {
        let mac = Path::new("/Applications/Selene Retrieval.app/Contents/MacOS");
        assert_eq!(
            resources_dir(mac),
            Path::new("/Applications/Selene Retrieval.app/Contents/Resources")
        );
        assert_eq!(
            libraries_dir(mac),
            Path::new("/Applications/Selene Retrieval.app/Contents/Frameworks")
        );
        // A plain folder (dev build, target/debug) keeps everything together.
        let dev = Path::new("/Users/me/src/target/debug");
        assert_eq!(resources_dir(dev), dev);
        assert_eq!(libraries_dir(dev), dev);
        assert_eq!(resources_dir(Path::new("/x/MacOS")), Path::new("/x/MacOS"));
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn one_folder_elsewhere() {
        let d = Path::new("/opt/sr/Contents/MacOS");
        assert_eq!(resources_dir(d), d);
        assert_eq!(libraries_dir(d), d);
    }
}
