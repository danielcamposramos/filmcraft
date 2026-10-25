//! Audio played ahead: playback mixes on its own thread into a ring buffer that stays
//! [`LEAD_S`] ahead of the device, and the device callback only copies samples out of it.
//!
//! Mixing (decoding AAC, effects, opening a clip's file at a cut) used to run inside the device
//! callback, which has one I/O buffer (~10 ms) to return. On a busy Mac any stall there missed the
//! device's deadline, heard as static and dropouts that were different on every play. Now a stall
//! only drains part of the lead; the callback never decodes, allocates or takes a lock.
//!
//! When the ring does run dry (an underrun), the callback plays silence for the missing frames and
//! counts them, and the mixer skips the same span, so sound stays locked to the picture.
//!
//! Desktop only: the web build has no threads and keeps mixing in its callback.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// How far ahead of the device the mixer keeps the ring (s). Edits made while playing are heard
/// this much later.
pub const LEAD_S: f64 = 0.2;
/// Frames mixed per block on the mixing thread.
pub const BLOCK_FRAMES: usize = 1024;
/// Ceiling on mixed channels (large interfaces and aggregate devices stay well under it).
const MAX_CHANNELS: usize = 64;

/// Underrun counters for the current (or last) play, readable from any thread (`perf.stats`).
#[derive(Debug, Default)]
pub struct AudioStats {
    /// Device callbacks served.
    pub callbacks: AtomicU64,
    /// Callbacks the ring could not fill completely.
    pub underruns: AtomicU64,
    /// Frames played as silence because the ring was empty.
    pub missing_frames: AtomicU64,
    /// Fewest frames left in the ring after a callback, once it first filled to the lead
    /// (`u64::MAX` until then).
    pub min_lead_frames: AtomicU64,
}

impl AudioStats {
    pub fn reset(&self) {
        self.callbacks.store(0, Ordering::Relaxed);
        self.underruns.store(0, Ordering::Relaxed);
        self.missing_frames.store(0, Ordering::Relaxed);
        self.min_lead_frames.store(u64::MAX, Ordering::Relaxed);
    }

    pub fn to_json(&self, sample_rate: u32) -> serde_json::Value {
        let min = self.min_lead_frames.load(Ordering::Relaxed);
        serde_json::json!({
            "callbacks": self.callbacks.load(Ordering::Relaxed),
            "underruns": self.underruns.load(Ordering::Relaxed),
            "missingFrames": self.missing_frames.load(Ordering::Relaxed),
            "minLeadMs": if min == u64::MAX || sample_rate == 0 { serde_json::Value::Null } else { serde_json::json!(min as f64 * 1e3 / sample_rate as f64) },
        })
    }
}

/// Single-producer, single-consumer ring. Each frame carries its device position so a block
/// published after an underrun cannot play at a later position. Indices count queued frames.
struct Ring {
    slots: Vec<AtomicU32>,
    positions: Vec<AtomicI64>,
    channels: usize,
    write: AtomicU64,
    read: AtomicU64,
}

impl Ring {
    fn new(frames: usize, channels: usize) -> Self {
        let frames = frames.max(1);
        Self {
            slots: (0..frames * channels).map(|_| AtomicU32::new(0)).collect(),
            positions: (0..frames).map(|_| AtomicI64::new(0)).collect(),
            channels,
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
        }
    }

    fn len(&self) -> usize {
        self.write.load(Ordering::Acquire).saturating_sub(self.read.load(Ordering::Acquire)) as usize
    }

    /// Producer: append whole frames and their absolute positions; return the number written.
    fn push(&self, src: &[f32], start: i64) -> usize {
        let cap = self.positions.len();
        let w = self.write.load(Ordering::Relaxed);
        let free = cap.saturating_sub(w.saturating_sub(self.read.load(Ordering::Acquire)) as usize);
        let n = (src.len() / self.channels).min(free);
        for (i, frame) in src.chunks_exact(self.channels).take(n).enumerate() {
            let at = ((w + i as u64) % cap as u64) as usize;
            for (c, sample) in frame.iter().enumerate() {
                self.slots[at * self.channels + c].store(sample.to_bits(), Ordering::Relaxed);
            }
            self.positions[at].store(start + i as i64, Ordering::Relaxed);
        }
        self.write.store(w + n as u64, Ordering::Release);
        n
    }

    /// Consumer: play only the frames due at `start`, discarding late frames and leaving
    /// silence for gaps. A producer publication racing this callback is checked next time.
    fn pop(&self, dst: &mut [f32], dev_ch: usize, start: i64) -> usize {
        dst.fill(0.0);
        let cap = self.positions.len();
        let mut r = self.read.load(Ordering::Relaxed);
        let w = self.write.load(Ordering::Acquire);
        let mut copied = 0;
        for (i, frame) in dst.chunks_exact_mut(dev_ch).enumerate() {
            let due = start + i as i64;
            while r < w && self.positions[(r % cap as u64) as usize].load(Ordering::Relaxed) < due {
                r += 1;
            }
            if r == w {
                break;
            }
            let at = (r % cap as u64) as usize;
            if self.positions[at].load(Ordering::Relaxed) != due {
                continue;
            }
            for (c, sample) in frame.iter_mut().take(self.channels).enumerate() {
                *sample = f32::from_bits(self.slots[at * self.channels + c].load(Ordering::Relaxed));
            }
            r += 1;
            copied += 1;
        }
        self.read.store(r, Ordering::Release);
        copied
    }
}

