//! Voice capture: record while a key is held or until told to stop, at whatever rate the device
//! gives, then resample to 16 kHz mono for transcription. Port of the recording half of
//! `voice.py`. Audio never reaches the disk; [`to_wav`] builds the bytes in memory.
//!
//! Capture goes through the WinMM `waveIn` API on the default input (`WAVE_MAPPER`), which the
//! `Win32_Media_Audio` feature already provides; the platform crate does not depend on `cpal`.
//! Every sample is resampled to [`SAMPLE_RATE`] as it arrives, so the silence test and the output
//! both see exactly what the transcriber will.

use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    waveInAddBuffer, waveInClose, waveInOpen, waveInPrepareHeader, waveInReset, waveInStart,
    waveInUnprepareHeader, CALLBACK_NULL, HWAVEIN, WAVEFORMATEX, WAVEHDR, WAVE_FORMAT_PCM,
    WAVE_MAPPER, WHDR_DONE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

/// What whisper wants. Everything recorded is resampled to this.
pub const SAMPLE_RATE: u32 = 16_000;
/// The window the silence test measures, in 16 kHz samples.
pub const BLOCK_FRAMES: usize = 1024;

/// Per-block RMS below which a block counts as silence. Measured, not guessed, on this machine's
/// array mic: room tone has a per-block median near 800 and a p90 near 1400, but a tail whose
/// worst block hit 3442 (sample peaks 5764), because the array mic applies its own gain. The first
/// guess, 2000, read that tail as speech and cut a recording off after 1.4 s. 5000 sits half again
/// above the loudest quiet block and well under speech at this gain. RMS rather than peak, so one
/// keyboard click does not read as a spoken word.
pub const SILENCE_LEVEL: f64 = 5000.0;

/// Rates tried in order when opening the device; the first the driver accepts is used.
const DEVICE_RATES: [u32; 3] = [48_000, 44_100, SAMPLE_RATE];
const BUFFERS: usize = 8;
const BUFFER_SECONDS: f64 = 0.064; // 1024 samples once resampled to 16 kHz
const POLL: Duration = Duration::from_millis(5);

/// Loudness of a span of samples, 0 for an empty one.
pub fn rms(block: &[i16]) -> f64 {
    if block.is_empty() {
        return 0.0;
    }
    let sum: f64 = block.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / block.len() as f64).sqrt()
}

/// True when these samples carry nothing louder than the room. Empty is silent.
pub fn is_silent(samples: &[i16], threshold: f64) -> bool {
    rms(samples) < threshold
}

/// Seconds of quiet at the end of a 16 kHz recording, measured in [`BLOCK_FRAMES`] windows walked
/// back from the last sample, so the answer does not depend on how the capture chunked the audio.
pub fn trailing_silence_seconds(samples: &[i16], threshold: f64) -> f64 {
    let mut quiet = 0usize;
    let mut end = samples.len();
    while end > 0 {
        let start = end.saturating_sub(BLOCK_FRAMES);
        let window = &samples[start..end];
        if rms(window) >= threshold {
            break;
        }
        quiet += window.len();
        end = start;
    }
    quiet as f64 / f64::from(SAMPLE_RATE)
}

/// Decides when a recording has trailed off. Silence ends a recording only once something was
/// actually said, or the breath before the first word would cut the user off.
#[derive(Debug, Clone)]
pub struct SilenceGate {
    silence_seconds: f64,
    threshold: f64,
    heard_speech: bool,
}

impl SilenceGate {
    /// `silence_seconds` of 0 or less disables the silence stop.
    pub fn new(silence_seconds: f64, threshold: f64) -> Self {
        SilenceGate {
            silence_seconds,
            threshold,
            heard_speech: false,
        }
    }

    /// Feed the whole recording so far, of which `new` samples just arrived. True means stop.
    pub fn should_stop(&mut self, all: &[i16], new: usize) -> bool {
        if self.silence_seconds <= 0.0 {
            return false;
        }
        if !self.heard_speech {
            let fresh = &all[all.len() - new.min(all.len())..];
            self.heard_speech = fresh
                .chunks(BLOCK_FRAMES)
                .any(|b| !is_silent(b, self.threshold));
            return false;
        }
        trailing_silence_seconds(all, self.threshold) >= self.silence_seconds
    }
}

