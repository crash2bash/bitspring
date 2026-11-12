//! Channel layouts and routing: which speaker each channel of a track is for, which speaker
//! each device channel feeds, and how one reaches the other.
//!
//! A track whose speakers the device has all goes out channel for channel, unchanged (still
//! bit-perfect). Otherwise it is folded down to the speakers the device does have, as ITU-R
//! BS.775 does it: the centre and the surrounds join the front pair at −3 dB, the LFE channel
//! is left out, and the whole mix is scaled so that it can never clip.

/// A speaker position, in the order WAVE (and FLAC, and symphonia) store channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Speaker {
    FrontLeft,
    FrontRight,
    FrontCenter,
    Lfe,
    BackLeft,
    BackRight,
    FrontLeftCenter,
    FrontRightCenter,
    BackCenter,
    SideLeft,
    SideRight,
    /// Height speakers and anything else without a place in a 7.1 layout.
    Other,
}

use Speaker::*;

/// WAVE's channel-mask bits, lowest first (bits above these are height speakers).
const MASK_ORDER: [Speaker; 11] =
    [FrontLeft, FrontRight, FrontCenter, Lfe, BackLeft, BackRight, FrontLeftCenter, FrontRightCenter, BackCenter, SideLeft, SideRight];

/// The speakers of a WAVE/FLAC/symphonia channel mask, in stored order.
pub fn from_mask(mask: u64) -> Vec<Speaker> {
    (0..64).filter(|b| mask & (1 << b) != 0).map(|b| MASK_ORDER.get(b).copied().unwrap_or(Other)).collect()
}

/// The usual layout for a file that only says how many channels it has (FLAC's and WAVE's
/// defaults: 3.0, quad, 5.0, 5.1, 6.1, 7.1).
pub fn default_layout(channels: usize) -> Vec<Speaker> {
    let base: &[Speaker] = match channels {
        0 | 1 => &[FrontCenter],
        2 => &[FrontLeft, FrontRight],
        3 => &[FrontLeft, FrontRight, FrontCenter],
        4 => &[FrontLeft, FrontRight, BackLeft, BackRight],
        5 => &[FrontLeft, FrontRight, FrontCenter, BackLeft, BackRight],
        6 => &[FrontLeft, FrontRight, FrontCenter, Lfe, BackLeft, BackRight],
        7 => &[FrontLeft, FrontRight, FrontCenter, Lfe, BackCenter, SideLeft, SideRight],
        _ => &[FrontLeft, FrontRight, FrontCenter, Lfe, BackLeft, BackRight, SideLeft, SideRight],
    };
    let mut v = base.to_vec();
    v.resize(channels.max(1), Other);
    v
}

/// "Mono", "Stereo", "5.1", "7.1"…: how a layout is usually called.
pub fn name(layout: &[Speaker]) -> String {
    match layout {
        [FrontCenter] => return "Mono".into(),
        [FrontLeft, FrontRight] => return "Stereo".into(),
        _ => {}
    }
    let lfe = layout.iter().filter(|&&s| s == Lfe).count();
    let other = layout.iter().filter(|&&s| s == Other).count();
    if other > 0 {
        return format!("{} ch", layout.len());
    }
    format!("{}.{lfe}", layout.len() - lfe)
}

/// How the channels of a track reach the device's channels.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// Channels of the "program": what the processing chain works on. The track's own
    /// channels, or the speakers it is folded down to.
    program: usize,
    /// Folding the track down: `program × track` gains, row by row. None = the program is
    /// the track itself.
    mix: Option<Box<[f32]>>,
    track_channels: usize,
    /// Which program channel each device channel plays (None: silent).
    map: Box<[Option<u16>]>,
    /// The fold-down's overall scaling, in dB (0 when nothing is folded).
    pub gain_db: f64,
    /// The layout the program has.
    pub program_layout: Vec<Speaker>,
}

const MINUS_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

