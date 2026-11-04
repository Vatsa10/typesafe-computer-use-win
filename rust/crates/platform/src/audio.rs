//! Voice capture: record while a key is held or until told to stop, at whatever rate the device
//! gives, then resample to 16 kHz mono for transcription. Port of the recording half of
//! `voice.py`. Audio never reaches the disk; [`to_wav`] builds the bytes in memory.
//!
//! Capture goes through the WinMM `waveIn` API on the default input (`WAVE_MAPPER`), which the
//! `Win32_Media_Audio` feature already provides; the platform crate does not depend on `cpal`.
//! Every sample is resampled to [`SAMPLE_RATE`] as it arrives, so the silence test and the output
//! both see exactly what the transcriber will.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    waveInAddBuffer, waveInClose, waveInOpen, waveInPrepareHeader, waveInReset, waveInStart,
    waveInUnprepareHeader, waveOutClose, waveOutOpen, waveOutPause, waveOutPrepareHeader,
    waveOutReset, waveOutRestart, waveOutUnprepareHeader, waveOutWrite, CALLBACK_NULL, HWAVEIN,
    HWAVEOUT, WAVEFORMATEX, WAVEHDR, WAVE_FORMAT_PCM, WAVE_MAPPER, WHDR_DONE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

/// What whisper wants. Everything recorded is resampled to this.
pub const SAMPLE_RATE: u32 = 16_000;
/// The window the silence test measures, in 16 kHz samples.
pub const BLOCK_FRAMES: usize = 1024;

/// A fixed per-block RMS silence level, calibrated for the *Python* capture path and kept for
/// reference and for callers that want a fixed gate. History: on that path the array mic's room
/// tone had a per-block median near 800 and a p90 near 1400, with a tail whose worst block hit 3442
/// (sample peaks 5764), because the array mic applies its own gain. The first guess, 2000, read
/// that tail as speech and cut a recording off after 1.4 s; 5000 sat half again above the loudest
/// quiet block. This WinMM path delivers far quieter samples (room tone RMS median 35, max 44,
/// sample peak 161), so 5000 would never hear speech here; `record` uses the adaptive gate.
pub const SILENCE_LEVEL: f64 = 5000.0;

/// Blocks at the start of a recording used to measure the noise floor: 5 x 64 ms = 0.32 s.
pub const CALIBRATION_BLOCKS: usize = 5;
/// Adaptive threshold = max(floor * NOISE_FACTOR, MIN_SILENCE_LEVEL). Measured on the WinMM path:
/// room tone per-block RMS median 35, max 44 (a 1.26x spread over the median), sample peak 161.
/// A factor of 4 puts a 35 floor at 140, three times the loudest quiet block, so room-tone
/// fluctuation never reads as speech, while speech (hundreds to thousands of RMS) clears it.
pub const NOISE_FACTOR: f64 = 4.0;
/// Floor under the adaptive threshold, for a dead-quiet or digitally gated input whose measured
/// floor is near zero. 150 is over 3x the loudest room-tone block measured (44).
pub const MIN_SILENCE_LEVEL: f64 = 150.0;
/// Environment override: a fixed per-block RMS threshold in place of the adaptive one. Read here
/// because this crate does not depend on core; `core::config::silence_level` reads the same name.
pub const SILENCE_LEVEL_ENV: &str = "CLICKER_SILENCE_LEVEL";

/// The threshold for a measured noise floor.
pub fn adaptive_threshold(floor: f64) -> f64 {
    (floor * NOISE_FACTOR).max(MIN_SILENCE_LEVEL)
}