/// A streaming linear-interpolation resampler, so chunks resample exactly like the whole would.
/// Positions are kept as exact integers (output k sits at input k * from / to), so chunking
/// cannot drift the way an accumulated float step would.
#[derive(Debug, Clone)]
pub struct Resampler {
    from: u64,
    to: u64,
    /// Index of the next output sample.
    next: u64,
    /// Input samples consumed by earlier chunks.
    base: u64,
    prev: Option<i16>,
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Resampler {
            from: u64::from(from_rate),
            to: u64::from(to_rate),
            next: 0,
            base: 0,
            prev: None,
        }
    }

    pub fn push(&mut self, chunk: &[i16], out: &mut Vec<i16>) {
        if chunk.is_empty() {
            return;
        }
        let end = self.base + chunk.len() as u64;
        let at = |abs: u64| -> f64 {
            if abs < self.base {
                f64::from(self.prev.unwrap_or(chunk[0]))
            } else {
                f64::from(chunk[(abs - self.base) as usize])
            }
        };
        loop {
            let num = self.next * self.from;
            let i = num / self.to;
            let rem = num % self.to;
            if i >= end || (rem > 0 && i + 1 >= end) {
                break;
            }
            let v = if rem > 0 {
                let frac = rem as f64 / self.to as f64;
                at(i) + (at(i + 1) - at(i)) * frac
            } else {
                at(i)
            };
            out.push(v.round().clamp(-32768.0, 32767.0) as i16);
            self.next += 1;
        }
        self.base = end;
        self.prev = chunk.last().copied();
    }
}

/// Resample a whole mono recording from `from_rate` to `to_rate`.
pub fn resample(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate {
        return samples.to_vec();
    }
    let mut out = Vec::with_capacity(samples.len() * to_rate as usize / from_rate as usize + 1);
    Resampler::new(from_rate, to_rate).push(samples, &mut out);
    out
}

/// A plain RIFF/WAVE file: mono PCM16 at `rate`.
pub fn to_wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

/// An open `waveIn` device with its buffers queued. Closing resets, unprepares and closes.
struct Device {
    handle: HWAVEIN,
    rate: u32,
    headers: Box<[Header]>,
    buffers: Vec<Vec<i16>>,
}

const HDR: u32 = std::mem::size_of::<WAVEHDR>() as u32;

/// A header at 8-byte alignment. `WAVEHDR` is `packed(1)`, but the driver writes `dwFlags` from
/// its own thread and the poll reads it volatile, which needs a properly aligned field.
#[repr(C, align(8))]
struct Header(WAVEHDR);

impl Device {
    fn open() -> Result<Device, String> {
        let mut last = 0;
        for rate in DEVICE_RATES {
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: 1,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * 2,
                nBlockAlign: 2,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            let mut handle = HWAVEIN::default();
            // SAFETY: valid out pointer and format; no callback, buffers are polled.
            last =
                unsafe { waveInOpen(Some(&mut handle), WAVE_MAPPER, &format, 0, 0, CALLBACK_NULL) };
            if last == 0 {
                return Device::start(handle, rate);
            }
        }
        Err(format!("no usable microphone (waveInOpen error {last})"))
    }

    fn start(handle: HWAVEIN, rate: u32) -> Result<Device, String> {
        let frames = (f64::from(rate) * BUFFER_SECONDS) as usize;
        let mut device = Device {
            handle,
            rate,
            headers: (0..BUFFERS).map(|_| Header(WAVEHDR::default())).collect(),
            buffers: (0..BUFFERS).map(|_| vec![0i16; frames]).collect(),
        };
        for i in 0..BUFFERS {
            let hdr = &mut device.headers[i].0;
            hdr.lpData = windows::core::PSTR(device.buffers[i].as_mut_ptr().cast());
            hdr.dwBufferLength = (frames * 2) as u32;
            // SAFETY: header and buffer are heap-pinned for the device's lifetime (Drop resets
            // the device before either is freed).
            let err = unsafe {
                let e = waveInPrepareHeader(handle, hdr, HDR);
                if e == 0 {
                    waveInAddBuffer(handle, hdr, HDR)
                } else {
                    e
                }
            };
            if err != 0 {
                return Err(format!("waveIn buffer setup failed ({err})"));
            }
        }
        // SAFETY: an open device with buffers queued.
        let err = unsafe { waveInStart(handle) };
        if err != 0 {
            return Err(format!("waveInStart failed ({err})"));
        }
        Ok(device)
    }