impl Route {
    /// The route for a track laid out as `track` to a device whose channels feed `device`
    /// (one entry per device channel).
    pub fn new(track: &[Speaker], device: &[Speaker]) -> Route {
        let n = track.len().max(1);
        let device_n = device.len().max(1);
        // Mono and stereo play as they always have: mono on the first two channels, stereo on
        // the first pair (the device's own speaker setup may say otherwise, but this is what
        // every DAC expects).
        if n <= 2 {
            let map = (0..device_n).map(|dc| if n == 1 { (dc < 2).then_some(0) } else { (dc < 2).then_some(dc as u16) }).collect();
            return Route { program: n, mix: None, track_channels: n, map, gain_db: 0.0, program_layout: track.to_vec() };
        }
        // Every speaker of the track is on the device: channel for channel. (A back pair
        // plays on side speakers and the other way round: in 5.1 they are the same thing.)
        let mut used = vec![false; device.len()];
        let mut placed: Vec<usize> = Vec::with_capacity(n);
        for &s in track {
            let spot = (0..device.len())
                .find(|&d| !used[d] && device[d] == s)
                .or_else(|| (0..device.len()).find(|&d| !used[d] && Some(device[d]) == twin(s)));
            match spot {
                Some(d) => {
                    used[d] = true;
                    placed.push(d);
                }
                None => break,
            }
        }
        // Channels the file doesn't place (discrete ones): in order, when the device has enough.
        if !s_all_known(track) && device.len() >= n {
            let map = (0..device_n).map(|d| (d < n).then_some(d as u16)).collect();
            return Route { program: n, mix: None, track_channels: n, map, gain_db: 0.0, program_layout: track.to_vec() };
        }
        if placed.len() == n && s_all_known(track) {
            let mut map = vec![None; device_n];
            for (c, &d) in placed.iter().enumerate() {
                map[d] = Some(c as u16);
            }
            return Route { program: n, mix: None, track_channels: n, map: map.into(), gain_db: 0.0, program_layout: track.to_vec() };
        }
        // Fold down onto the device's speakers (the first 8 channels at most; a device that
        // doesn't say where its speakers are is taken as a stereo pair).
        let targets: Vec<Speaker> = device.iter().take(8).copied().collect();
        let targets = if targets.contains(&FrontLeft) && targets.contains(&FrontRight) {
            targets
        } else {
            vec![FrontLeft, FrontRight]
        };
        let p = targets.len();
        let mut gains = vec![0f32; p * n];
        for (c, &s) in track.iter().enumerate() {
            for (to, g) in fold(s, &targets) {
                if let Some(pc) = targets.iter().position(|&t| t == to) {
                    gains[pc * n + c] += g;
                }
            }
        }
        // Never louder than full scale: the busiest output channel sums to at most 1.
        let worst = (0..p).map(|pc| gains[pc * n..(pc + 1) * n].iter().map(|g| g.abs()).sum::<f32>()).fold(0f32, f32::max);
        let scale = if worst > 1.0 { 1.0 / worst } else { 1.0 };
        gains.iter_mut().for_each(|g| *g *= scale);
        let mut map = vec![None; device_n];
        if device[..device.len().min(8)].contains(&FrontLeft) && device[..device.len().min(8)].contains(&FrontRight) {
            for (pc, &t) in targets.iter().enumerate() {
                if let Some(d) = device.iter().position(|&s| s == t) {
                    map[d] = Some(pc as u16);
                }
            }
        } else {
            for (pc, m) in map.iter_mut().take(2).enumerate() {
                *m = Some(pc as u16);
            }
        }
        Route {
            program: p,
            mix: Some(gains.into()),
            track_channels: n,
            map: map.into(),
            gain_db: 20.0 * (scale as f64).log10(),
            program_layout: targets,
        }
    }

    /// The program's channel count (what the processing chain works on).
    pub fn program_channels(&self) -> usize {
        self.program
    }

    /// The track is folded down (so not bit-perfect).
    pub fn folds(&self) -> bool {
        self.mix.is_some()
    }

    /// Every channel of the track reaches a device channel of its own.
    pub fn is_direct(&self) -> bool {
        self.mix.is_none() && (self.track_channels <= 2 || (0..self.track_channels).all(|c| self.map.contains(&Some(c as u16))))
    }

    /// The program channel device channel `dc` plays.
    #[inline(always)]
    pub fn program_channel(&self, dc: usize) -> Option<usize> {
        self.map.get(dc).copied().flatten().map(usize::from)
    }