/// Stops the mixing thread when the device callback that owns it is dropped (stream stopped).
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Mixes `mix(cursor, buf, channels)` (interleaved frames from device frame `cursor`) on its own
/// thread, from `start`, and returns the device callback that plays it (`fill(buf, channels)`).
/// The first block is mixed before this returns, so playback starts with sound. The thread runs
/// until the returned callback is dropped.
pub fn spawn<M>(mut mix: M, start: i64, sample_rate: u32, channels: usize, stats: Arc<AudioStats>) -> Box<dyn FnMut(&mut [f32], usize) + Send>
where
    M: FnMut(i64, &mut [f32], usize) + Send + 'static,
{
    stats.reset();
    let ch = channels.clamp(1, MAX_CHANNELS);
    let lead = ((sample_rate as f64 * LEAD_S) as usize).max(BLOCK_FRAMES * 2);
    let ring = Arc::new(Ring::new(lead, ch));
    let mut block = vec![0.0; BLOCK_FRAMES * ch];
    mix(start, &mut block, ch);
    ring.push(&block, start);
    let played = Arc::new(AtomicI64::new(start));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (ring, played, stop) = (ring.clone(), played.clone(), stop.clone());
        let body = move || {
            let mut cursor = start + BLOCK_FRAMES as i64;
            while !stop.load(Ordering::Acquire) {
                if ring.len() + BLOCK_FRAMES > lead {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                cursor = cursor.max(played.load(Ordering::Acquire));
                block.fill(0.0);
                mix(cursor, &mut block, ch);
                ring.push(&block, cursor);
                cursor += BLOCK_FRAMES as i64;
            }
        };
        let spawned = std::thread::Builder::new().name("filmcraft-audio-mix".into()).spawn(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_err() {
                log::error!("audio mixing stopped after a panic; playback continues silent");
            }
        });
        if let Err(e) = spawned {
            log::error!("audio mixing thread: {e}");
        }
    }
    let guard = StopOnDrop(stop);
    let mut primed = false;
    let mut cursor = start;
    Box::new(move |buf: &mut [f32], dev_ch: usize| {
        let _keep = &guard;
        stats.callbacks.fetch_add(1, Ordering::Relaxed);
        // a device wider or narrower than the mix (it changed since `channels` was read) still
        // plays the channels both have
        let dev_ch = dev_ch.max(1);
        let have = ring.pop(buf, dev_ch, cursor);
        let n = buf.len() / dev_ch;
        cursor += n as i64;
        played.store(cursor, Ordering::Release);
        if have < n {
            stats.underruns.fetch_add(1, Ordering::Relaxed);
            stats.missing_frames.fetch_add((n - have) as u64, Ordering::Relaxed);
        }
        let left = ring.len();
        primed |= left + 2 * BLOCK_FRAMES > lead;
        if primed {
            stats.min_lead_frames.fetch_min(left as u64, Ordering::Relaxed);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::Ring;

    #[test]
    fn a_block_published_after_silence_discards_its_late_frames() {
        let ring = Ring::new(8, 2);
        let mut out = [f32::NAN; 4];
        assert_eq!(ring.pop(&mut out, 2, 4), 0);
        assert_eq!(out, [0.0; 4]);
        ring.push(&[4.0, 4.0, 5.0, 5.0, 6.0, 6.0, 7.0, 7.0], 4);
        assert_eq!(ring.pop(&mut out, 2, 6), 2);
        assert_eq!(out, [6.0, 6.0, 7.0, 7.0]);
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn future_frames_wait_for_their_device_positions() {
        let ring = Ring::new(4, 2);
        ring.push(&[10.0, 11.0, 12.0, 13.0], 10);
        let mut out = [f32::NAN; 8];
        assert_eq!(ring.pop(&mut out, 2, 8), 2);
        assert_eq!(out, [0.0, 0.0, 0.0, 0.0, 10.0, 11.0, 12.0, 13.0]);
    }

    #[test]
    fn discarded_frames_leave_capacity_for_wrapped_multichannel_frames() {
        let ring = Ring::new(2, 3);
        ring.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 0);
        let mut out = [f32::NAN; 2];
        assert_eq!(ring.pop(&mut out, 1, 1), 1);
        assert_eq!(out, [4.0, 0.0]);
        assert_eq!(ring.push(&[7.0, 8.0, 9.0], 3), 1);
        let mut wide = [f32::NAN; 5];
        assert_eq!(ring.pop(&mut wide, 5, 3), 1);
        assert_eq!(wide, [7.0, 8.0, 9.0, 0.0, 0.0]);
    }
}
