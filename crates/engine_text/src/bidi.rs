//! The Unicode bidirectional algorithm (UAX #9, Unicode 16) over buffers kept between
//! calls: a paragraph's embedding levels per character (rules P2 to I2), a line's levels at
//! its end reset (L1) and its visual order (L2).
//!
//! Adapted from unicode-bidi 0.3.18 (the Servo Project Developers, MIT OR Apache-2.0),
//! whose character data it uses. There every call allocates (7 times for a line of Latin
//! text, 14 to 23 for Arabic or Hebrew ones, measured 2026-10-16); here nothing once the
//! buffers have grown, and text with nothing right to left in it is not processed at all.
//! Levels are kept per character rather than per byte, and an isolating run sequence is a
//! list of runs linked through two arrays rather than a vector of its own.

use std::sync::OnceLock;
use unicode_bidi::BidiClass::{self, *};
use unicode_bidi::{BidiDataSource, HardcodedBidiData, bidi_class};

/// The deepest explicit embedding (BD2).
const MAX_DEPTH: u8 = 125;
/// The most brackets open at once (BD16).
const MAX_BRACKETS: usize = 63;
/// No run.
const NONE: u32 = u32::MAX;

/// The Basic Multilingual Plane's classes, a page of 256 characters filled on its first
/// lookup (5 µs): unicode-bidi's table is searched, which costs about as much as the rest
/// of the algorithm. 64 KB at most.
static PAGES: [OnceLock<[BidiClass; 256]>; 256] = [const { OnceLock::new() }; 256];

/// The class of `c`.
#[inline]
fn class_of(c: char) -> BidiClass {
    let u = c as u32;
    if u >= 0x1_0000 {
        return bidi_class(c);
    }
    let page = PAGES[(u >> 8) as usize].get_or_init(|| {
        let base = u & !0xFF;
        core::array::from_fn(|k| char::from_u32(base + k as u32).map_or(L, bidi_class))
    });
    page[(u & 0xFF) as usize]
}

/// Removed by rule X9: kept, as `BN`, for the rules after it.
fn removed(class: BidiClass) -> bool {
    matches!(class, RLE | LRE | RLO | LRO | PDF | BN)
}

/// A neutral or isolate formatting character (NI).
fn neutral(class: BidiClass) -> bool {
    matches!(class, B | S | WS | ON | FSI | LRI | RLI | PDI)
}

/// The direction of a level (`L` for even ones, `R` for odd).
fn direction(level: u8) -> BidiClass {
    if level.is_multiple_of(2) { L } else { R }
}