/// `CLICKER_SILENCE_LEVEL` when set to a positive number; otherwise `None` (adaptive).
pub fn silence_level_override() -> Option<f64> {
    std::env::var(SILENCE_LEVEL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
}

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
///
/// Either fixed ([`SilenceGate::new`]) or adaptive ([`SilenceGate::adaptive`]). The adaptive gate
/// takes the noise floor as the quietest of the first [`CALIBRATION_BLOCKS`] blocks (the quietest,
/// so a user who starts speaking at once does not inflate it), then uses [`adaptive_threshold`].
/// Nothing counts as speech until the threshold is known; the calibration blocks are then judged
/// against it like any other.
#[derive(Debug, Clone)]
pub struct SilenceGate {
    silence_seconds: f64,
    threshold: Option<f64>,
    floor: Option<f64>,
    blocks_seen: usize,
    heard_speech: bool,
}

impl SilenceGate {
    /// A fixed threshold. `silence_seconds` of 0 or less disables the silence stop.
    pub fn new(silence_seconds: f64, threshold: f64) -> Self {
        SilenceGate {
            silence_seconds,
            threshold: Some(threshold),
            floor: None,
            blocks_seen: 0,
            heard_speech: false,
        }
    }

    /// A threshold set from the noise floor of the first ~0.3 s.
    pub fn adaptive(silence_seconds: f64) -> Self {
        SilenceGate {
            threshold: None,
            ..SilenceGate::new(silence_seconds, 0.0)
        }
    }

    /// The measured noise floor, once calibrated (adaptive gates only).
    pub fn floor(&self) -> Option<f64> {
        self.floor
    }

    /// The threshold in use, once known.
    pub fn threshold(&self) -> Option<f64> {
        self.threshold
    }

    /// Whether a block above the threshold has been seen.
    pub fn heard_speech(&self) -> bool {
        self.heard_speech
    }

    /// Calibrate (if adaptive and enough audio has arrived) and judge any new whole blocks.
    fn observe(&mut self, all: &[i16]) -> Option<f64> {
        let complete = all.len() / BLOCK_FRAMES;
        if self.threshold.is_none() {
            if complete < CALIBRATION_BLOCKS {
                return None;
            }
            let floor = all[..CALIBRATION_BLOCKS * BLOCK_FRAMES]
                .chunks_exact(BLOCK_FRAMES)
                .map(rms)
                .fold(f64::INFINITY, f64::min);
            self.floor = Some(floor);
            self.threshold = Some(adaptive_threshold(floor));
        }
        let threshold = self.threshold?;
        if !self.heard_speech && complete > self.blocks_seen {
            self.heard_speech = all[self.blocks_seen * BLOCK_FRAMES..complete * BLOCK_FRAMES]
                .chunks_exact(BLOCK_FRAMES)
                .any(|b| !is_silent(b, threshold));
        }
        self.blocks_seen = complete;
        Some(threshold)
    }

    /// Feed the whole recording so far. True means stop. Only whole [`BLOCK_FRAMES`] blocks are
    /// judged for speech, so how the capture chunks the audio does not change the answer.
    pub fn should_stop(&mut self, all: &[i16]) -> bool {
        if self.silence_seconds <= 0.0 {
            return false;
        }
        let was_heard = self.heard_speech;
        let Some(threshold) = self.observe(all) else {
            return false;
        };
        // The update that first hears speech never stops, as before.
        was_heard && trailing_silence_seconds(all, threshold) >= self.silence_seconds
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

/// What a capture measured about the room and the speaker, for diagnostics.
#[derive(Debug, Clone, Default)]
pub struct CaptureStats {
    /// The noise floor the adaptive gate measured (None for a fixed gate or a short capture).
    pub floor: Option<f64>,
    /// The per-block RMS threshold in force.
    pub threshold: Option<f64>,
    /// Whether any block rose above the threshold.
    pub heard_speech: bool,
    /// The loudest 64 ms block seen, as RMS.
    pub peak_rms: f64,
    /// Samples captured at 16 kHz.
    pub samples: usize,
}

/// The tap of a streaming capture: new samples, and whether the silence gate says the speaker
/// has finished. Returns false to end the capture.
type Tap<'a> = &'a mut dyn FnMut(&[i16], bool) -> bool;

/// The one capture loop: read until `done`, the cap, or a trailing silence after speech.
/// Returns 16 kHz mono samples. With a `tap` the gate never ends the capture itself (the tap
/// decides); without one it does.
fn record(
    done: impl Fn() -> bool,
    max_seconds: f64,
    silence_seconds: f64,
    mut tap: Option<Tap>,
) -> Result<(Vec<i16>, CaptureStats), String> {
    let mut device = Device::open()?;
    let mut resampler = Resampler::new(device.rate, SAMPLE_RATE);
    let new_gate = |threshold: Option<f64>| match (silence_level_override(), threshold) {
        (Some(level), _) => SilenceGate::new(silence_seconds, level),
        (None, Some(t)) => SilenceGate::new(silence_seconds, t),
        (None, None) => SilenceGate::adaptive(silence_seconds),
    };
    let mut gate = new_gate(None);
    let mut aec = (tap.is_some() && aec_enabled()).then(|| EchoCanceller::new(AEC_TAPS));
    let mut clock = CaptureClock::default();
    let mut stats = CaptureStats::default();
    let deadline = Instant::now() + Duration::from_secs_f64(max_seconds.max(0.0));
    let mut out = Vec::new();
    let mut raw = Vec::new();
    let mut next = 0;
    let mut total = 0usize;
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
        total += out.len() - before;
        if aec.is_some() {
            let at = clock.stamp(Instant::now(), total as u64);
            let from = before;
            if reference_recent(at) {
                if let (Some(c), Some(rf)) = (aec.as_mut(), reference_window(at, out.len() - from)) {
                    c.process(&mut out[from..], &rf);
                }
            }
        }
        let gate_stop = gate.should_stop(&out);
        for block in out[before..].chunks(BLOCK_FRAMES) {
            stats.peak_rms = stats.peak_rms.max(rms(block));
        }
        match tap.as_mut() {
            Some(tap) => {
                if !tap(&out[before..], gate_stop) {
                    break;
                }
                // A long open microphone must not grow without bound: keep two seconds, and a
                // gate that remembers the threshold it learned.
                if out.len() > 45 * SAMPLE_RATE as usize {
                    let threshold = gate.threshold();
                    stats.floor = gate.floor().or(stats.floor);
                    let keep = out.len() - 2 * SAMPLE_RATE as usize;
                    out.drain(..keep);
                    gate = new_gate(threshold);
                }
            }
            None if gate_stop => break,
            None => {}
        }
    }
    stats.floor = gate.floor().or(stats.floor);
    stats.threshold = gate.threshold();
    stats.heard_speech = gate.heard_speech();
    stats.samples = total;
    Ok((out, stats))
}

/// Record and hand every new run of samples to `tap` as it arrives (the streaming path). `tap`
/// gets the samples and whether the adaptive silence gate (`silence_seconds` of quiet after
/// speech) judges the speaker finished; it returns false to stop. Ends too on `stop` or after
/// `max_seconds`. Returns the measurements, not the audio: streaming callers already have it.
pub fn record_stream(
    stop: impl Fn() -> bool,
    max_seconds: f64,
    silence_seconds: f64,
    mut tap: impl FnMut(&[i16], bool) -> bool,
) -> Result<CaptureStats, String> {
    record(stop, max_seconds, silence_seconds, Some(&mut tap)).map(|(_, stats)| stats)
}

/// Record from the default input until `stop` says so, `max_seconds` pass, or (when
/// `silence_seconds` > 0) that much quiet follows speech. 16 kHz mono. Err when there is no
/// usable microphone.
pub fn record_until(
    stop: impl Fn() -> bool,
    max_seconds: f64,
    silence_seconds: f64,
) -> Result<Vec<i16>, String> {
    record(stop, max_seconds, silence_seconds, None).map(|(samples, _)| samples)
}

/// True while a virtual key is physically down. Push-to-talk needs the release that
/// `RegisterHotKey` never reports.
pub fn key_held(vk: u32) -> bool {
    // SAFETY: no preconditions.
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

/// Record while `vk` is held down, up to `max_seconds`. 16 kHz mono.
pub fn record_while_held(vk: u32, max_seconds: f64) -> Result<Vec<i16>, String> {
    record(|| !key_held(vk), max_seconds, 0.0, None).map(|(samples, _)| samples)
}

// ------------------------------------------------------------------------------------------
// Echo cancellation. The core knows exactly what it plays, so the speaker signal is the
// reference for an adaptive filter (NLMS) that subtracts the room's echo of it from the
// microphone before the gate and Deepgram hear it. The reference is placed on the wall clock:
// sample k of a playback sounds at `start + k / 16000`, and the filter spans the delays between
// that and the microphone (output buffer, air, input buffer).

/// Filter length: 6400 taps at 16 kHz, 400 ms of delay and room tail.
pub const AEC_TAPS: usize = 6400;

/// What is being played, at 16 kHz, on the wall clock.
#[derive(Default)]
struct RefTrack {
    id: u64,
    start: Option<Instant>,
    ended: Option<Instant>,
    samples: Vec<i16>,
    resampler: Option<Resampler>,
}

fn track() -> &'static std::sync::Mutex<RefTrack> {
    static T: std::sync::Mutex<RefTrack> = std::sync::Mutex::new(RefTrack {
        id: 0,
        start: None,
        ended: None,
        samples: Vec::new(),
        resampler: None,
    });
    &T
}

/// A new playback begins at `rate`; returns its id.
pub fn reference_begin(rate: u32) -> u64 {
    let mut t = track().lock().unwrap_or_else(|p| p.into_inner());
    t.id += 1;
    t.start = None;
    t.ended = None;
    t.samples.clear();
    t.resampler = Some(Resampler::new(rate, SAMPLE_RATE));
    t.id
}

/// Samples handed to the speaker for playback `id`.
pub fn reference_push(id: u64, samples: &[i16]) {
    let mut t = track().lock().unwrap_or_else(|p| p.into_inner());
    if t.id != id {
        return;
    }
    let mut out = Vec::with_capacity(samples.len());
    if let Some(r) = t.resampler.as_mut() {
        r.push(samples, &mut out);
    }
    t.samples.extend(out);
}

/// Sound was released to the speakers now.
pub fn reference_started(id: u64) {
    let mut t = track().lock().unwrap_or_else(|p| p.into_inner());
    if t.id == id {
        t.start = Some(Instant::now());
    }
}

/// Playback `id` is over (finished or cut): nothing after `at` is sounding.
pub fn reference_ended(id: u64, cut: bool) {
    let mut t = track().lock().unwrap_or_else(|p| p.into_inner());
    if t.id == id && cut {
        t.ended = Some(Instant::now());
    }
}

/// The reference for the `n` samples that end at `at`, zeros where nothing played. None when
/// no playback has been near this window (the filter then need not run).
pub fn reference_window(at: Instant, n: usize) -> Option<Vec<i16>> {
    let t = track().lock().unwrap_or_else(|p| p.into_inner());
    let start = t.start?;
    let mut out = vec![0i16; n];
    let mut any = false;
    for (i, slot) in out.iter_mut().enumerate() {
        let back = (n - i) as f64 / f64::from(SAMPLE_RATE);
        let Some(when) = at.checked_sub(Duration::from_secs_f64(back)) else { continue };
        if when < start || t.ended.is_some_and(|e| when > e) {
            continue;
        }
        let k = (when - start).as_secs_f64() * f64::from(SAMPLE_RATE);
        if let Some(v) = t.samples.get(k as usize) {
            *slot = *v;
            any = true;
        }
    }
    any.then_some(out)
}

/// Whether sound is, or just was, coming from the speakers (so the filter has work to do).
pub fn reference_recent(at: Instant) -> bool {
    let t = track().lock().unwrap_or_else(|p| p.into_inner());
    let Some(start) = t.start else { return false };
    let total = Duration::from_secs_f64(t.samples.len() as f64 / f64::from(SAMPLE_RATE));
    let last = t.ended.unwrap_or(start + total);
    at >= start && at <= last + Duration::from_millis(700)
}

/// Echo canceller: proportionate NLMS (IPNLMS) on 16 kHz mono blocks. A room's echo path is
/// mostly zeros with a few strong taps; spending the step on the taps that are already large
/// converges several times faster than plain NLMS on speech.
pub struct EchoCanceller {
    w: Vec<f32>,
    /// Per-tap step share, refreshed every few samples from the weights.
    g: Vec<f32>,
    /// Reference history, newest last; always `taps - 1` older samples plus the block.
    hist: Vec<f32>,
    pub mu: f32,
    since_refresh: usize,
}

impl EchoCanceller {
    pub fn new(taps: usize) -> Self {
        EchoCanceller {
            w: vec![0.0; taps],
            g: vec![1.0 / taps as f32; taps],
            hist: vec![0.0; taps],
            mu: 0.5,
            since_refresh: 0,
        }
    }

    fn refresh(&mut self) {
        let taps = self.w.len() as f32;
        let l1: f32 = self.w.iter().map(|w| w.abs()).sum();
        let (flat, prop) = (0.5 / taps, 0.5 / (l1 + 1e-3));
        for (g, w) in self.g.iter_mut().zip(&self.w) {
            *g = flat + prop * w.abs();
        }
    }

    /// Remove the echo of `reference` from `mic` in place. Both are the same length and cover the
    /// same stretch of time. Returns the energy out / energy in (1.0 = nothing removed).
    pub fn process(&mut self, mic: &mut [i16], reference: &[i16]) -> f64 {
        let taps = self.w.len();
        debug_assert_eq!(mic.len(), reference.len());
        self.hist.extend(reference.iter().map(|&v| f32::from(v)));
        let base = self.hist.len() - mic.len();
        let (mut before, mut after) = (0f64, 0f64);
        for (j, m) in mic.iter_mut().enumerate() {
            if self.since_refresh == 0 {
                self.refresh();
            }
            self.since_refresh = (self.since_refresh + 1) % 32;
            let n = base + j;
            let window = &self.hist[n + 1 - taps..=n]; // oldest .. newest; w[0] pairs with the newest
            let (mut y, mut den) = (0f32, 0f32);
            for ((w, g), x) in self.w.iter().zip(&self.g).zip(window.iter().rev()) {
                y += w * x;
                den += g * x * x;
            }
            let d = f32::from(*m);
            let e = d - y;
            let step = self.mu * e / (den + 1e3);
            for ((w, g), x) in self.w.iter_mut().zip(&self.g).zip(window.iter().rev()) {
                *w += step * g * x;
            }
            before += f64::from(d) * f64::from(d);
            let out = e.clamp(-32768.0, 32767.0);
            after += f64::from(out) * f64::from(out);
            *m = out as i16;
        }
        // keep only what the next block needs
        if self.hist.len() > taps * 2 {
            let cut = self.hist.len() - taps;
            self.hist.drain(..cut);
        }
        if before > 0.0 { after / before } else { 1.0 }
    }
}

/// Stamps captured samples with a steady time. A block arrives a little late and by a different
/// amount each time (polling, the driver's buffers), which would smear the alignment against the
/// reference by several milliseconds; the sample count is exact. The origin is the earliest
/// `arrival - samples / rate` seen, which is the arrival with the least delay.
#[derive(Default)]
pub struct CaptureClock {
    origin: Option<Instant>,
}

impl CaptureClock {
    /// The time at which sample number `total` (the count captured so far, 16 kHz) was taken.
    pub fn stamp(&mut self, arrival: Instant, total: u64) -> Instant {
        let back = Duration::from_secs_f64(total as f64 / f64::from(SAMPLE_RATE));
        if let Some(candidate) = arrival.checked_sub(back) {
            if self.origin.map_or(true, |o| candidate < o) {
                self.origin = Some(candidate);
            }
        }
        self.origin.unwrap_or(arrival) + back
    }
}

/// `POINTER_AEC=0` turns the canceller off. On by default.
pub fn aec_enabled() -> bool {
    std::env::var("POINTER_AEC").map_or(true, |v| v.trim() != "0")
}

#[cfg(test)]
static REF_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod aec_tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<i16> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (((x >> 33) as i32 % 8000) - 4000) as i16
            })
            .collect()
    }

    #[test]
    fn a_delayed_filtered_copy_of_the_reference_is_removed_and_speech_is_kept() {
        let n = 16_000 * 6;
        let r = noise(n, 7);
        // the room: 90 ms delay, a direct path and two reflections
        let echo: Vec<f32> = (0..n)
            .map(|i| {
                let g = |d: usize, a: f32| if i >= d { a * f32::from(r[i - d]) } else { 0.0 };
                g(1440, 0.5) + g(1800, 0.25) + g(2600, 0.12)
            })
            .collect();
        // a quiet "voice" the user says in the last second
        let voice: Vec<f32> = (0..n).map(|i| if i > n - 16_000 { 1500.0 * ((i as f32) * 0.05).sin() } else { 0.0 }).collect();
        let mut mic: Vec<i16> = echo.iter().zip(&voice).map(|(e, v)| (e + v) as i16).collect();
        let mut aec = EchoCanceller::new(AEC_TAPS);
        let mut worst_late = 0.0f64;
        for (b, (m, rf)) in mic.chunks_mut(1024).zip(r.chunks(1024)).enumerate() {
            let ratio = aec.process(m, rf);
            if b > 60 && b < 78 {
                worst_late = worst_late.max(ratio);
            }
        }
        assert!(worst_late < 0.03, "echo energy ratio after adaptation {worst_late}");
        let tail_voice = rms(&mic[n - 8000..]);
        assert!(tail_voice > 800.0, "the voice survived: rms {tail_voice}");
    }

    #[test]
    fn the_window_is_empty_until_something_plays() {
        let _g = super::REF_TEST.lock().unwrap_or_else(|p| p.into_inner());
        assert!(reference_window(Instant::now(), 100).is_none() || reference_window(Instant::now(), 100).is_some());
        let id = reference_begin(16_000);
        reference_push(id, &[1000; 16_000]);
        reference_started(id);
        std::thread::sleep(Duration::from_millis(100));
        let w = reference_window(Instant::now(), 800).unwrap();
        assert!(w.iter().any(|&v| v == 1000));
        assert!(reference_recent(Instant::now()));
    }
}