    /// The samples of buffer `i` if it is full, after which it is queued again.
    fn take(&mut self, i: usize, out: &mut Vec<i16>) -> bool {
        // SAFETY: aligned by `Header`; the driver writes dwFlags from another thread.
        let flags =
            unsafe { std::ptr::read_volatile(std::ptr::addr_of!(self.headers[i].0.dwFlags)) };
        if flags & WHDR_DONE == 0 {
            return false;
        }
        let n = self.headers[i].0.dwBytesRecorded as usize / 2;
        out.extend_from_slice(&self.buffers[i][..n]);
        self.headers[i].0.dwFlags &= !WHDR_DONE;
        // SAFETY: the header is still prepared; requeue it.
        unsafe {
            waveInAddBuffer(self.handle, &mut self.headers[i].0, HDR);
        }
        true
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: reset returns every buffer, so unpreparing and freeing them afterwards is sound.
        unsafe {
            waveInReset(self.handle);
            for Header(hdr) in self.headers.iter_mut() {
                waveInUnprepareHeader(self.handle, hdr, HDR);
            }
            waveInClose(self.handle);
        }
    }
}

/// The one capture loop: read until `done`, the cap, or a trailing silence after speech.
/// Returns 16 kHz mono samples.
fn record(
    done: impl Fn() -> bool,
    max_seconds: f64,
    silence_seconds: f64,
) -> Result<Vec<i16>, String> {
    let mut device = Device::open()?;
    let mut resampler = Resampler::new(device.rate, SAMPLE_RATE);
    let mut gate = SilenceGate::new(silence_seconds, SILENCE_LEVEL);
    let deadline = Instant::now() + Duration::from_secs_f64(max_seconds.max(0.0));
    let mut out = Vec::new();
    let mut raw = Vec::new();
    let mut next = 0;
    // `done` is checked before the clock so a caller's own gate wins.
    while !done() && Instant::now() < deadline {
        raw.clear();
        if !device.take(next, &mut raw) {
            std::thread::sleep(POLL);
            continue;
        }
        next = (next + 1) % BUFFERS;
        let before = out.len();
        resampler.push(&raw, &mut out);
        if gate.should_stop(&out, out.len() - before) {
            break;
        }
    }
    Ok(out)
}

/// Record from the default input until `stop` says so, `max_seconds` pass, or (when
/// `silence_seconds` > 0) that much quiet follows speech. 16 kHz mono. Err when there is no
/// usable microphone.
pub fn record_until(
    stop: impl Fn() -> bool,
    max_seconds: f64,
    silence_seconds: f64,
) -> Result<Vec<i16>, String> {
    record(stop, max_seconds, silence_seconds)
}