/// Whether `c` can make text other than all left to right: below the Hebrew block nothing
/// is right to left, an Arabic number or explicit formatting.
#[inline]
fn significant(c: char) -> bool {
    (c as u32) >= 0x0590
        && matches!(
            class_of(c),
            R | AL | AN | RLE | LRE | RLO | LRO | PDF | RLI | LRI | FSI | PDI
        )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Override {
    Neutral,
    Ltr,
    Rtl,
    Isolate,
}

/// A bracket pair (BD16): its characters and their runs.
#[derive(Debug, Clone, Copy)]
struct Pair {
    open: u32,
    close: u32,
    open_run: u32,
    close_run: u32,
}

/// The algorithm's buffers (see the module documentation).
#[derive(Debug, Default)]
pub(crate) struct Bidi {
    /// Each character's byte offset in the text.
    offsets: Vec<u32>,
    /// Each character.
    chars: Vec<char>,
    /// Each character's class, FSIs resolved to LRI or RLI (P2, P3).
    original: Vec<BidiClass>,
    /// The classes as the rules change them.
    classes: Vec<BidiClass>,
    /// Each character's level.
    levels: Vec<u8>,
    /// Level runs (BD7) as ranges of characters.
    runs: Vec<(u32, u32)>,
    /// The run after and before each run in its isolating run sequence.
    next: Vec<u32>,
    prev: Vec<u32>,
    /// Isolating run sequences (BD13): their first runs.
    sequences: Vec<u32>,
    /// The directional status stack (X1).
    stack: Vec<(u8, Override)>,
    /// Open isolates (P2) or open sequences' last runs (X10).
    open: Vec<u32>,
    /// Scratch lists of characters (W5's runs, N1's runs).
    marks: Vec<u32>,
    bns: Vec<u32>,
    brackets: Vec<(char, u32, u32)>,
    pairs: Vec<Pair>,
    /// The first paragraph's level.
    level: u8,
    /// Whether the levels were resolved; if not, every character is at `level` (0).
    mixed: bool,
}

impl Bidi {
    /// Resolves the levels of `text`: its paragraph's level is `base` (0 left to right, 1
    /// right to left) or, without one, its first strong character's (P2, P3). A paragraph
    /// separator in it ends a paragraph (P1), the next one finding its own level.
    pub(crate) fn resolve(&mut self, text: &str, base: Option<u8>) {
        self.offsets.clear();
        self.chars.clear();
        self.original.clear();
        self.classes.clear();
        self.levels.clear();
        self.runs.clear();
        self.sequences.clear();
        self.level = base.unwrap_or(0);
        self.mixed = base.is_some_and(|b| b % 2 == 1) || text.chars().any(significant);
        if !self.mixed {
            return;
        }
        for (i, c) in text.char_indices() {
            self.offsets.push(i as u32);
            self.chars.push(c);
            self.original.push(class_of(c));
        }
        let n = self.chars.len();
        self.classes.extend_from_slice(&self.original);
        self.levels.resize(n, 0);
        let mut start = 0;
        let mut first = true;
        while start < n {
            let end = self.original[start..]
                .iter()
                .position(|&c| c == B)
                .map_or(n, |k| start + k + 1);
            let level = self.paragraph(start, end, base);
            if first {
                self.level = level;
                first = false;
            }
            start = end;
        }
    }

    /// The first paragraph's level.
    pub(crate) fn level(&self) -> u8 {
        self.level
    }

    /// Whether anything is right to left (else every level is the paragraph's).
    pub(crate) fn mixed(&self) -> bool {
        self.mixed
    }

    /// The level of the character at byte `offset` (one of its bytes), from the character
    /// `hint` on (characters are looked up in order, so a walk costs one step each).
    pub(crate) fn level_at(&self, offset: u32, hint: &mut usize) -> u8 {
        if !self.mixed {
            return self.level;
        }
        let mut k = (*hint).min(self.offsets.len().saturating_sub(1));
        while k > 0 && self.offsets[k] > offset {
            k -= 1;
        }
        while k + 1 < self.offsets.len() && self.offsets[k + 1] <= offset {
            k += 1;
        }
        *hint = k;
        self.levels.get(k).copied().unwrap_or(self.level)
    }

    /// The levels of the characters, after [`Self::resolve`] when [`Self::mixed`].
    #[cfg(test)]
    pub(crate) fn levels(&self) -> &[u8] {
        &self.levels
    }

    /// Applies L1 to the characters whose bytes lie in `bytes` (a line of the paragraph at
    /// level `para`): whitespace and isolate formatting at its end and before segment and
    /// paragraph separators back to the paragraph's level; removed characters take the
    /// level before them.
    pub(crate) fn reset_line(&mut self, bytes: core::ops::Range<u32>, para: u8) {
        if !self.mixed {
            return;
        }
        let start = self.offsets.partition_point(|&o| o < bytes.start);
        let end = self.offsets.partition_point(|&o| o < bytes.end);
        let mut reset_from = Some(start);
        let mut reset_to = None;
        let mut prev = para;
        for i in start..end {
            match self.original[i] {
                B | S => {
                    reset_to = Some(i + 1);
                    if reset_from.is_none() {
                        reset_from = Some(i);
                    }
                }
                WS | FSI | LRI | RLI | PDI => {
                    if reset_from.is_none() {
                        reset_from = Some(i);
                    }
                }
                RLE | LRE | RLO | LRO | PDF | BN => {
                    if reset_from.is_none() {
                        reset_from = Some(i);
                    }
                    self.levels[i] = prev;
                }
                _ => reset_from = None,
            }
            if let (Some(from), Some(to)) = (reset_from, reset_to) {
                self.levels[from..to].fill(para);
                reset_from = None;
                reset_to = None;
            }
            prev = self.levels[i];
        }
        if let Some(from) = reset_from {
            self.levels[from..end].fill(para);
        }
    }

    /// One paragraph, characters `start..end`: its level (P2, P3, FSIs resolved), explicit
    /// levels and runs (X1 to X10), weak and neutral types (W1 to N2), implicit levels (I1,
    /// I2). Returns its level.
    fn paragraph(&mut self, start: usize, end: usize, base: Option<u8>) -> u8 {
        let mut level = base;
        let mut pure = true;
        let mut isolates = false;
        self.open.clear();
        for i in start..end {
            match self.original[i] {
                class @ (L | R | AL) => {
                    if class != L {
                        pure = false;
                    }
                    match self.open.last() {
                        Some(&k) => {
                            if self.original[k as usize] == FSI {
                                self.original[k as usize] = if class == L { LRI } else { RLI };
                            }
                        }
                        None => {
                            if level.is_none() {
                                level = Some(u8::from(class != L));
                            }
                        }
                    }
                }
                AN | LRE | RLE | LRO | RLO => pure = false,
                RLI | LRI | FSI => {
                    pure = false;
                    isolates = true;
                    self.open.push(i as u32);
                }
                PDI => {
                    self.open.pop();
                }
                _ => {}
            }
        }
        let level = level.unwrap_or(0);
        self.levels[start..end].fill(level);
        self.classes[start..end].copy_from_slice(&self.original[start..end]);
        if level == 0 && pure {
            return level;
        }
        let first_run = self.runs.len();
        let first_sequence = self.sequences.len();
        self.explicit(start, end, level);
        self.isolating(first_run, isolates);
        for s in first_sequence..self.sequences.len() {
            let head = self.sequences[s];
            let (sos, eos) = self.bounds(head, start, end, level);
            self.weak(head, sos, eos);
            self.neutral(head, sos, eos);
        }
        for i in start..end {
            let odd = self.levels[i] % 2 == 1;
            match (odd, self.classes[i]) {
                (false, AN | EN) => self.levels[i] += 2,
                (false, R) | (true, L | EN | AN) => self.levels[i] += 1,
                _ => {}
            }
        }
        for i in start..end {
            if removed(self.original[i]) {
                self.levels[i] = if i > start { self.levels[i - 1] } else { level };
            }
        }
        level
    }

    /// X1 to X8: explicit levels and directions; BD7: the level runs.
    fn explicit(&mut self, start: usize, end: usize, para: u8) {
        self.stack.clear();
        self.stack.push((para, Override::Neutral));
        let mut overflow_isolates = 0u32;
        let mut overflow_embeddings = 0u32;
        let mut valid_isolates = 0u32;
        let mut run_start = start;
        let mut run_level = para;
        for i in start..end {
            let (top_level, top_status) = self.top();
            let class = self.original[i];
            match class {
                RLE | LRE | RLO | LRO | RLI | LRI | FSI => {
                    self.levels[i] = top_level;
                    let isolate = matches!(class, RLI | LRI | FSI);
                    if isolate {
                        match top_status {
                            Override::Rtl => self.classes[i] = R,
                            Override::Ltr => self.classes[i] = L,
                            _ => {}
                        }
                    }
                    // An FSI still unresolved has nothing strong in it: left to right.
                    let rtl = matches!(class, RLE | RLO | RLI);
                    let next = if rtl {
                        (top_level + 1) | 1
                    } else {
                        (top_level + 2) & !1
                    };
                    if next <= MAX_DEPTH && overflow_isolates == 0 && overflow_embeddings == 0 {
                        let status = match class {
                            RLO => Override::Rtl,
                            LRO => Override::Ltr,
                            RLI | LRI | FSI => Override::Isolate,
                            _ => Override::Neutral,
                        };
                        self.stack.push((next, status));
                        if isolate {
                            valid_isolates += 1;
                        } else {
                            self.levels[i] = next;
                        }
                    } else if isolate {
                        overflow_isolates += 1;
                    } else if overflow_isolates == 0 {
                        overflow_embeddings += 1;
                    }
                    if !isolate {
                        self.classes[i] = BN;
                    }
                }
                PDI => {
                    if overflow_isolates > 0 {
                        overflow_isolates -= 1;
                    } else if valid_isolates > 0 {
                        overflow_embeddings = 0;
                        while let Some((_, status)) = self.stack.pop() {
                            if status == Override::Isolate {
                                break;
                            }
                        }
                        valid_isolates -= 1;
                    }
                    let (level, status) = self.top();
                    self.levels[i] = level;
                    match status {
                        Override::Rtl => self.classes[i] = R,
                        Override::Ltr => self.classes[i] = L,
                        _ => {}
                    }
                }
                PDF => {
                    if overflow_isolates > 0 {
                    } else if overflow_embeddings > 0 {
                        overflow_embeddings -= 1;
                    } else if top_status != Override::Isolate && self.stack.len() >= 2 {
                        self.stack.pop();
                    }
                    self.levels[i] = self.top().0;
                    self.classes[i] = BN;
                }
                B => {}
                _ => {
                    self.levels[i] = top_level;
                    if class != BN {
                        match top_status {
                            Override::Rtl => self.classes[i] = R,
                            Override::Ltr => self.classes[i] = L,
                            _ => {}
                        }
                    }
                }
            }
            if i == start {
                run_level = self.levels[i];
            } else if !removed(class) && self.levels[i] != run_level {
                self.runs.push((run_start as u32, i as u32));
                run_level = self.levels[i];
                run_start = i;
            }
        }
        if end > run_start {
            self.runs.push((run_start as u32, end as u32));
        }
    }

    /// The top of the directional status stack (never empty: its first entry is not
    /// popped).
    fn top(&self) -> (u8, Override) {
        self.stack
            .last()
            .copied()
            .unwrap_or((self.level, Override::Neutral))
    }

    /// X10: the paragraph's level runs (from `first_run`) into isolating run sequences.
    fn isolating(&mut self, first_run: usize, isolates: bool) {
        let n = self.runs.len();
        self.next.resize(n, NONE);
        self.prev.resize(n, NONE);
        self.next[first_run..n].fill(NONE);
        self.prev[first_run..n].fill(NONE);
        if !isolates {
            self.sequences.extend(first_run as u32..n as u32);
            return;
        }
        self.open.clear();
        for r in first_run..n {
            let (s, e) = self.runs[r];
            let start_class = self.original[s as usize];
            let end_class = self.original[s as usize..e as usize]
                .iter()
                .copied()
                .rev()
                .find(|&c| !removed(c))
                .unwrap_or(start_class);
            match self.open.last().copied() {
                Some(last) if start_class == PDI => {
                    self.open.pop();
                    self.next[last as usize] = r as u32;
                    self.prev[r] = last;
                }
                _ => self.sequences.push(r as u32),
            }
            if matches!(end_class, RLI | LRI | FSI) {
                self.open.push(r as u32);
            }
        }
    }

    /// A sequence's sos and eos (X10).
    fn bounds(&self, head: u32, start: usize, end: usize, para: u8) -> (BidiClass, BidiClass) {
        let mut last = head;
        while self.next[last as usize] != NONE {
            last = self.next[last as usize];
        }
        let (first_start, _) = self.runs[head as usize];
        let (_, last_end) = self.runs[last as usize];
        let chars = SequenceChars::new(&self.runs, &self.next, head);
        let seq_level = chars
            .clone()
            .find(|&i| !removed(self.original[i as usize]))
            .map_or(self.levels[first_start as usize], |i| {
                self.levels[i as usize]
            });
        let mut end_level = self.levels[last_end as usize - 1];
        let mut last_kept = BN;
        let mut r = head;
        loop {
            let (s, e) = self.runs[r as usize];
            for i in s..e {
                if !removed(self.original[i as usize]) {
                    end_level = self.levels[i as usize];
                    last_kept = self.original[i as usize];
                }
            }
            match self.next[r as usize] {
                NONE => break,
                n => r = n,
            }
        }
        let pred_level = self.original[start..first_start as usize]
            .iter()
            .rposition(|&c| !removed(c))
            .map_or(para, |k| self.levels[start + k]);
        let succ_level = if matches!(last_kept, RLI | LRI | FSI) {
            para
        } else {
            self.original[last_end as usize..end]
                .iter()
                .position(|&c| !removed(c))
                .map_or(para, |k| self.levels[last_end as usize + k])
        };
        (
            direction(seq_level.max(pred_level)),
            direction(end_level.max(succ_level)),
        )
    }

    /// W1 to W7: weak types, in the sequence starting at run `head`.
    fn weak(&mut self, head: u32, sos: BidiClass, eos: BidiClass) {
        let Self {
            runs,
            next,
            prev,
            classes,
            marks: et,
            bns,
            ..
        } = self;
        et.clear();
        bns.clear();
        let mut before_w4 = sos;
        let mut before_w5 = sos;
        let mut before_w1 = sos;
        let mut last_strong_al = false;
        let mut r = head;
        while r != NONE {
            let (s, e) = runs[r as usize];
            for i in s..e {
                let k = i as usize;
                if classes[k] == BN {
                    bns.push(i);
                    continue;
                }
                let mut w2 = classes[k];
                // W1.
                if classes[k] == NSM {
                    classes[k] = match before_w1 {
                        RLI | LRI | FSI | PDI => ON,
                        other => other,
                    };
                    w2 = classes[k];
                }
                before_w1 = classes[k];
                // W2, W3.
                match classes[k] {
                    EN if last_strong_al => classes[k] = AN,
                    AL => classes[k] = R,
                    _ => {}
                }
                match w2 {
                    L | R => last_strong_al = false,
                    AL => last_strong_al = true,
                    _ => {}
                }
                let before_w456 = classes[k];
                match classes[k] {
                    // W5: terminators before a number.
                    EN => {
                        for &j in et.iter() {
                            classes[j as usize] = EN;
                        }
                        et.clear();
                    }
                    // W4 and W6 for separators.
                    ES | CS => {
                        let mut after = SequenceChars::from(runs, next, i + 1, r)
                            .map(|j| classes[j as usize])
                            .find(|&c| !removed(c))
                            .unwrap_or(eos);
                        if after == EN && last_strong_al {
                            after = AN;
                        }
                        classes[k] = match (before_w4, classes[k], after) {
                            (EN, ES | CS, EN) => EN,
                            (AN, CS, AN) => AN,
                            _ => ON,
                        };
                        if classes[k] == ON {
                            for j in SequenceBack::from(runs, prev, i, r) {
                                if classes[j as usize] != BN {
                                    break;
                                }
                                classes[j as usize] = ON;
                            }
                            for j in SequenceChars::from(runs, next, i + 1, r) {
                                if classes[j as usize] != BN {
                                    break;
                                }
                                classes[j as usize] = ON;
                            }
                        }
                    }
                    // W5: terminators after a number.
                    ET => {
                        if before_w5 == EN {
                            classes[k] = EN;
                        } else {
                            et.extend_from_slice(bns);
                            et.push(i);
                        }
                    }
                    _ => {}
                }
                bns.clear();
                before_w5 = classes[k];
                // W6 for terminators.
                if before_w5 != ET {
                    for &j in et.iter() {
                        classes[j as usize] = ON;
                    }
                    et.clear();
                }
                before_w4 = before_w456;
            }
            r = next[r as usize];
        }
        for &j in et.iter() {
            classes[j as usize] = ON;
        }
        et.clear();
        // W7.
        let mut last_strong_l = sos == L;
        for i in SequenceChars::new(runs, next, head) {
            let k = i as usize;
            match classes[k] {
                EN if last_strong_l => classes[k] = L,
                L => last_strong_l = true,
                R | AL => last_strong_l = false,
                _ => {}
            }
        }
    }

    /// N0 to N2: bracket pairs and neutrals, in the sequence starting at run `head`.
    fn neutral(&mut self, head: u32, sos: BidiClass, eos: BidiClass) {
        let Self {
            runs,
            next,
            prev,
            chars,
            original,
            classes,
            levels,
            marks,
            brackets,
            pairs,
            ..
        } = self;
        let e = direction(levels[runs[head as usize].0 as usize]);
        let not_e = if e == L { R } else { L };
        // BD16: the bracket pairs.
        brackets.clear();
        pairs.clear();
        let mut r = head;
        'runs: while r != NONE {
            let (s, end) = runs[r as usize];
            for i in s..end {
                let k = i as usize;
                if classes[k] != ON {
                    continue;
                }
                let Some(matched) = HardcodedBidiData.bidi_matched_opening_bracket(chars[k]) else {
                    continue;
                };
                if matched.is_open {
                    if brackets.len() >= MAX_BRACKETS {
                        break 'runs;
                    }
                    brackets.push((matched.opening, i, r));
                } else if let Some(at) = brackets.iter().rposition(|b| b.0 == matched.opening) {
                    let (_, open, open_run) = brackets[at];
                    pairs.push(Pair {
                        open,
                        close: i,
                        open_run,
                        close_run: r,
                    });
                    brackets.truncate(at);
                }
            }
            r = next[r as usize];
        }
        pairs.sort_unstable_by_key(|p| p.open);
        // N0.
        for pair in pairs.iter() {
            let mut found_e = false;
            let mut found_not_e = false;
            for j in SequenceChars::from(runs, next, pair.open + 1, pair.open_run) {
                if j >= pair.close {
                    break;
                }
                let class = classes[j as usize];
                if class == e {
                    found_e = true;
                } else if class == not_e {
                    found_not_e = true;
                } else if matches!(class, EN | AN) {
                    if e == L {
                        found_not_e = true;
                    } else {
                        found_e = true;
                    }
                }
                if found_e {
                    break;
                }
            }
            let set = if found_e {
                Some(e)
            } else if found_not_e {
                let before = SequenceBack::from(runs, prev, pair.open, pair.open_run)
                    .map(|i| classes[i as usize])
                    .find(|c| matches!(c, L | R | EN | AN))
                    .unwrap_or(sos);
                Some(if matches!(before, EN | AN) { R } else { before })
            } else {
                None
            };
            let Some(set) = set else {
                continue;
            };
            classes[pair.open as usize] = set;
            classes[pair.close as usize] = set;
            for i in SequenceBack::from(runs, prev, pair.open, pair.open_run) {
                if classes[i as usize] != BN {
                    break;
                }
                classes[i as usize] = set;
            }
            for i in SequenceChars::from(runs, next, pair.open + 1, pair.open_run) {
                if original[i as usize] == NSM || classes[i as usize] == BN {
                    classes[i as usize] = set;
                } else {
                    break;
                }
            }
            for i in SequenceChars::from(runs, next, pair.close + 1, pair.close_run) {
                if original[i as usize] == NSM || classes[i as usize] == BN {
                    classes[i as usize] = set;
                } else {
                    break;
                }
            }
        }
        // N1, N2.
        let mut chars_of = SequenceChars::new(runs, next, head);
        let mut before = sos;
        while let Some(mut i) = chars_of.next() {
            let class = classes[i as usize];
            if neutral(class) || class == BN {
                marks.clear();
                marks.push(i);
                let after;
                loop {
                    match chars_of.next() {
                        Some(j) => {
                            i = j;
                            let c = classes[j as usize];
                            if neutral(c) || c == BN {
                                marks.push(j);
                            } else {
                                after = c;
                                break;
                            }
                        }
                        None => {
                            after = eos;
                            break;
                        }
                    }
                }
                let set = match (before, after) {
                    (L, L) => L,
                    (R | AN | EN, R | AN | EN) => R,
                    _ => e,
                };
                for &j in marks.iter() {
                    classes[j as usize] = set;
                }
            }
            before = classes[i as usize];
        }
    }
}

