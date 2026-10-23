//! Joins FLEX alphanumeric messages that were sent in fragments across
//! frames. A long message's first fragment carries fragment number 3 and the
//! *continued* flag; the following fragments are numbered 0, 1, 2, 0, … and
//! the last one has the flag clear. Fragments of one message share a capcode
//! and arrive a frame or a few apart, so the capcode is the key here (not
//! the phase: on air a continuation can come on another phase than its
//! first fragment did).
//!
//! Frames lost to collisions leave holes: a missing fragment shows as " … "
//! in the joined text and marks the message damaged; a message whose end
//! never comes is released after `TIMEOUT_FRAMES` with a trailing marker.

use std::collections::HashMap;

use crate::flex::Page;

/// Frames per 4-minute cycle, cycles per hour: frame indices wrap here.
const FRAMES_PER_CYCLE: u32 = 128;
const CYCLES: u32 = 15;
const WRAP: u32 = FRAMES_PER_CYCLE * CYCLES;
/// Frames to wait for the next fragment before giving a message up.
pub const TIMEOUT_FRAMES: u32 = 8;
/// What stands in for a fragment that never arrived.
pub const GAP: &str = " … ";

/// Index of a frame within the hour, for ordering and distances.
pub fn frame_index(cycle: u8, frame: u8) -> u32 {
    (cycle as u32 % CYCLES) * FRAMES_PER_CYCLE + frame as u32 % FRAMES_PER_CYCLE
}

fn frames_since(from: u32, to: u32) -> u32 {
    (to + WRAP - from) % WRAP
}

/// Fragment number expected after `prev` (3 is the first fragment, then
/// 0, 1, 2 cycle).
fn next_fragment(prev: u8) -> u8 {
    if prev == 3 { 0 } else { (prev + 1) % 3 }
}

struct Pending {
    page: Page,
    last_fragment: u8,
    last_index: u32,
    /// (frame index, fragment) already folded in: the same frame can be
    /// decoded twice from overlapping buffers.
    seen: Vec<(u32, u8)>,
}

/// Frames remembered by `new_frame`.
const RECENT_FRAMES: usize = 32;

/// Collects fragments per capcode and releases whole messages.
#[derive(Default)]
pub struct FragmentJoiner {
    pending: HashMap<u64, Pending>,
    /// Frame indices fed lately, newest last.
    recent: std::collections::VecDeque<u32>,
}

