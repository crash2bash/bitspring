//! Keeping access to the music folders the user picked, across launches (macOS).
//!
//! The Mac App Store build runs in the App Sandbox: a folder chosen in the open panel is
//! readable only until the app quits. A security-scoped bookmark, made while that access is
//! live and saved with the library, opens it again on the next launch without asking. The
//! website build isn't sandboxed; there the bookmark still works and simply grants nothing new
//! (it also follows a folder that was moved or renamed).
//!
//! Elsewhere there is nothing to keep: iPhone and Android use fixed folders, Windows has no
//! sandbox.

#[cfg(target_os = "macos")]
mod imp {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSData, NSString, NSURL, NSURLBookmarkCreationOptions, NSURLBookmarkResolutionOptions};

    /// A bookmark for `path`, base64 for the JSON index. None if the system refuses (the path
    /// is gone, or the sandbox gave us no access to it).
    pub fn create(path: &str) -> Option<String> {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        let options = NSURLBookmarkCreationOptions::WithSecurityScope | NSURLBookmarkCreationOptions::SecurityScopeAllowOnlyReadAccess;
        let data = url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(options, None, None).ok()?;
        Some(STANDARD.encode(data.to_vec()))
    }

    /// Open a saved bookmark and start reading through it, for the rest of the run. Returns the
    /// folder's path now (it may have been moved) and, when the system says the bookmark went
    /// stale, a fresh one to save instead.
    pub fn open(saved: &str) -> Option<(String, Option<String>)> {
        let bytes = STANDARD.decode(saved).ok()?;
        let data = NSData::with_bytes(&bytes);
        let mut stale = Bool::NO;
        // SAFETY: `stale` outlives the call, which only writes a BOOL through it.
        let url = unsafe {
            NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                &data,
                NSURLBookmarkResolutionOptions::WithSecurityScope | NSURLBookmarkResolutionOptions::WithoutUI,
                None,
                &mut stale,
            )
        }
        .ok()?;
        // SAFETY: plain Foundation call on a valid URL. Access stays on until the app quits; a
        // folder removed from the library is simply no longer read.
        if !unsafe { url.startAccessingSecurityScopedResource() } {
            return None;
        }
        let path = url.path()?.to_string();
        let fresh = if stale.as_bool() { create(&path) } else { None };
        Some((path, fresh))
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn create(_path: &str) -> Option<String> {
        None
    }

    pub fn open(_saved: &str) -> Option<(String, Option<String>)> {
        None
    }
}

pub use imp::{create, open};

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn a_folder_comes_back_through_its_bookmark_even_after_a_move() {
        let base = std::env::temp_dir().join(format!("bitspring-bookmark-{}", std::process::id()));
        let before = base.join("Music");
        let after = base.join("Moved Music");
        std::fs::create_dir_all(&before).unwrap();
        let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap().to_string_lossy().into_owned();

        let saved = super::create(&before.to_string_lossy()).expect("bookmark made");
        let (path, _) = super::open(&saved).expect("bookmark opens");
        assert_eq!(canon(std::path::Path::new(&path)), canon(&before));

        std::fs::rename(&before, &after).unwrap();
        let (path, _) = super::open(&saved).expect("bookmark follows the move");
        assert_eq!(canon(std::path::Path::new(&path)), canon(&after));

        assert!(super::open("not a bookmark").is_none());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