/// The characters of an isolating run sequence in order, from a character on.
#[derive(Clone)]
struct SequenceChars<'a> {
    runs: &'a [(u32, u32)],
    next: &'a [u32],
    run: u32,
    at: u32,
}

impl<'a> SequenceChars<'a> {
    /// From the start of the sequence whose first run is `head`.
    fn new(runs: &'a [(u32, u32)], next: &'a [u32], head: u32) -> Self {
        let at = runs.get(head as usize).map_or(0, |r| r.0);
        Self {
            runs,
            next,
            run: head,
            at,
        }
    }

    /// From character `at` of run `run` (`at` may be the run's end).
    fn from(runs: &'a [(u32, u32)], next: &'a [u32], at: u32, run: u32) -> Self {
        Self {
            runs,
            next,
            run,
            at,
        }
    }
}

impl Iterator for SequenceChars<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        loop {
            let &(_, end) = self.runs.get(self.run as usize)?;
            if self.at < end {
                self.at += 1;
                return Some(self.at - 1);
            }
            self.run = *self.next.get(self.run as usize)?;
            self.at = self.runs.get(self.run as usize)?.0;
        }
    }
}

/// The characters of an isolating run sequence backwards, before a character.
struct SequenceBack<'a> {
    runs: &'a [(u32, u32)],
    prev: &'a [u32],
    run: u32,
    /// One past the next character.
    at: u32,
}