impl FragmentJoiner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Messages still waiting for fragments.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Whether frame (`cycle`, `frame`) is new, and remembers it. Buffers
    /// that overlap decode a frame twice; its pages are fed once.
    pub fn new_frame(&mut self, cycle: u8, frame: u8) -> bool {
        let idx = frame_index(cycle, frame);
        if self.recent.contains(&idx) {
            return false;
        }
        if self.recent.len() >= RECENT_FRAMES {
            self.recent.pop_front();
        }
        self.recent.push_back(idx);
        true
    }

    /// Feeds one decoded page from the frame (`cycle`, `frame`). Returns
    /// the pages ready to show: usually none for a fragment, one for a
    /// whole message, sometimes more when an unfinished message has to be
    /// released to make way for a new one.
    pub fn push(&mut self, page: Page, cycle: u8, frame: u8) -> Vec<Page> {
        let idx = frame_index(cycle, frame);
        let mut out = Vec::new();
        if page.kind != "alpha" || (page.fragment == 3 && !page.continued && !self.pending.contains_key(&page.capcode)) {
            out.push(page);
            return out;
        }
        let key = page.capcode;
        if page.fragment == 3 {
            // A new message starts: whatever was pending is over.
            if let Some(p) = self.pending.remove(&key) {
                if p.seen.contains(&(idx, 3)) {
                    // The same first fragment again (overlapping decode).
                    self.pending.insert(key, p);
                    return out;
                }
                out.push(release(p, true));
            }
            if page.continued {
                self.pending.insert(key, Pending { last_fragment: 3, last_index: idx, seen: vec![(idx, 3)], page });
            } else {
                out.push(page);
            }
            return out;
        }
        // A continuation fragment.
        match self.pending.get_mut(&key) {
            Some(p) => {
                if p.seen.contains(&(idx, page.fragment)) {
                    return out;
                }
                let expected = next_fragment(p.last_fragment);
                if page.fragment != expected {
                    p.page.text.push_str(GAP);
                    p.page.damaged = true;
                }
                p.page.text.push_str(&page.text);
                p.page.damaged |= page.damaged;
                p.last_fragment = page.fragment;
                p.last_index = idx;
                p.seen.push((idx, page.fragment));
                if !page.continued {
                    let p = self.pending.remove(&key).unwrap();
                    out.push(release(p, false));
                }
            }
            None => {
                // The start was lost (a collided frame): show what there is.
                let mut text = String::from(GAP.trim_start());
                text.push_str(&page.text);
                let partial = Page { text, damaged: true, ..page.clone() };
                if page.continued {
                    self.pending.insert(
                        key,
                        Pending { page: partial, last_fragment: page.fragment, last_index: idx, seen: vec![(idx, page.fragment)] },
                    );
                } else {
                    out.push(Page { continued: false, ..partial });
                }
            }
        }
        out
    }

    /// Call after each frame: releases messages whose next fragment is
    /// overdue, marked incomplete.
    pub fn expire(&mut self, cycle: u8, frame: u8) -> Vec<Page> {
        let idx = frame_index(cycle, frame);
        let stale: Vec<u64> =
            self.pending.iter().filter(|(_, p)| frames_since(p.last_index, idx) > TIMEOUT_FRAMES).map(|(k, _)| *k).collect();
        let mut out: Vec<Page> = stale.into_iter().filter_map(|k| self.pending.remove(&k)).map(|p| release(p, true)).collect();
        out.sort_by_key(|p| p.capcode);
        out
    }

    /// Releases everything pending (end of a recording).
    pub fn flush(&mut self) -> Vec<Page> {
        let mut out: Vec<Page> = self.pending.drain().map(|(_, p)| release(p, true)).collect();
        out.sort_by_key(|p| p.capcode);
        out
    }
}

fn release(p: Pending, incomplete: bool) -> Page {
    let mut page = p.page;
    if incomplete {
        page.text.push_str(GAP.trim_end());
        page.damaged = true;
    }
    page.continued = false;
    page
}