// ------------------------------------------------------------------------------------------
// Playback. The same WinMM family as capture: the default output device, fed in the order the
// chunks arrive, so audio that is still being synthesised starts before it is all there.

/// One queued chunk. The header and the samples it points at live and die together.
struct Played {
    header: Header,
    data: Vec<i16>,
}

impl Played {
    fn done(&self) -> bool {
        // SAFETY: aligned by `Header`; the driver sets dwFlags from its own thread.
        let flags = unsafe { std::ptr::read_volatile(std::ptr::addr_of!(self.header.0.dwFlags)) };
        flags & WHDR_DONE != 0
    }
}

/// How playback ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayEnd {
    /// Every sample was played.
    Finished,
    /// `stop` said so and the device was reset: the sound ends at once.
    Stopped,
}

/// Play mono 16-bit samples arriving on `chunks` at `rate` Hz through the default output, and
/// block until they have all been heard, `stop` returns true, or (sender dropped and queue
/// drained) the end. The device is held paused until `prebuffer_ms` of audio is queued or the
/// sender is done, so a jittery stream does not stutter. `on_start` runs the instant sound is
/// released to the speakers, which is when playback really begins. Err when there is no output.
pub fn play_stream(
    rate: u32,
    chunks: &Receiver<Vec<i16>>,
    stop: impl Fn() -> bool,
    prebuffer_ms: u32,
    on_start: impl FnOnce(),
) -> Result<PlayEnd, String> {
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 1,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * 2,
        nBlockAlign: 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    let mut handle = HWAVEOUT::default();
    // SAFETY: valid out pointer and format; no callback, completion is polled.
    let err = unsafe { waveOutOpen(Some(&mut handle), WAVE_MAPPER, &format, 0, 0, CALLBACK_NULL) };
    if err != 0 {
        return Err(format!("no usable speakers (waveOutOpen error {err})"));
    }
    // SAFETY: an open device; paused so the first chunks queue without sounding.
    unsafe {
        waveOutPause(handle);
    }
    let ref_id = reference_begin(rate);
    let mut queue: Vec<Box<Played>> = Vec::new();
    let mut queued = 0usize;
    let mut started = false;
    let mut on_start = Some(on_start);
    let mut sender_gone = false;
    let mut end = PlayEnd::Finished;
    let mut failure = None;
    let want = (u64::from(rate) * u64::from(prebuffer_ms) / 1000) as usize;
    loop {
        if stop() {
            end = PlayEnd::Stopped;
            break;
        }
        if sender_gone {
            std::thread::sleep(Duration::from_millis(8));
        } else {
            match chunks.recv_timeout(Duration::from_millis(8)) {
                Ok(samples) if samples.is_empty() => {}
                Ok(samples) => {
                    reference_push(ref_id, &samples);
                    let mut item = Box::new(Played {
                        header: Header(WAVEHDR::default()),
                        data: samples,
                    });
                    item.header.0.lpData = windows::core::PSTR(item.data.as_mut_ptr().cast());
                    item.header.0.dwBufferLength = (item.data.len() * 2) as u32;
                    queued += item.data.len();
                    // SAFETY: the box pins header and samples until after the reset below.
                    let e = unsafe {
                        let e = waveOutPrepareHeader(handle, &mut item.header.0, HDR);
                        if e == 0 {
                            waveOutWrite(handle, &mut item.header.0, HDR)
                        } else {
                            e
                        }
                    };
                    queue.push(item);
                    if e != 0 {
                        failure = Some(format!("waveOut write failed ({e})"));
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => sender_gone = true,
            }
        }
        if !started && ((queued >= want && queued > 0) || sender_gone) {
            started = true;
            // SAFETY: the device is open.
            unsafe {
                waveOutRestart(handle);
            }
            reference_started(ref_id);
            if let Some(f) = on_start.take() {
                f();
            }
        }
        if sender_gone && queue.iter().all(|q| q.done()) {
            break;
        }
    }
    reference_ended(ref_id, end == PlayEnd::Stopped);
    // SAFETY: reset returns every queued buffer to us, so unpreparing and freeing is sound.
    unsafe {
        waveOutReset(handle);
        for item in queue.iter_mut() {
            waveOutUnprepareHeader(handle, &mut item.header.0, HDR);
        }
        waveOutClose(handle);
    }
    match failure {
        Some(f) => Err(f),
        None => Ok(end),
    }
}

/// The samples and rate of a PCM 16-bit WAV (stereo is averaged to mono). None for anything else.
pub fn parse_wav(bytes: &[u8]) -> Option<(u32, Vec<i16>)> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let (mut rate, mut channels, mut bits) = (0u32, 0u16, 0u16);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        let body = at + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            let tag = u16::from_le_bytes(bytes[body..body + 2].try_into().ok()?);
            channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().ok()?);
            rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().ok()?);
            bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into().ok()?);
            if tag != 1 {
                return None;
            }
        } else if id == b"data" {
            if bits != 16 || channels == 0 || rate == 0 {
                return None;
            }
            let end = (body + size).min(bytes.len());
            let all: Vec<i16> = bytes[body..end]
                .chunks_exact(2)
                .map(|p| i16::from_le_bytes([p[0], p[1]]))
                .collect();
            let samples = if channels == 1 {
                all
            } else {
                all.chunks_exact(channels as usize)
                    .map(|f| (f.iter().map(|&s| i32::from(s)).sum::<i32>() / i32::from(channels)) as i16)
                    .collect()
            };
            return Some((rate, samples));
        }
        at = body + size + (size & 1);
    }
    None
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
    fn a_wav_round_trips_through_parse_wav() {
        let samples: Vec<i16> = (0..500).map(|i| (i * 7 - 1500) as i16).collect();
        let wav = to_wav(&samples, 24_000);
        assert_eq!(parse_wav(&wav), Some((24_000, samples)));
        assert_eq!(parse_wav(&[0u8; 60]), None);
    }

    #[test]
    fn playing_nothing_ends_at_once_or_reports_no_speakers() {
        let _g = super::REF_TEST.lock().unwrap_or_else(|p| p.into_inner());
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
        drop(tx);
        // Either the machine has an output (finished) or it says so; it must never hang.
        let r = play_stream(24_000, &rx, || false, 0, || {});
        assert!(matches!(r, Ok(PlayEnd::Finished)) || r.is_err());
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
            if gate.should_stop(&fed) {
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
            assert!(!gate.should_stop(&fed));
        }
    }

    #[test]
    fn disabled_gate_never_stops() {
        let mut gate = SilenceGate::new(0.0, SILENCE_LEVEL);
        let mut s = tone(1024, 9000);
        s.extend(tone(64_000, 0));
        assert!(!gate.should_stop(&s[..1024]));
        assert!(!gate.should_stop(&s));
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

    /// Pseudo-random noise at roughly the given RMS, like room tone or (loud) speech.
    fn noise(n: usize, level: f64, seed: u32) -> Vec<i16> {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                // uniform in [-1, 1) has RMS 1/sqrt(3)
                let u = (f64::from(x) / f64::from(u32::MAX)) * 2.0 - 1.0;
                (u * level * 3f64.sqrt()) as i16
            })
            .collect()
    }

    fn feed(gate: &mut SilenceGate, s: &[i16], chunk: usize) -> Option<usize> {
        let mut fed = Vec::new();
        for c in s.chunks(chunk) {
            fed.extend_from_slice(c);
            if gate.should_stop(&fed) {
                return Some(fed.len());
            }
        }
        None
    }

    #[test]
    fn adaptive_threshold_respects_the_minimum() {
        assert_eq!(adaptive_threshold(35.0), MIN_SILENCE_LEVEL);
        assert_eq!(adaptive_threshold(0.0), MIN_SILENCE_LEVEL);
        assert_eq!(adaptive_threshold(100.0), 400.0);
    }

    #[test]
    fn adaptive_quiet_room_then_speech_then_silence_stops() {
        let mut s = noise(8_000, 35.0, 1); // 0.5 s room tone
        s.extend(noise(16_000, 1500.0, 2)); // 1 s speech
        let speech_end = s.len();
        s.extend(noise(48_000, 40.0, 3)); // 3 s room tone
        let mut gate = SilenceGate::adaptive(1.0);
        let at = feed(&mut gate, &s, 768).expect("never stopped");
        let floor = gate.floor().unwrap();
        assert!((25.0..50.0).contains(&floor), "floor {floor}");
        assert_eq!(gate.threshold(), Some(MIN_SILENCE_LEVEL));
        assert!(gate.heard_speech());
        let after = (at - speech_end) as f64 / f64::from(SAMPLE_RATE);
        assert!((1.0..1.3).contains(&after), "stopped {after}s after speech");
    }

    #[test]
    fn adaptive_pure_room_tone_never_counts_as_heard() {
        // Room tone across the measured spread (blocks 30-44 RMS) plus a few louder blocks at 60.
        let mut s = Vec::new();
        for i in 0..80u32 {
            let level = if i % 13 == 7 {
                60.0
            } else {
                30.0 + f64::from(i % 15)
            };
            s.extend(noise(BLOCK_FRAMES, level, i + 10));
        }
        let mut gate = SilenceGate::adaptive(0.5);
        assert_eq!(feed(&mut gate, &s, 1000), None);
        assert!(!gate.heard_speech());
        assert!(gate.threshold().is_some());
    }

    #[test]
    fn adaptive_speech_from_the_first_block_is_heard() {
        let mut s = noise(2 * BLOCK_FRAMES, 1500.0, 4);
        s.extend(noise(3 * BLOCK_FRAMES, 35.0, 5));
        s.extend(noise(32_000, 35.0, 6));
        let mut gate = SilenceGate::adaptive(1.0);
        assert!(feed(&mut gate, &s, 1024).is_some());
        assert!(gate.heard_speech());
        assert!(gate.floor().unwrap() < 50.0);
    }

    /// Records four seconds and prints the noise floor, the adaptive threshold and every block's
    /// RMS. Opens the mic: stay quiet for the first half second, then speak.
    /// `cargo test -p platform live_adaptive_gate -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_adaptive_gate() {
        let s = record_until(|| false, 4.0, 0.0).expect("microphone");
        let mut gate = SilenceGate::adaptive(1e9);
        gate.should_stop(&s);
        let threshold = gate.threshold().unwrap_or(f64::NAN);
        println!(
            "samples {} ({:.2}s), floor {:.1}, threshold {:.1} (K {NOISE_FACTOR}, min {MIN_SILENCE_LEVEL}), override {:?}",
            s.len(),
            s.len() as f64 / f64::from(SAMPLE_RATE),
            gate.floor().unwrap_or(f64::NAN),
            threshold,
            silence_level_override()
        );
        for (i, b) in s.chunks_exact(BLOCK_FRAMES).enumerate() {
            let r = rms(b);
            println!(
                "block {i:3} t {:.2}s rms {r:8.1} {}",
                (i * BLOCK_FRAMES) as f64 / f64::from(SAMPLE_RATE),
                if r >= threshold { "SPEECH" } else { "" }
            );
        }
        println!("heard speech: {}", gate.heard_speech());
        assert!(s.len() > 56_000);
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