/// True while a virtual key is physically down. Push-to-talk needs the release that
/// `RegisterHotKey` never reports.
pub fn key_held(vk: u32) -> bool {
    // SAFETY: no preconditions.
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

/// Record while `vk` is held down, up to `max_seconds`. 16 kHz mono.
pub fn record_while_held(vk: u32, max_seconds: f64) -> Result<Vec<i16>, String> {
    record(|| !key_held(vk), max_seconds, 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: i16) -> Vec<i16> {
        (0..n)
            .map(|i| if i % 2 == 0 { amp } else { -amp })
            .collect()
    }

    #[test]
    fn rms_of_known_signals() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&tone(1024, 3000)), 3000.0);
        assert!(
            is_silent(&tone(1024, 3442), SILENCE_LEVEL),
            "measured room tail"
        );
        assert!(!is_silent(&tone(1024, 6000), SILENCE_LEVEL));
        assert!(is_silent(&[], SILENCE_LEVEL));
    }

    #[test]
    fn trailing_silence_walks_back_in_blocks() {
        let mut s = tone(4096, 10_000);
        s.extend(tone(16_000, 500));
        let t = trailing_silence_seconds(&s, SILENCE_LEVEL);
        // 16000 quiet samples: 15 whole blocks are quiet, the 16th straddles the loud part.
        assert!((t - 15.0 * 1024.0 / 16_000.0).abs() < 1e-9, "{t}");
        assert_eq!(
            trailing_silence_seconds(&tone(3000, 100), SILENCE_LEVEL),
            3000.0 / 16_000.0
        );
        assert_eq!(
            trailing_silence_seconds(&tone(3000, 9000), SILENCE_LEVEL),
            0.0
        );
    }

    #[test]
    fn trailing_silence_independent_of_chunking() {
        let mut s = tone(5000, 9000);
        s.extend(tone(20_000, 200));
        let whole = trailing_silence_seconds(&s, SILENCE_LEVEL);
        let mut gate = SilenceGate::new(1.0, SILENCE_LEVEL);
        let mut fed = Vec::new();
        let mut stopped_at = None;
        for chunk in s.chunks(777) {
            fed.extend_from_slice(chunk);
            if gate.should_stop(&fed, chunk.len()) {
                stopped_at = Some(fed.len());
                break;
            }
        }
        let at = stopped_at.expect("never stopped");
        assert!(trailing_silence_seconds(&fed, SILENCE_LEVEL) >= 1.0);
        assert!(at < s.len() && whole > 1.0);
    }

    #[test]
    fn silence_before_speech_never_stops() {
        let quiet = tone(64_000, 3442); // 4 s of loud room tone, no speech
        let mut gate = SilenceGate::new(0.5, SILENCE_LEVEL);
        let mut fed = Vec::new();
        for chunk in quiet.chunks(1024) {
            fed.extend_from_slice(chunk);
            assert!(!gate.should_stop(&fed, chunk.len()));
        }
    }

    #[test]
    fn disabled_gate_never_stops() {
        let mut gate = SilenceGate::new(0.0, SILENCE_LEVEL);
        let mut s = tone(1024, 9000);
        s.extend(tone(64_000, 0));
        assert!(!gate.should_stop(&s[..1024], 1024));
        assert!(!gate.should_stop(&s, s.len() - 1024));
    }

    #[test]
    fn resample_lengths() {
        assert_eq!(resample(&vec![0; 48_000], 48_000, 16_000).len(), 16_000);
        let n = resample(&vec![0; 44_100], 44_100, 16_000).len();
        assert!((15_999..=16_001).contains(&n), "{n}");
        assert_eq!(resample(&vec![1; 16_000], 16_000, 16_000).len(), 16_000);
        assert_eq!(resample(&vec![0; 8_000], 8_000, 16_000).len(), 15_999);
        assert!(resample(&[], 48_000, 16_000).is_empty());
    }

    #[test]
    fn streaming_resample_matches_one_shot() {
        let input: Vec<i16> = (0..10_000)
            .map(|i| ((i * 37) % 2000 - 1000) as i16)
            .collect();
        for (from, chunk) in [(48_000, 3072), (44_100, 2822), (44_100, 1)] {
            let whole = resample(&input, from, 16_000);
            let mut r = Resampler::new(from, 16_000);
            let mut parts = Vec::new();
            for c in input.chunks(chunk) {
                r.push(c, &mut parts);
            }
            assert_eq!(whole, parts, "{from} by {chunk}");
        }
    }

    #[test]
    fn resample_interpolates() {
        assert_eq!(resample(&[0, 300, 600, 900], 3, 2), vec![0, 450, 900]);
        assert_eq!(resample(&[0, 100], 1, 2), vec![0, 50, 100]);
    }

    #[test]
    fn wav_header_bytes() {
        let w = to_wav(&[1, -1, 0x1234], 16_000);
        assert_eq!(w.len(), 44 + 6);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[4..8], &42u32.to_le_bytes());
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(&w[16..20], &16u32.to_le_bytes());
        assert_eq!(&w[20..24], &[1, 0, 1, 0]);
        assert_eq!(&w[24..28], &16_000u32.to_le_bytes());
        assert_eq!(&w[28..32], &32_000u32.to_le_bytes());
        assert_eq!(&w[32..36], &[2, 0, 16, 0]);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(&w[40..44], &6u32.to_le_bytes());
        assert_eq!(&w[44..], &[1, 0, 0xFF, 0xFF, 0x34, 0x12]);
    }

    /// Records two seconds of room tone and prints its per-block RMS distribution. Opens the mic.
    /// `cargo test -p platform audio -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_room_tone() {
        let s = record_until(|| false, 2.0, 0.0).expect("microphone");
        let mut levels: Vec<f64> = s.chunks_exact(BLOCK_FRAMES).map(rms).collect();
        levels.sort_by(f64::total_cmp);
        let at = |p: f64| levels[((levels.len() - 1) as f64 * p).round() as usize];
        let peak = s.iter().map(|v| i32::from(*v).abs()).max().unwrap_or(0);
        println!(
            "samples {} ({:.2}s), blocks {}, rms min {:.0} median {:.0} p90 {:.0} max {:.0}, peak {}",
            s.len(),
            s.len() as f64 / 16_000.0,
            levels.len(),
            at(0.0),
            at(0.5),
            at(0.9),
            at(1.0),
            peak
        );
        assert!(s.len() > 28_000);
    }
}