    /// Program channel `pc` of one track frame, from `sample(track_channel)`.
    #[inline(always)]
    pub fn program_sample(&self, pc: usize, sample: impl Fn(usize) -> f32) -> f32 {
        match &self.mix {
            None => sample(pc),
            Some(g) => {
                let n = self.track_channels;
                let row = &g[pc * n..(pc + 1) * n];
                let mut acc = 0f32;
                for (c, &k) in row.iter().enumerate() {
                    if k != 0.0 {
                        acc += k * sample(c);
                    }
                }
                acc
            }
        }
    }

    /// Fold interleaved track frames into interleaved program frames (`out` is replaced).
    pub fn fold_block(&self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        let n = self.track_channels;
        for frame in input.chunks_exact(n) {
            for pc in 0..self.program {
                out.push(self.program_sample(pc, |c| frame[c]));
            }
        }
    }
}

/// The other name of a surround speaker: 5.1 calls its surrounds "back" or "side" depending
/// on who wrote the file.
fn twin(s: Speaker) -> Option<Speaker> {
    match s {
        BackLeft => Some(SideLeft),
        BackRight => Some(SideRight),
        SideLeft => Some(BackLeft),
        SideRight => Some(BackRight),
        _ => None,
    }
}

fn s_all_known(track: &[Speaker]) -> bool {
    track.iter().all(|&s| s != Other)
}

