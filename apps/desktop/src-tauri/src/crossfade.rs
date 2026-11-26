//! Crossfades between tracks (Settings → Playback): which joins get one.
//!
//! Off unless the user turns it on. Then the end of a track fades out under the start of
//! the next one — except where an album simply goes on: the next track of the same album
//! joins gapless, as it was made (a live set, a symphony, a mix that runs on without a
//! break). So in a playlist or a shuffled queue every change of album crossfades, and an
//! album played through stays as it is.
//!
//! What the engine itself leaves out (see `Playback::queue_next_crossfading`): DSD sent as
//! it is (DoP or native), tracks at different rates (the device would have to switch),
//! network players.

use hifi_engine::decode::TrackMeta;

/// The lengths the setting offers, in seconds.
pub const LENGTHS: [u32; 5] = [2, 4, 6, 8, 12];

/// The setting as the UI sends it: seconds, 0 = off. Anything else is taken to the nearest
/// length offered.
pub fn secs_from(asked: f64) -> u32 {
    if !(asked > 0.0) {
        return 0;
    }
    LENGTHS.iter().copied().min_by(|a, b| (*a as f64 - asked).abs().total_cmp(&(*b as f64 - asked).abs())).unwrap_or(0)
}

/// Whether `next` is the track after `prev` on the same album, so the two join gapless even
/// with crossfades on. The same album: the same album title and the same album artist (or
/// artist, when neither says), compared without case. The track after: the next number on
/// the same disc, or the first track of the next disc. Without track numbers to go by, any
/// two tracks of the same album count as going on: a crossfade in the middle of an album is
/// the worse mistake.
pub fn album_goes_on(prev: &TrackMeta, next: &TrackMeta) -> bool {
    fn same(a: &Option<String>, b: &Option<String>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => a.trim().to_lowercase() == b.trim().to_lowercase(),
            _ => false,
        }
    }
    let (Some(album), Some(_)) = (&prev.album, &next.album) else { return false };
    if album.trim().is_empty() || !same(&prev.album, &next.album) {
        return false;
    }
    let artist = |m: &TrackMeta| m.album_artist.clone().or_else(|| m.artist.clone());
    let (pa, na) = (artist(prev), artist(next));
    // (One of them untagged: nothing says they differ.)
    if pa.is_some() && na.is_some() && !same(&pa, &na) {
        return false;
    }
    match (prev.track_number, next.track_number) {
        (Some(p), Some(n)) => {
            let (pd, nd) = (prev.disc_number.unwrap_or(1), next.disc_number.unwrap_or(1));
            (nd == pd && n == p + 1) || (nd == pd + 1 && n == 1)
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(album: &str, artist: &str, n: Option<u64>, disc: Option<u64>) -> TrackMeta {
        TrackMeta {
            album: (!album.is_empty()).then(|| album.to_string()),
            album_artist: (!artist.is_empty()).then(|| artist.to_string()),
            track_number: n,
            disc_number: disc,
            ..Default::default()
        }
    }

    #[test]
    fn the_next_track_of_an_album_goes_on_without_a_crossfade() {
        assert!(album_goes_on(&track("Kind of Blue", "Miles Davis", Some(1), None), &track("Kind of Blue", "Miles Davis", Some(2), None)));
        // Case and spaces don't make another album.
        assert!(album_goes_on(&track("Kind of Blue ", "Miles Davis", Some(1), None), &track("kind of blue", "MILES DAVIS", Some(2), None)));
        // On to the next disc.
        assert!(album_goes_on(&track("Ring", "Solti", Some(9), Some(1)), &track("Ring", "Solti", Some(1), Some(2))));
    }

    #[test]
    fn anything_else_crossfades() {
        // Another album.
        assert!(!album_goes_on(&track("A", "X", Some(1), None), &track("B", "X", Some(2), None)));
        // Same title, another artist ("Greatest Hits").
        assert!(!album_goes_on(&track("Greatest Hits", "X", Some(1), None), &track("Greatest Hits", "Y", Some(2), None)));
        // The same album out of order (shuffled), or the same track again (repeat one).
        assert!(!album_goes_on(&track("A", "X", Some(5), None), &track("A", "X", Some(2), None)));
        assert!(!album_goes_on(&track("A", "X", Some(5), None), &track("A", "X", Some(5), None)));
        // No album to go by.
        assert!(!album_goes_on(&track("", "X", Some(1), None), &track("", "X", Some(2), None)));
    }

    #[test]
    fn an_album_without_track_numbers_goes_on() {
        assert!(album_goes_on(&track("A", "X", None, None), &track("A", "X", None, None)));
        assert!(album_goes_on(&track("A", "", Some(1), None), &track("A", "X", Some(2), None)));
    }

    #[test]
    fn the_setting_takes_the_nearest_length_offered() {
        assert_eq!(secs_from(0.0), 0);
        assert_eq!(secs_from(-3.0), 0);
        assert_eq!(secs_from(f64::NAN), 0);
        assert_eq!(secs_from(4.0), 4);
        assert_eq!(secs_from(5.1), 6);
        assert_eq!(secs_from(60.0), 12);
    }
}
