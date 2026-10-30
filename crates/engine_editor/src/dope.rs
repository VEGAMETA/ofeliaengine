//! The dope sheet's editing of a clip made in the editor (ADR 0124's E12-4c): keys picked by
//! their track's place among the clip's tracks and their own in the track, moved, deleted,
//! copied and pasted, as functions of the clip, which the Animation tab calls and writes as
//! undo steps.

use std::ops::RangeInclusive;

use engine_scene::{AnimFile, AnimKey, KeyCurve};

use crate::shell::{CurveKey, CurveLane};

/// A key: its track's place among the clip's tracks (in their order in the file), and its
/// own in the track.
pub(crate) type KeyAt = (usize, usize);

/// A key copied ([`copy_keys`]): its track's target, how long after the first one copied it
/// lies, its value and curve.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CopiedKey {
    pub(crate) target: String,
    pub(crate) offset: f32,
    pub(crate) value: Vec<f32>,
    pub(crate) curve: KeyCurve,
}

/// Two times of keys are one time.
fn same_time(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-4
}

/// `before` with the keys `picked` moved by `dt` seconds, and where they are now (sorted).
/// They move together, held within the clip (0 to `length`) by its first and last key; a
/// key moved onto another of its track takes its place.
pub(crate) fn move_keys(
    before: &AnimFile,
    picked: &[KeyAt],
    dt: f32,
    length: f32,
) -> (AnimFile, Vec<KeyAt>) {
    let times = before.tracks.values().enumerate().flat_map(|(k, track)| {
        track
            .keys
            .iter()
            .enumerate()
            .filter(move |&(j, _)| picked.contains(&(k, j)))
            .map(|(_, key)| key.time)
    });
    let (first, last) = times.fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), t| {
        (a.min(t), b.max(t))
    });
    let mut out = before.clone();
    let mut now = Vec::with_capacity(picked.len());
    if !first.is_finite() {
        return (out, now);
    }
    let dt = dt.clamp(-first, (length.max(last) - last).max(0.0));
    for (k, track) in out.tracks.values_mut().enumerate() {
        if !picked.iter().any(|p| p.0 == k) {
            continue;
        }
        let mut moved: Vec<AnimKey> = Vec::new();
        let mut kept: Vec<AnimKey> = Vec::new();
        for (j, key) in track.keys.drain(..).enumerate() {
            if picked.contains(&(k, j)) {
                moved.push(AnimKey {
                    time: key.time + dt,
                    ..key
                });
            } else {
                kept.push(key);
            }
        }
        kept.retain(|s| !moved.iter().any(|m| same_time(m.time, s.time)));
        kept.extend(moved.iter().cloned());
        kept.sort_by(|a, b| a.time.total_cmp(&b.time));
        for m in &moved {
            if let Some(j) = kept.iter().position(|x| x.time == m.time) {
                now.push((k, j));
            }
        }
        track.keys = kept;
    }
    now.sort_unstable();
    (out, now)
}

/// `anim` without the keys `picked`; a track left without keys goes too. How many keys went.
pub(crate) fn delete_keys(anim: &mut AnimFile, picked: &[KeyAt]) -> usize {
    let mut gone = 0;
    for (k, track) in anim.tracks.values_mut().enumerate() {
        let mut j = 0;
        track.keys.retain(|_| {
            let keep = !picked.contains(&(k, j));
            gone += usize::from(!keep);
            j += 1;
            keep
        });
    }
    anim.tracks.retain(|_, track| !track.keys.is_empty());
    gone
}