/// Where speaker `s` goes when the device has only `targets`, and how loud (before scaling).
fn fold(s: Speaker, targets: &[Speaker]) -> Vec<(Speaker, f32)> {
    let has = |t: Speaker| targets.contains(&t);
    if has(s) {
        return vec![(s, 1.0)];
    }
    if let Some(t) = twin(s).filter(|&t| has(t)) {
        return vec![(t, 1.0)];
    }
    let pair = |l: Speaker, r: Speaker, g: f32| vec![(l, g), (r, g)];
    match s {
        // ITU-R BS.775: the LFE channel isn't part of the downmix.
        Lfe => vec![],
        FrontCenter => pair(FrontLeft, FrontRight, MINUS_3DB),
        FrontLeftCenter if has(FrontCenter) => vec![(FrontLeft, MINUS_3DB), (FrontCenter, MINUS_3DB)],
        FrontRightCenter if has(FrontCenter) => vec![(FrontRight, MINUS_3DB), (FrontCenter, MINUS_3DB)],
        FrontLeftCenter => vec![(FrontLeft, 1.0)],
        FrontRightCenter => vec![(FrontRight, 1.0)],
        BackLeft | SideLeft => vec![(FrontLeft, MINUS_3DB)],
        BackRight | SideRight => vec![(FrontRight, MINUS_3DB)],
        BackCenter if has(BackLeft) && has(BackRight) => pair(BackLeft, BackRight, MINUS_3DB),
        BackCenter if has(SideLeft) && has(SideRight) => pair(SideLeft, SideRight, MINUS_3DB),
        BackCenter | Other => pair(FrontLeft, FrontRight, 0.5),
        FrontLeft | FrontRight => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn layouts_have_their_usual_names() {
        assert_eq!(name(&default_layout(1)), "Mono");
        assert_eq!(name(&default_layout(2)), "Stereo");
        assert_eq!(name(&default_layout(6)), "5.1");
        assert_eq!(name(&default_layout(8)), "7.1");
        assert_eq!(name(&default_layout(4)), "4.0");
        assert_eq!(name(&default_layout(10)), "10 ch");
        assert_eq!(from_mask(0x3F), default_layout(6));
        assert_eq!(from_mask(0x60F), vec![FrontLeft, FrontRight, FrontCenter, Lfe, SideLeft, SideRight]);
    }

    #[test]
    fn stereo_and_mono_play_as_before() {
        let r = Route::new(&default_layout(2), &default_layout(8));
        assert!(r.is_direct() && !r.folds());
        assert_eq!((0..8).map(|d| r.program_channel(d)).collect::<Vec<_>>(), [Some(0), Some(1), None, None, None, None, None, None]);
        let m = Route::new(&default_layout(1), &default_layout(2));
        assert_eq!((m.program_channel(0), m.program_channel(1)), (Some(0), Some(0)));
    }

    #[test]
    fn five_one_goes_channel_for_channel_where_the_speakers_are() {
        // A 7.1 receiver with its surrounds called "side": 5.1's back pair goes there.
        let device = [FrontLeft, FrontRight, FrontCenter, Lfe, SideLeft, SideRight, BackLeft, BackRight];
        let r = Route::new(&default_layout(6), &device);
        assert!(r.is_direct() && !r.folds());
        assert_eq!(r.program_channels(), 6);
        let map: Vec<_> = (0..8).map(|d| r.program_channel(d)).collect();
        assert_eq!(map, [Some(0), Some(1), Some(2), Some(3), None, None, Some(4), Some(5)]);
        // A device that orders its channels its own way (CoreAudio's L R Ls Rs C LFE).
        let device = [FrontLeft, FrontRight, SideLeft, SideRight, FrontCenter, Lfe];
        let r = Route::new(&default_layout(6), &device);
        let map: Vec<_> = (0..6).map(|d| r.program_channel(d)).collect();
        assert_eq!(map, [Some(0), Some(1), Some(4), Some(5), Some(2), Some(3)]);
    }

    #[test]
    fn five_one_folds_to_stereo_without_clipping() {
        let r = Route::new(&default_layout(6), &default_layout(2));
        assert!(r.folds() && !r.is_direct());
        assert_eq!(r.program_channels(), 2);
        // L = (FL + 0.707 C + 0.707 BL) / 2.414, no LFE.
        let frame = [1.0f32, 0.0, 1.0, 1.0, 1.0, 0.0];
        let l = r.program_sample(0, |c| frame[c]);
        let rr = r.program_sample(1, |c| frame[c]);
        assert!(close(l, 1.0), "{l}");
        assert!(close(rr, MINUS_3DB / (1.0 + 2.0 * MINUS_3DB)), "{rr}");
        assert!((r.gain_db + 7.66).abs() < 0.01, "{}", r.gain_db);
        // Full-scale everywhere (even LFE) never goes past full scale.
        let loud = [1.0f32; 6];
        assert!(r.program_sample(0, |c| loud[c]) <= 1.0 + 1e-6);
        let mut out = Vec::new();
        r.fold_block(&[frame, frame].concat(), &mut out);
        assert_eq!(out.len(), 4);
        assert!(close(out[2], 1.0));
    }

    #[test]
    fn seven_one_folds_onto_a_five_one_device() {
        let device = default_layout(6);
        let r = Route::new(&default_layout(8), &device);
        assert!(r.folds());
        assert_eq!(r.program_channels(), 6);
        // The side pair has no speakers of its own here: it joins the back pair.
        let only = |k: usize| move |c: usize| if c == k { 1.0 } else { 0.0 };
        let sl = 6; // FL FR FC LFE BL BR SL SR
        let bl_pc = r.program_layout.iter().position(|&s| s == BackLeft).unwrap();
        assert!(r.program_sample(bl_pc, only(sl)) > 0.0);
        assert!(r.program_sample(bl_pc, only(4)) > 0.0);
        assert_eq!(r.program_sample(0, only(sl)), 0.0);
        // The LFE keeps its own speaker.
        let lfe_pc = r.program_layout.iter().position(|&s| s == Lfe).unwrap();
        assert!(r.program_sample(lfe_pc, only(3)) > 0.0);
    }

    #[test]
    fn discrete_channels_go_in_order() {
        let track = [Other; 4];
        let r = Route::new(&track, &default_layout(8));
        assert!(r.is_direct());
        assert_eq!((0..8).map(|d| r.program_channel(d)).collect::<Vec<_>>(), [Some(0), Some(1), Some(2), Some(3), None, None, None, None]);
        assert!(Route::new(&track, &default_layout(2)).folds());
    }

    #[test]
    fn an_unlabelled_stereo_device_gets_the_fold_on_its_first_pair() {
        let r = Route::new(&default_layout(6), &[Other, Other]);
        assert!(r.folds());
        assert_eq!((r.program_channel(0), r.program_channel(1)), (Some(0), Some(1)));
    }
}