impl<'a> SequenceBack<'a> {
    /// The characters before character `at` of run `run`.
    fn from(runs: &'a [(u32, u32)], prev: &'a [u32], at: u32, run: u32) -> Self {
        Self {
            runs,
            prev,
            run,
            at,
        }
    }
}

impl Iterator for SequenceBack<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        loop {
            let &(start, _) = self.runs.get(self.run as usize)?;
            if self.at > start {
                self.at -= 1;
                return Some(self.at);
            }
            self.run = *self.prev.get(self.run as usize)?;
            self.at = self.runs.get(self.run as usize)?.1;
        }
    }
}

/// L2: the visual order of `levels` (a line's, after L1), as indices into it, into `order`.
pub(crate) fn visual_order(levels: &[u8], order: &mut Vec<u32>) {
    order.clear();
    order.extend(0..levels.len() as u32);
    let Some(&max) = levels.iter().max() else {
        return;
    };
    let min = levels.iter().copied().min().unwrap_or(0);
    // The lowest odd level at or above the line's lowest.
    let lowest_odd = min | 1;
    let mut level = max;
    while level >= lowest_odd {
        let mut k = 0;
        while k < levels.len() {
            if levels[order[k] as usize] >= level {
                let start = k;
                while k < levels.len() && levels[order[k] as usize] >= level {
                    k += 1;
                }
                order[start..k].reverse();
            } else {
                k += 1;
            }
        }
        if level == 0 {
            break;
        }
        level -= 1;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The cases of a `BidiCharacterTest.txt` that fail, the first few described.
    fn failures(file: &str) -> (usize, usize, Vec<String>) {
        let mut bidi = Bidi::default();
        let mut order = Vec::new();
        let mut passed = 0;
        let mut failed = 0;
        let mut shown = Vec::new();
        for line in file
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let f: Vec<&str> = line.split(';').collect();
            let text: String = f[0]
                .split(' ')
                .map(|h| char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap())
                .collect();
            let base = match f[1] {
                "0" => Some(0),
                "1" => Some(1),
                _ => None,
            };
            let para: u8 = f[2].parse().unwrap();
            let expected: Vec<Option<u8>> = f[3].split(' ').map(|x| x.parse().ok()).collect();
            let expected_order: Vec<u32> = f[4]
                .split(' ')
                .filter(|x| !x.is_empty())
                .map(|x| x.parse().unwrap())
                .collect();
            bidi.resolve(&text, base);
            let level = bidi.level();
            bidi.reset_line(0..text.len() as u32, level);
            let n = text.chars().count();
            let levels: Vec<u8> = if bidi.mixed() {
                bidi.levels().to_vec()
            } else {
                vec![level; n]
            };
            let gone: Vec<bool> = text.chars().map(|c| removed(bidi_class(c))).collect();
            visual_order(&levels, &mut order);
            let got_order: Vec<u32> = order
                .iter()
                .copied()
                .filter(|&k| !gone[k as usize])
                .collect();
            let levels_ok = levels
                .iter()
                .zip(&expected)
                .all(|(&l, e)| e.is_none_or(|e| e == l));
            if level == para && levels_ok && got_order == expected_order {
                passed += 1;
            } else {
                failed += 1;
                if shown.len() < 8 {
                    shown.push(format!(
                        "{line}\n  got level {level}, levels {levels:?}, order {got_order:?}"
                    ));
                }
            }
        }
        (passed, failed, shown)
    }

    #[test]
    fn a_sample_of_unicode_s_conformance_test_passes() {
        let (passed, failed, shown) =
            failures(include_str!("../tests/data/BidiCharacterTest-sample.txt"));
        assert!(passed > 1800, "{passed} passed");
        assert_eq!(failed, 0, "{}", shown.join("\n"));
    }

    /// The whole test (91,707 cases): `ENGINE_BIDI_TESTS` names the folder of Unicode 16's
    /// `BidiCharacterTest.txt`.
    #[test]
    fn unicode_s_conformance_test_passes() {
        let Some(dir) = std::env::var_os("ENGINE_BIDI_TESTS") else {
            return;
        };
        let file =
            std::fs::read_to_string(std::path::Path::new(&dir).join("BidiCharacterTest.txt"))
                .unwrap();
        let (passed, failed, shown) = failures(&file);
        println!("{passed} passed, {failed} failed");
        assert_eq!(failed, 0, "{}", shown.join("\n"));
    }

    #[test]
    fn left_to_right_text_is_not_processed() {
        let mut bidi = Bidi::default();
        bidi.resolve("Score: 1,234,567 — Сцена (ok)", None);
        assert!(!bidi.mixed());
        assert_eq!(bidi.level(), 0);
        bidi.resolve("Счёт ٣", None);
        assert!(bidi.mixed());
    }
}

