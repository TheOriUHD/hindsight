//! In-memory replay buffer of encoded packets. Holds compressed data only, so
//! 60 s of 4K is a few hundred MB at most.

use std::collections::VecDeque;

use hindsight_encode::Packet;

pub struct Replay {
    packets: VecDeque<Packet>,
    keep_us: i64,
    bytes: usize,
    newest_us: i64,
}

impl Replay {
    pub fn new(buffer_secs: u32) -> Self {
        // Keep an extra GOP so a full-length clip can still start on a keyframe.
        Self { packets: VecDeque::new(), keep_us: (buffer_secs as i64 + 2) * 1_000_000, bytes: 0, newest_us: 0 }
    }

    pub fn push(&mut self, p: Packet) {
        self.newest_us = self.newest_us.max(p.dts);
        self.bytes += p.data.len();
        self.packets.push_back(p);
        while let Some(f) = self.packets.front() {
            if f.dts >= self.newest_us - self.keep_us {
                break;
            }
            self.bytes -= f.data.len();
            self.packets.pop_front();
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn span_secs(&self) -> f64 {
        match self.packets.front() {
            Some(f) => ((self.newest_us - f.dts).max(0) as f64 / 1e6 - 2.0).max(0.0),
            None => 0.0,
        }
    }

    /// Packets for the `clip_us` before `end` (both on the recorder clock, µs),
    /// starting at a video keyframe, sorted by dts, plus the rebase timestamp.
    pub fn snapshot(&self, clip_us: i64, end: i64) -> Option<(Vec<Packet>, i64)> {
        let newest = self.packets.iter().filter(|p| p.stream == 0).map(|p| p.pts).max()?;
        let end = end.min(newest);
        let want = end - clip_us;
        let keyframes = self.packets.iter().filter(|p| p.stream == 0 && p.keyframe);
        // Latest keyframe at or before the wanted start, else the earliest one.
        let kf = keyframes
            .clone()
            .filter(|p| p.pts <= want)
            .max_by_key(|p| p.pts)
            .or_else(|| keyframes.min_by_key(|p| p.pts))?;
        let (kf_dts, base) = (kf.dts, kf.pts);
        let mut out: Vec<Packet> = self
            .packets
            .iter()
            // Video cut by dts so every reference a kept frame needs is kept too.
            .filter(|p| if p.stream == 0 { p.dts >= kf_dts && p.dts <= end } else { p.pts >= base && p.pts <= end })
            .cloned()
            .collect();
        out.sort_by_key(|p| p.dts);
        Some((out, base))
    }
}
