//! Which headphones a Bluetooth output is, as far as its name tells: so the app can offer the
//! correction profiles we have for them. Only offered, never switched on.
//!
//! A Bluetooth device names itself ("WH-1000XM4", "Alex's AirPods Pro", "LE_WH-1000XM5"), and
//! a profile is named after the headphones ("Sony WH-1000XM4"). The match is on words: a run of
//! the profile's words after its brand that the device's name has too. A missed hint costs
//! nothing, a silly one makes the app look broken, so the rule leans hard towards saying
//! nothing: a run must carry a word long and particular enough to name a model, not a kind of
//! product ("Pro 2", "Wireless Headphones" name nothing).
//!
//! The answer is the model as the profile writes it, not one profile: two generations often
//! share a Bluetooth name and each model has several measurements, so the UI opens the
//! headphone picker with it typed in and the listener chooses.

use std::ops::Range;

/// Words that say what kind of thing it is, not which one: they don't make a run particular,
/// and a run doesn't start or end on one (the hint is "Momentum 4", not "Momentum 4 Wireless";
/// a brand's second word, as in "Campfire Audio", isn't taken for the model).
const GENERIC: &[&str] = &[
    "wireless", "wired", "bluetooth", "headphones", "headphone", "headset", "earbuds", "earbud", "earphones", "earphone", "buds", "ear", "studio", "elite",
    "sport", "sports", "stereo", "audio", "sound", "music", "classic", "classics", "edition", "series", "active", "noise", "cancelling", "canceling", "gaming",
    "monitor", "monitors", "speaker", "speakers", "true", "hands", "free",
];

fn generic(word: &str) -> bool {
    GENERIC.contains(&word)
}

/// The words of `s`, lower case, each with where it is in `s`: anything that is not a letter or
/// a digit separates them ("LE_WH-1000XM5" → `le`, `wh`, `1000xm5`).
fn words(s: &str) -> Vec<(String, Range<usize>)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() {
            start.get_or_insert(i);
        } else if let Some(st) = start.take() {
            out.push((s[st..i].to_lowercase(), st..i));
        }
    }
    if let Some(st) = start {
        out.push((s[st..].to_lowercase(), st..s.len()));
    }
    out
}

/// A profile's name without the note at its end: "Apple AirPods (3rd generation)" → "Apple
/// AirPods", "Beyerdynamic DT 770 Pro (80 Ohm)" → "Beyerdynamic DT 770 Pro". The note tells
/// measurements apart; a device never names itself that way.
fn without_note(name: &str) -> &str {
    let name = name.trim();
    match name.strip_suffix(')').and_then(|n| n.rfind('(')) {
        Some(open) => name[..open].trim_end(),
        None => name,
    }
}

/// Does the run of profile words `run` name a model? Long enough and particular enough (see the
/// module's comment); `whole` says it is every word after the brand.
fn names_a_model(run: &[(String, Range<usize>)], whole: bool) -> bool {
    let (Some(first), Some(last)) = (run.first(), run.last()) else { return false };
    if generic(&first.0) || generic(&last.0) {
        return false;
    }
    if run.len() >= 2 {
        run.iter().any(|(w, _)| w.chars().count() >= 5 && !generic(w))
    } else {
        // One word is enough only when it is the whole model and a long one ("AirPods",
        // "Maxwell"); "Clear" or "Aria" alone could be anything.
        whole && first.0.chars().count() >= 6
    }
}