/// Joins the pages of already-decoded frames (a recording), in frame order.
pub fn join_frames(frames: &[crate::flex::FlexFrame]) -> Vec<Page> {
    let mut j = FragmentJoiner::new();
    let mut out = Vec::new();
    for f in frames {
        if !j.new_frame(f.cycle, f.frame) {
            continue;
        }
        for p in &f.pages {
            out.extend(j.push(p.clone(), f.cycle, f.frame));
        }
        out.extend(j.expire(f.cycle, f.frame));
    }
    out.extend(j.flush());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag(capcode: u64, fragment: u8, continued: bool, text: &str) -> Page {
        Page { capcode, phase: 'A', kind: "alpha", text: text.into(), damaged: false, continued, fragment, message_no: 0 }
    }

    #[test]
    fn whole_messages_pass_straight_through() {
        let mut j = FragmentJoiner::new();
        let p = frag(1, 3, false, "short");
        assert_eq!(j.push(p.clone(), 0, 1), vec![p]);
        let tone = Page { kind: "tone", ..frag(2, 0, false, "") };
        assert_eq!(j.push(tone.clone(), 0, 1), vec![tone]);
        assert_eq!(j.pending(), 0);
    }

    #[test]
    fn two_fragments_join_in_order() {
        let mut j = FragmentJoiner::new();
        assert!(j.push(frag(7, 3, true, "first part "), 0, 10).is_empty());
        // Another capcode in between is independent.
        assert_eq!(j.push(frag(8, 3, false, "other"), 0, 10).len(), 1);
        // The same first fragment decoded again from the overlapping buffer.
        assert!(j.push(frag(7, 3, true, "first part "), 0, 10).is_empty());
        let out = j.push(frag(7, 0, false, "second part"), 0, 11);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "first part second part");
        assert!(!out[0].damaged && !out[0].continued);
        assert_eq!(j.pending(), 0);
        // Three fragments: 3, 0, 1.
        assert!(j.push(frag(7, 3, true, "a"), 0, 12).is_empty());
        assert!(j.push(frag(7, 0, true, "b"), 0, 13).is_empty());
        let out = j.push(frag(7, 1, false, "c"), 0, 14);
        assert_eq!(out[0].text, "abc");
    }

    #[test]
    fn a_frame_decoded_twice_is_fed_once() {
        // The live monitor decodes overlapping buffers, so a frame can come
        // round twice. The second time its pages must not be fed again: a
        // continuation fed after its message was released looked like a
        // lost start and made a damaged "… tail" page.
        let mut j = FragmentJoiner::new();
        assert!(j.new_frame(5, 20));
        assert!(j.push(frag(7, 3, true, "head "), 5, 20).is_empty());
        assert!(j.new_frame(5, 21));
        assert_eq!(j.push(frag(7, 0, false, "tail"), 5, 21)[0].text, "head tail");
        assert!(!j.new_frame(5, 21), "same frame again");
        assert!(!j.new_frame(5, 20));
        // A fresh frame is new; an hour later the same index is new again.
        assert!(j.new_frame(5, 22));
        for f in 0..40u8 {
            j.new_frame(6, f);
        }
        assert!(j.new_frame(5, 20));
    }

    #[test]
    fn fragments_join_across_phases() {
        // On air (929.6125, 3200/4) a message's first fragment went out on
        // phase B and its continuation on phase A, a frame later.
        let mut j = FragmentJoiner::new();
        assert!(j.push(Page { phase: 'B', ..frag(2_423_793, 3, true, "first ") }, 9, 112).is_empty());
        let out = j.push(Page { phase: 'A', ..frag(2_423_793, 0, false, "second") }, 9, 113);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "first second");
        assert!(!out[0].damaged);
        assert_eq!(j.pending(), 0);
    }

    #[test]
    fn missing_middle_fragment_leaves_a_gap() {
        let mut j = FragmentJoiner::new();
        assert!(j.push(frag(7, 3, true, "one "), 1, 0).is_empty());
        // Fragment 0 was lost; 1 arrives.
        let out = j.push(frag(7, 1, false, "three"), 1, 2);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, format!("one {GAP}three"));
        assert!(out[0].damaged);
        // A lost start: a continuation with nothing pending.
        let out = j.push(frag(9, 0, false, "tail only"), 1, 3);
        assert_eq!(out[0].text, format!("{}tail only", GAP.trim_start()));
        assert!(out[0].damaged);
    }

    #[test]
    fn unfinished_messages_time_out_or_yield_to_a_new_start() {
        let mut j = FragmentJoiner::new();
        assert!(j.push(frag(7, 3, true, "never finished"), 2, 100).is_empty());
        assert!(j.expire(2, 100 + TIMEOUT_FRAMES as u8).is_empty(), "not yet overdue");
        let out = j.expire(2, 100 + TIMEOUT_FRAMES as u8 + 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, format!("never finished{}", GAP.trim_end()));
        assert!(out[0].damaged && !out[0].continued);
        // Wrap-around of the frame index at the end of the hour.
        assert!(j.push(frag(7, 3, true, "late"), 14, 127).is_empty());
        assert!(j.expire(0, 3).is_empty());
        assert_eq!(j.expire(0, TIMEOUT_FRAMES as u8 + 1).len(), 1);
        // A new first fragment releases the old message as incomplete.
        assert!(j.push(frag(7, 3, true, "x"), 3, 0).is_empty());
        let out = j.push(frag(7, 3, true, "y"), 3, 1);
        assert_eq!(out.len(), 1);
        assert!(out[0].text.starts_with('x') && out[0].damaged);
        assert_eq!(j.push(frag(7, 0, false, "z"), 3, 2)[0].text, "yz");
    }
}