#[cfg(test)]
mod cost {
    //! `cargo test --release -p engine_text --lib bidi::cost -- --ignored --nocapture`

    #![allow(clippy::print_stdout, clippy::unwrap_used)]

    use super::*;

    #[test]
    #[ignore = "a measurement"]
    fn what_levels_cost() {
        let line = "مرحبا بك في اللعبة، لديك ٣ أرواح و 250 نقطة ذهب.";
        let mut paragraph = String::new();
        while paragraph.len() < 10_000 {
            paragraph.push_str(
                "مرحبا بك في اللعبة الجديدة، لديك 3 أرواح و 250 نقطة ذهب في المستوى 12. اضغط على Enter للمتابعة. ",
            );
        }
        let mut bidi = Bidi::default();
        let mut order = Vec::new();
        for (name, text) in [
            ("48-character line", line),
            ("10 KB paragraph", &paragraph[..]),
        ] {
            let n = if text.len() < 200 { 100_000 } else { 200 };
            let mut samples: Vec<f64> = (0..9)
                .map(|_| {
                    let t = std::time::Instant::now();
                    for _ in 0..n {
                        bidi.resolve(std::hint::black_box(text), None);
                        let level = bidi.level();
                        bidi.reset_line(0..text.len() as u32, level);
                        visual_order(&bidi.levels, &mut order);
                        std::hint::black_box(&order);
                    }
                    t.elapsed().as_secs_f64() * 1e9 / f64::from(n)
                })
                .collect();
            samples.sort_by(f64::total_cmp);
            println!(
                "{name}: {:.0} ns ({:.1} ns per byte)",
                samples[4],
                samples[4] / text.len() as f64
            );
        }
    }
}