/// The model in `device_name` we have profiles for, as written in the profile's name
/// (e.g. "AirPods Pro", "WH-1000XM4"), to search the profiles by; None when unsure.
///
/// A candidate is a run of a profile's words, not counting its brand, that is also a run of the
/// device's words and [names a model](names_a_model). The brand is the name's first word as
/// spaces separate them, so "Audio-Technica" is all brand. The longest run wins (most words;
/// on a tie, the first profile's).
pub fn suggest<'a>(device_name: &str, profile_names: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let device: Vec<String> = words(device_name).into_iter().map(|(w, _)| w).collect();
    if device.is_empty() {
        return None;
    }
    let mut best: Option<(usize, String)> = None;
    for full in profile_names {
        let name = without_note(full);
        let brand_end = name.find(char::is_whitespace).unwrap_or(name.len());
        let model: Vec<_> = words(name).into_iter().filter(|(_, at)| at.start >= brand_end).collect();
        for i in 0..model.len() {
            for j in 0..device.len() {
                // How far the profile's words from `i` and the device's from `j` go together.
                let common = model[i..].iter().zip(&device[j..]).take_while(|((p, _), d)| p == *d).count();
                // Only runs longer than the best so far can win, and the longest one that names
                // a model is the one to take from here.
                let shortest = best.as_ref().map_or(1, |(n, _)| n + 1);
                if let Some(len) = (shortest..=common).rev().find(|&len| names_a_model(&model[i..i + len], len == model.len())) {
                    let at = model[i].1.start..model[i + len - 1].1.end;
                    best = Some((len, name[at].to_string()));
                }
            }
        }
    }
    best.map(|(_, piece)| piece)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list like the built-in one, with the near misses the rule must not fall for.
    const PROFILES: &[&str] = &[
        "Apple AirPods (3rd generation)",
        "Apple AirPods Max",
        "Apple AirPods Pro",
        "Apple AirPods Pro 2",
        "Audio-Technica ATH-M50x",
        "Audeze Maxwell",
        "Beats Studio Buds",
        "Bose QuietComfort 45",
        "Bose SoundLink Around-Ear II",
        "Campfire Audio Andromeda",
        "Focal Clear",
        "Moondrop Aria",
        "Samsung Galaxy Buds2 Pro",
        "Sennheiser Momentum 4 Wireless",
        "Sony WF-1000XM4",
        "Sony WH-1000XM4",
        "Sony WH-1000XM5",
    ];

    fn hint(device: &str) -> Option<String> {
        suggest(device, PROFILES.iter().copied())
    }

    /// The headphone picker's search, as `HeadphonePicker.svelte` does it: every word typed is
    /// somewhere in the name with its spaces and punctuation dropped ("hd600" finds "HD 600").
    fn picker_finds(query: &str, name: &str) -> bool {
        let compact: String = words(name).into_iter().map(|(w, _)| w).collect();
        let typed = words(query);
        !typed.is_empty() && typed.iter().all(|(w, _)| compact.contains(w.as_str()))
    }

    #[test]
    fn words_split_on_anything_but_letters_and_digits() {
        let w = |s: &str| words(s).into_iter().map(|(w, _)| w).collect::<Vec<_>>();
        assert_eq!(w("WH-1000XM4"), ["wh", "1000xm4"]);
        assert_eq!(w("Alex's AirPods Pro"), ["alex", "s", "airpods", "pro"]);
        assert_eq!(w("Alex’s AirPods Pro"), ["alex", "s", "airpods", "pro"]);
        assert_eq!(w("LE_WH-1000XM5"), ["le", "wh", "1000xm5"]);
        assert_eq!(w("  "), Vec::<String>::new());
        assert_eq!(without_note("Apple AirPods (3rd generation)"), "Apple AirPods");
        assert_eq!(without_note("Audeze LCD-2 (Fazor)"), "Audeze LCD-2");
        assert_eq!(without_note("Sony WH-1000XM4"), "Sony WH-1000XM4");
    }

    #[test]
    fn known_headphones_are_hinted_as_the_profile_writes_them() {
        for (device, want) in [
            ("Alex's AirPods Pro", "AirPods Pro"),
            // Not "AirPods" (3rd generation): the longer run wins.
            ("AirPods Max", "AirPods Max"),
            ("AirPods", "AirPods"),
            ("Alex’s AirPods", "AirPods"),
            ("AirPods Pro 2", "AirPods Pro 2"),
            ("WH-1000XM4", "WH-1000XM4"),
            ("LE_WH-1000XM4", "WH-1000XM4"),
            ("WH-1000XM5", "WH-1000XM5"),
            ("WF-1000XM4", "WF-1000XM4"),
            ("Headphones (WH-1000XM4 Stereo)", "WH-1000XM4"),
            ("MOMENTUM 4", "Momentum 4"),
            ("Galaxy Buds2 Pro (4F2A)", "Galaxy Buds2 Pro"),
            ("Bose QuietComfort 45", "QuietComfort 45"),
            ("Maxwell", "Maxwell"),
        ] {
            assert_eq!(hint(device).as_deref(), Some(want), "{device}");
        }
    }

    #[test]
    fn unsure_names_get_no_hint() {
        for device in [
            // "Pro 2" is shared, and no word of it is long enough to name a model.
            "Pixel Buds Pro 2",
            // One word of a longer model.
            "Bose SoundLink Mini",
            "JBL Flip 6",
            "FiiO Q7",
            "MacBook Pro Speakers",
            "Wireless Headphones",
            // A model the list doesn't have, though it starts like one it has.
            "ATH-M50xBT2",
            "Audio-Technica ATH-M50xBT2",
            // The brand's second word isn't the model.
            "Campfire Audio",
            // Short one-word models could be anything.
            "Clear",
            "Aria",
            "",
            "---",
        ] {
            assert_eq!(hint(device), None, "{device}");
        }
    }

    /// Precision over recall: "Studio" is generic and "Buds" too short, so the very name of a
    /// profile can go without a hint. Accepted: a missed hint costs nothing.
    #[test]
    fn some_real_models_are_missed_on_purpose() {
        assert_eq!(hint("Beats Studio Buds"), None);
    }

    #[test]
    fn a_tie_goes_to_the_first_profile_and_an_empty_list_says_nothing() {
        assert_eq!(suggest("Sony WH-1000XM4", ["Sony WH-1000XM4 (sample 2)", "Sony WH-1000XM4"]).as_deref(), Some("WH-1000XM4"));
        assert_eq!(suggest("Momentum 4", ["Sennheiser MOMENTUM 4", "Sennheiser Momentum 4 Wireless"]).as_deref(), Some("MOMENTUM 4"));
        assert_eq!(suggest("AirPods Pro", std::iter::empty()), None);
        // A name of one word is all brand: nothing to match.
        assert_eq!(suggest("Sony", ["Sony"]), None);
    }

    /// What the hint says, the headphone picker finds: the profile it came from, and no more
    /// than the models that share those words.
    #[test]
    fn the_picker_finds_what_is_hinted() {
        for (device, profile) in [
            ("WH-1000XM4", "Sony WH-1000XM4"),
            ("Alex's AirPods Pro", "Apple AirPods Pro"),
            ("MOMENTUM 4", "Sennheiser Momentum 4 Wireless"),
            ("Galaxy Buds2 Pro (4F2A)", "Samsung Galaxy Buds2 Pro"),
            ("AirPods", "Apple AirPods (3rd generation)"),
        ] {
            let h = hint(device).unwrap();
            assert!(picker_finds(&h, profile), "{h} → {profile}");
        }
        assert!(!picker_finds("WH-1000XM4", "Sony WH-1000XM5"));
        assert!(!picker_finds("WH-1000XM4", "Sony WF-1000XM4"));
        assert!(!picker_finds("AirPods Max", "Apple AirPods Pro"));
    }

    #[test]
    fn the_built_in_profiles() {
        let all: Vec<&str> = hifi_engine::dsp::eq_io::headphone_profiles().iter().map(|p| p.name.as_str()).collect();
        assert!(all.len() > 100);
        for name in &all {
            // Each profile's own name, with and without its brand: a hint, when there is one,
            // finds that very profile in the picker.
            let model = name.split_once(char::is_whitespace).map_or(*name, |(_, m)| m);
            for device in [*name, model] {
                if let Some(h) = suggest(device, all.iter().copied()) {
                    assert!(picker_finds(&h, name), "{device} → {h}");
                }
            }
        }
        let built = |d: &str| suggest(d, all.iter().copied());
        assert_eq!(built("AirPods Pro").as_deref(), Some("AirPods Pro"));
        assert_eq!(built("Alex's AirPods Pro").as_deref(), Some("AirPods Pro"));
        assert_eq!(built("AirPods Max").as_deref(), Some("AirPods Max"));
        assert_eq!(built("WH-1000XM4").as_deref(), Some("WH-1000XM4"));
        assert_eq!(built("LE_WH-1000XM5").as_deref(), Some("WH-1000XM5"));
        assert_eq!(built("MOMENTUM 4").as_deref(), Some("Momentum 4"));
        assert_eq!(built("Galaxy Buds2 Pro (4F2A)").as_deref(), Some("Galaxy Buds2 Pro"));
        for device in ["Pixel Buds Pro 2", "JBL Flip 6", "FiiO Q7", "MacBook Pro Speakers", "ATH-M50xBT2", "Bose SoundLink Mini"] {
            assert_eq!(built(device), None, "{device}");
        }
    }
}