/// The keys `picked`, copied in time order: each one's target, time after the first one's,
/// value and curve.
pub(crate) fn copy_keys(anim: &AnimFile, picked: &[KeyAt]) -> Vec<CopiedKey> {
    let mut out: Vec<(f32, CopiedKey)> = Vec::new();
    for (k, (target, track)) in anim.tracks.iter().enumerate() {
        for (j, key) in track.keys.iter().enumerate() {
            if picked.contains(&(k, j)) {
                out.push((
                    key.time,
                    CopiedKey {
                        target: target.clone(),
                        offset: 0.0,
                        value: key.value.clone(),
                        curve: key.curve,
                    },
                ));
            }
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    let first = out.first().map_or(0.0, |o| o.0);
    out.into_iter()
        .map(|(time, key)| CopiedKey {
            offset: time - first,
            ..key
        })
        .collect()
}

/// `copied` pasted from `time` on (each its own offset after it) into the tracks of their
/// targets (made where missing), over keys at their times; where they are now (sorted). The
/// clip grows to hold them.
pub(crate) fn paste_keys(anim: &mut AnimFile, copied: &[CopiedKey], time: f32) -> Vec<KeyAt> {
    for c in copied {
        let t = (time + c.offset).max(0.0);
        anim.set_key(&c.target, t, &c.value);
        if let Some(key) = anim
            .tracks
            .get_mut(&c.target)
            .and_then(|track| track.keys.iter_mut().find(|k| same_time(k.time, t)))
        {
            key.curve = c.curve;
        }
    }
    let mut now: Vec<KeyAt> = copied
        .iter()
        .filter_map(|c| {
            let t = (time + c.offset).max(0.0);
            let k = anim.tracks.keys().position(|target| *target == c.target)?;
            let j = anim.key_at(&c.target, t)?;
            Some((k, j))
        })
        .collect();
    now.sort_unstable();
    now.dedup();
    now
}

/// The keys on tracks `tracks` from time `t0` to `t1` (either way round), added to `out`
/// (kept sorted, each once).
pub(crate) fn keys_in(
    anim: &AnimFile,
    tracks: RangeInclusive<usize>,
    t0: f32,
    t1: f32,
    out: &mut Vec<KeyAt>,
) {
    let (lo, hi) = (t0.min(t1), t0.max(t1));
    for (k, track) in anim.tracks.values().enumerate() {
        if !tracks.contains(&k) {
            continue;
        }
        for (j, key) in track.keys.iter().enumerate() {
            if key.time >= lo - 1e-4 && key.time <= hi + 1e-4 && !out.contains(&(k, j)) {
                out.push((k, j));
            }
        }
    }
    out.sort_unstable();
}

/// The Curves view of `anim` (ADR 0124's E12-4d): each lane of the tracks `shown` says as a
/// line in `lanes` through `points`, its keys as points in `keys`, all as fractions of the
/// view (time along it to the clip's length, value down from the top of the range). The
/// range of values drawn is `range` where given (held while a key is dragged), else the
/// shown values' with a margin; it is returned. A smooth stretch is drawn through samples
/// about 4 pixels apart for a view `width` wide, a linear one straight, a step flat to the
/// next key and up. `curves` are the tracks laid out ([`engine_app::TrackCurve`]); `times`
/// a list kept between frames (nothing allocated once the lists have room).
#[allow(clippy::too_many_arguments)]
pub(crate) fn curve_lines(
    anim: &AnimFile,
    curves: &[engine_app::TrackCurve],
    shown: impl Fn(usize) -> bool,
    width: f32,
    range: Option<[f32; 2]>,
    times: &mut Vec<f32>,
    lanes: &mut Vec<CurveLane>,
    points: &mut Vec<[f32; 2]>,
    keys: &mut Vec<CurveKey>,
) -> [f32; 2] {
    lanes.clear();
    points.clear();
    keys.clear();
    let length = anim.length.max(1e-6);
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    let mut value = [0.0f32; 4];
    for (k, (track, curve)) in anim.tracks.values().zip(curves).enumerate() {
        if !shown(k) || curve.lanes() == 0 {
            continue;
        }
        // The moments its lines pass through.
        times.clear();
        if track.keys.first().is_some_and(|key| key.time > 0.0) {
            times.push(0.0);
        }
        for (i, key) in track.keys.iter().enumerate() {
            times.push(key.time);
            let Some(next) = track.keys.get(i + 1) else {
                continue;
            };
            let span = next.time - key.time;
            match key.curve {
                KeyCurve::Step => times.push(next.time - (span * 0.5).min(1e-4)),
                KeyCurve::Linear => {}
                KeyCurve::Smooth => {
                    let m = ((span / length * width / 4.0) as usize).clamp(2, 24);
                    times.extend((1..m).map(|s| key.time + span * s as f32 / m as f32));
                }
            }
        }
        if track.keys.last().is_some_and(|key| key.time < length) {
            times.push(length);
        }
        for lane in 0..curve.lanes() {
            let from = points.len() as u32;
            for &t in times.iter() {
                curve.sample(t, &mut value);
                points.push([t, value[lane]]);
                lo = lo.min(value[lane]);
                hi = hi.max(value[lane]);
            }
            lanes.push(CurveLane {
                track: k,
                lane,
                points: [from, points.len() as u32],
            });
            for (j, key) in track.keys.iter().enumerate() {
                if let Some(&v) = key.value.get(lane) {
                    keys.push(CurveKey {
                        track: k,
                        key: j,
                        lane,
                        at: [key.time, v],
                    });
                }
            }
        }
    }
    let [lo, hi] = range.unwrap_or_else(|| {
        if !lo.is_finite() {
            [0.0, 1.0]
        } else if hi - lo < 1e-6 {
            [lo - 1.0, hi + 1.0]
        } else {
            let margin = (hi - lo) * 0.1;
            [lo - margin, hi + margin]
        }
    });
    let span = (hi - lo).max(1e-6);
    let place = |[t, v]: [f32; 2]| [(t / length).clamp(0.0, 1.0), 1.0 - (v - lo) / span];
    for p in points.iter_mut() {
        *p = place(*p);
    }
    for key in keys.iter_mut() {
        key.at = place(key.at);
    }
    [lo, hi]
}

/// The curve the keys `picked` share, if they share one.
pub(crate) fn shared_curve(anim: &AnimFile, picked: &[KeyAt]) -> Option<KeyCurve> {
    let mut curves = anim.tracks.values().enumerate().flat_map(|(k, track)| {
        track
            .keys
            .iter()
            .enumerate()
            .filter(move |&(j, _)| picked.contains(&(k, j)))
            .map(|(_, key)| key.curve)
    });
    let first = curves.next()?;
    curves.all(|c| c == first).then_some(first)
}

/// The keys `picked` given curve `curve` (how the clip goes from each to the next). How many
/// changed.
pub(crate) fn set_curves(anim: &mut AnimFile, picked: &[KeyAt], curve: KeyCurve) -> usize {
    let mut changed = 0;
    for (k, track) in anim.tracks.values_mut().enumerate() {
        for (j, key) in track.keys.iter_mut().enumerate() {
            if picked.contains(&(k, j)) && key.curve != curve {
                key.curve = curve;
                changed += 1;
            }
        }
    }
    changed
}

/// The picked keys that `anim` still has (after undo or another edit changed it).
pub(crate) fn still_there(anim: &AnimFile, picked: &mut Vec<KeyAt>) {
    picked.retain(|&(k, j)| {
        anim.tracks
            .values()
            .nth(k)
            .is_some_and(|track| j < track.keys.len())
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    /// A door's clip: its turn keyed at 0, 0.5 and 1 s, its lamp at 0.25 s.
    fn door() -> AnimFile {
        let mut anim = AnimFile::default();
        anim.set_key("Transform.rotation", 0.0, &[0.0, 0.0, 0.0]);
        anim.set_key("Transform.rotation", 0.5, &[0.0, 45.0, 0.0]);
        anim.set_key("Transform.rotation", 1.0, &[0.0, 90.0, 0.0]);
        anim.set_key("Lamp:PointLight.range", 0.25, &[4.0]);
        anim
    }

    fn times(anim: &AnimFile, target: &str) -> Vec<f32> {
        anim.tracks[target].keys.iter().map(|k| k.time).collect()
    }

    /// The tracks in the file's order: the lamp's first ("L" before "T").
    #[test]
    fn keys_move_together_held_in_the_clip_and_take_the_place_of_keys_they_land_on() {
        let anim = door();
        // The turn's middle key and the lamp's key a frame and a half later.
        let (moved, now) = move_keys(&anim, &[(0, 0), (1, 1)], 0.05, 1.0);
        assert_eq!(times(&moved, "Lamp:PointLight.range"), [0.3]);
        assert_eq!(times(&moved, "Transform.rotation"), [0.0, 0.55, 1.0]);
        assert_eq!(now, [(0, 0), (1, 1)]);
        // Onto the last key: it takes its place, its value goes with it.
        let (moved, now) = move_keys(&anim, &[(1, 1)], 0.5, 1.0);
        assert_eq!(times(&moved, "Transform.rotation"), [0.0, 1.0]);
        assert_eq!(
            moved.tracks["Transform.rotation"].keys[1].value,
            [0.0, 45.0, 0.0]
        );
        assert_eq!(now, [(1, 1)]);
        // Past the start: held there, the others keeping their distance.
        let (moved, _) = move_keys(&anim, &[(1, 1), (1, 2)], -2.0, 1.0);
        assert_eq!(times(&moved, "Transform.rotation"), [0.0, 0.5]);
        assert_eq!(
            moved.tracks["Transform.rotation"].keys[0].value,
            [0.0, 45.0, 0.0]
        );
        // Past the end: held at it.
        let (moved, _) = move_keys(&anim, &[(0, 0)], 3.0, 1.0);
        assert_eq!(times(&moved, "Lamp:PointLight.range"), [1.0]);
    }

    #[test]
    fn keys_deleted_take_an_empty_track_with_them() {
        let mut anim = door();
        assert_eq!(delete_keys(&mut anim, &[(1, 1)]), 1);
        assert_eq!(times(&anim, "Transform.rotation"), [0.0, 1.0]);
        assert_eq!(delete_keys(&mut anim, &[(0, 0)]), 1);
        assert!(
            !anim.tracks.contains_key("Lamp:PointLight.range"),
            "its last key gone"
        );
        assert_eq!(anim.tracks.len(), 1);
    }

    #[test]
    fn keys_copied_paste_at_the_playhead_keeping_their_distances() {
        let mut anim = door();
        anim.tracks.get_mut("Transform.rotation").unwrap().keys[2].curve = KeyCurve::Step;
        let copied = copy_keys(&anim, &[(0, 0), (1, 2)]);
        assert_eq!(copied.len(), 2);
        assert_eq!((copied[0].offset, copied[1].offset), (0.0, 0.75));
        assert_eq!(copied[1].curve, KeyCurve::Step);
        // Pasted at 0.5: the lamp's at 0.5, the turn's at 1.25 (the clip grows).
        let now = paste_keys(&mut anim, &copied, 0.5);
        assert_eq!(times(&anim, "Lamp:PointLight.range"), [0.25, 0.5]);
        assert_eq!(times(&anim, "Transform.rotation"), [0.0, 0.5, 1.0, 1.25]);
        assert_eq!(anim.length, 1.25);
        assert_eq!(
            anim.tracks["Transform.rotation"].keys[3].curve,
            KeyCurve::Step
        );
        assert_eq!(now, [(0, 1), (1, 3)]);
        // Into a clip without the track: made.
        let mut other = AnimFile::default();
        let now = paste_keys(&mut other, &copied, 0.0);
        assert_eq!(other.tracks.len(), 2);
        assert_eq!(now, [(0, 0), (1, 0)]);
    }

    /// The Curves view: a lane per number of the shown tracks, a step flat to the next key
    /// and up, a linear stretch straight, a smooth one through samples; the keys' points on
    /// their lanes; the range the values' with a margin, or the one given.
    #[test]
    fn curves_follow_each_keys_curve_and_fit_the_values() {
        let mut anim = door();
        let rotation = anim.tracks.get_mut("Transform.rotation").unwrap();
        rotation.keys[0].curve = KeyCurve::Step;
        rotation.keys[1].curve = KeyCurve::Linear;
        let curves: Vec<engine_app::TrackCurve> = anim
            .tracks
            .values()
            .map(engine_app::TrackCurve::new)
            .collect();
        let (mut times, mut lanes, mut points, mut keys) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        // The turn alone (track 1): three lanes, its yaw from 0 to 90 degrees.
        let range = curve_lines(
            &anim,
            &curves,
            |k| k == 1,
            400.0,
            None,
            &mut times,
            &mut lanes,
            &mut points,
            &mut keys,
        );
        assert_eq!(lanes.len(), 3);
        assert_eq!(range, [-9.0, 99.0], "0 to 90 and a tenth more each way");
        let yaw = lanes[1];
        let line = &points[yaw.points[0] as usize..yaw.points[1] as usize];
        let value = |p: [f32; 2]| 99.0 - p[1] * 108.0;
        // Flat at 0 until just before 0.5 s, then 45 there, straight to 90 at 1 s.
        assert_eq!(line.len(), 4, "{line:?}");
        assert!(value(line[1]).abs() < 1e-3 && line[1][0] < 0.5);
        assert!((value(line[2]) - 45.0).abs() < 1e-3 && line[2][0] == 0.5);
        assert!((value(line[3]) - 90.0).abs() < 1e-3 && line[3][0] == 1.0);
        let yaw_keys: Vec<[f32; 2]> = keys.iter().filter(|k| k.lane == 1).map(|k| k.at).collect();
        assert_eq!(yaw_keys.len(), 3);
        assert!((value(yaw_keys[1]) - 45.0).abs() < 1e-3);
        // A smooth stretch samples between its keys; a range given is kept.
        anim.tracks.get_mut("Transform.rotation").unwrap().keys[0].curve = KeyCurve::Smooth;
        let curves: Vec<engine_app::TrackCurve> = anim
            .tracks
            .values()
            .map(engine_app::TrackCurve::new)
            .collect();
        let range = curve_lines(
            &anim,
            &curves,
            |k| k == 1,
            400.0,
            Some([0.0, 180.0]),
            &mut times,
            &mut lanes,
            &mut points,
            &mut keys,
        );
        assert_eq!(range, [0.0, 180.0]);
        let yaw = lanes[1];
        assert!(
            yaw.points[1] - yaw.points[0] > 4,
            "samples between 0 and 0.5 s"
        );
        // The picked keys' curve: shared, or none when they differ.
        assert_eq!(shared_curve(&anim, &[(1, 1)]), Some(KeyCurve::Linear));
        assert_eq!(shared_curve(&anim, &[(1, 0), (1, 1)]), None);
        assert_eq!(set_curves(&mut anim, &[(1, 0), (1, 1)], KeyCurve::Step), 2);
        assert_eq!(shared_curve(&anim, &[(1, 0), (1, 1)]), Some(KeyCurve::Step));
    }

    #[test]
    fn a_box_picks_the_keys_inside_it() {
        let anim = door();
        let mut picked = Vec::new();
        keys_in(&anim, 1..=1, 0.6, 0.2, &mut picked);
        assert_eq!(picked, [(1, 1)]);
        keys_in(&anim, 0..=1, 0.0, 0.3, &mut picked);
        assert_eq!(picked, [(0, 0), (1, 0), (1, 1)], "added, each once");
        let mut gone = vec![(1, 2), (1, 5), (4, 0)];
        still_there(&anim, &mut gone);
        assert_eq!(gone, [(1, 2)]);
    }
}
