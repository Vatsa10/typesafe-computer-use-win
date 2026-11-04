//! Measurements for the voice path, against the real services and devices. Keys come from the
//! environment and are never printed; transcripts are only counted, except `--show`.
//!
//!   voice_lab endpoint <in.wav> [noise_rms] [old]   real-time stream through the gate; when did
//!                                                    Finalize go and when did the final arrive,
//!                                                    measured from the real end of speech
//!   voice_lab tts <rumik|murf> "<text>" [n]         first-audio latency, n runs, no sound
//!   voice_lab aec <in.wav> [lead_s]                  PLAYS the wav aloud once, records the mic,
//!                                                    cancels offline, reports levels and leaked words

use std::time::{Duration, Instant};

use api::deepgram::{self, Endpointer, Heard, Stream};
use platform::audio;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let r = match a.first().map(String::as_str) {
        Some("endpoint") if a.len() >= 2 => endpoint(&a[1], a.get(2).and_then(|v| v.parse().ok()).unwrap_or(0.0), a.get(3).is_some_and(|v| v == "old")),
        Some("tts") if a.len() >= 3 => tts(&a[1], &a[2], a.get(3).and_then(|v| v.parse().ok()).unwrap_or(3)),
        Some("words") if a.len() >= 2 => words(&a[1]),
        Some("offline") if a.len() >= 3 => offline(&a[1], &a[2]),
        Some("aec") if a.len() >= 2 => aec(&a[1], a.get(2).and_then(|v| v.parse().ok()).unwrap_or(1.0)),
        _ => Err("usage: voice_lab endpoint <wav> [noise] [old] | tts <engine> <text> [n] | aec <wav>".into()),
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn load16(path: &str) -> Result<Vec<i16>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let (rate, s) = audio::parse_wav(&bytes).ok_or("not a 16-bit PCM wav")?;
    Ok(audio::resample(&s, rate, audio::SAMPLE_RATE))
}

fn key() -> Result<String, String> {
    Ok(std::env::var("DEEPGRAM_API_KEY").map_err(|_| "DEEPGRAM_API_KEY is not set")?.trim().to_string())
}

fn endpoint(path: &str, noise: f64, old: bool) -> Result<(), String> {
    let mut pcm = load16(path)?;
    // where speech really ends: the last 64 ms block well above the file's own quiet
    let blocks: Vec<f64> = pcm.chunks(1024).map(audio::rms).collect();
    let loud = blocks.iter().cloned().fold(0.0, f64::max);
    let last = blocks.iter().rposition(|&r| r > loud * 0.15).unwrap_or(0);
    let end_of_speech_ms = ((last + 1) * 1024 * 1000 / 16_000) as u64;
    pcm.extend(std::iter::repeat_n(0i16, 16_000 * 4));
    if noise > 0.0 {
        let mut x: u64 = 99;
        for s in pcm.iter_mut() {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = (((x >> 33) as f64 / 2147483648.0) - 0.5) * 2.0 * noise * 1.7;
            *s = (f64::from(*s) + n).clamp(-32768.0, 32767.0) as i16;
        }
    }
    let params = deepgram::Params::new("en");
    let mut stream = Stream::connect(&key()?, &params)?;
    let mut gate = audio::SilenceGate::adaptive(if old { 0.7 } else { 0.45 });
    let mut end = Endpointer::default();
    let began = Instant::now();
    let (mut fin_sent, mut final_at, mut text_len) = (None, None, 0);
    let mut sent = 0usize;
    for chunk in pcm.chunks(1024) {
        stream.send_audio(chunk)?;
        sent += chunk.len();
        std::thread::sleep(Duration::from_millis(64));
        let now = began.elapsed().as_millis() as u64;
        let quiet = gate.should_stop(&pcm[..sent]);
        for h in stream.poll()? {
            match h {
                Heard::Interim(t) => end.words(&t, now),
                Heard::Final(t) => {
                    final_at.get_or_insert(now);
                    text_len = t.chars().count();
                    end.closed();
                }
            }
        }
        let fire = if old { quiet && fin_sent.is_none() && stream.pending() } else { end.decide(quiet, now, stream.pending()) };
        if fire && fin_sent.is_none() {
            fin_sent = Some(now);
            stream.finalize()?;
        }
        if final_at.is_some() {
            break;
        }
    }
    println!(
        "{} noise_rms={noise}: speech ended {end_of_speech_ms} ms | Finalize sent {} ms after | final transcript {} ms after | chars {text_len}",
        if old { "OLD(gate 0.7s)" } else { "NEW(endpointer)" },
        fin_sent.map_or("never".into(), |t| (t as i64 - end_of_speech_ms as i64).to_string()),
        final_at.map_or("never".into(), |t| (t as i64 - end_of_speech_ms as i64).to_string()),
    );
    Ok(())
}

fn tts(engine: &str, text: &str, n: usize) -> Result<(), String> {
    let mut firsts = Vec::new();
    for i in 0..n {
        let began = Instant::now();
        let mut first = None;
        let mut total = 0usize;
        let mut cb = |c: &[i16]| {
            first.get_or_insert(began.elapsed().as_millis());
            total += c.len();
            true
        };
        match engine {
            "rumik" => api::rumik::Rumik::from_env().ok_or("no RUMIK_API_KEY")?.stream(text, &mut cb)?,
            "murf" => api::murf::Murf::from_env().ok_or("no MURF_API_KEY")?.stream(text, &api::murf::voice_from_env(), &mut cb)?,
            _ => return Err("engine is rumik or murf".into()),
        }
        println!("{engine} run {}: first audio {} ms, total {} ms, {:.1}s of audio", i + 1, first.unwrap_or(0), began.elapsed().as_millis(), total as f64 / 24000.0);
        firsts.push(first.unwrap_or(0));
    }
    firsts.sort();
    println!("{engine} median first audio {} ms", firsts[firsts.len() / 2]);
    Ok(())
}

fn aec(path: &str, lead: f64) -> Result<(), String> {
    std::env::set_var("POINTER_AEC", "0"); // record raw; cancel offline so both can be compared
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let (rate, samples) = audio::parse_wav(&bytes).ok_or("not a wav")?;
    let secs = samples.len() as f64 / f64::from(rate) + lead + 1.5;
    let (tx, rx) = std::sync::mpsc::channel();
    for c in samples.chunks(2400) {
        tx.send(c.to_vec()).unwrap();
    }
    drop(tx);
    let player = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs_f64(lead));
        audio::play_stream(rate, &rx, || false, 80, || {})
    });
    let mut blocks: Vec<(Instant, Vec<i16>)> = Vec::new();
    let t0 = Instant::now();
    let (mut clock, mut total) = (audio::CaptureClock::default(), 0u64);
    audio::record_stream(|| t0.elapsed().as_secs_f64() > secs, secs + 1.0, 0.0, |s, _| {
        total += s.len() as u64;
        blocks.push((clock.stamp(Instant::now(), total), s.to_vec()));
        true
    })?;
    player.join().map_err(|_| "player panicked")??;
    // the delay between what was queued for the speaker and what the mic hears of it
    {
        let mut refcat: Vec<i16> = Vec::new();
        for (at, b) in &blocks {
            refcat.extend(audio::reference_window(*at, b.len()).unwrap_or_else(|| vec![0; b.len()]));
        }
        let raw: Vec<i16> = blocks.iter().flat_map(|(_, b)| b.iter().copied()).collect();
        let span = 32_000.min(raw.len() / 2);
        let from = (lead * 16000.0) as usize + 16_000;
        let (mut best, mut best_lag) = (0f64, 0usize);
        for lag in (0..20_000usize).step_by(8) {
            if from + span > raw.len() || from < lag { break; }
            let c: f64 = (from..from + span).map(|i| f64::from(raw[i]) * f64::from(refcat[i - lag])).sum();
            if c.abs() > best { best = c.abs(); best_lag = lag; }
        }
        println!("strongest echo lag {} ms (filter covers {} ms)", best_lag / 16, audio::AEC_TAPS / 16);
    }
    {
        let raw: Vec<i16> = blocks.iter().flat_map(|(_, b)| b.iter().copied()).collect();
        let mut refcat: Vec<i16> = Vec::new();
        for (at, b) in &blocks {
            refcat.extend(audio::reference_window(*at, b.len()).unwrap_or_else(|| vec![0; b.len()]));
        }
        let _ = std::fs::write(format!("{path}.mic.wav"), audio::to_wav(&raw, 16000));
        let _ = std::fs::write(format!("{path}.ref.wav"), audio::to_wav(&refcat, 16000));
    }
    let mut canceller = audio::EchoCanceller::new(audio::AEC_TAPS);
    let (mut raw, mut clean): (Vec<i16>, Vec<i16>) = (Vec::new(), Vec::new());
    for (at, b) in &blocks {
        raw.extend_from_slice(b);
        let mut c = b.clone();
        if audio::reference_recent(*at) {
            if let Some(r) = audio::reference_window(*at, c.len()) {
                canceller.process(&mut c, &r);
            }
        }
        clean.extend_from_slice(&c);
    }
    let from = (lead * 16000.0) as usize;
    let to = (raw.len()).min(from + (samples.len() as f64 / f64::from(rate) * 16000.0) as usize);
    let half = from + (to - from) / 3; // skip the first third: the filter is still adapting
    let level = |v: &[i16]| audio::rms(v);
    println!("rms during playback (after first third): raw {:.0}  cancelled {:.0}  -> {:.1} dB removed", level(&raw[half..to]), level(&clean[half..to]), 20.0 * (level(&raw[half..to]) / level(&clean[half..to]).max(1.0)).log10());
    println!("rms of whole playback:                   raw {:.0}  cancelled {:.0}", level(&raw[from..to]), level(&clean[from..to]));
    for (name, pcm) in [("raw", &raw), ("cancelled", &clean)] {
        let words = transcribe(pcm)?;
        println!("{name}: Deepgram heard {words} words");
    }
    Ok(())
}

fn transcribe(pcm: &[i16]) -> Result<usize, String> {
    let params = deepgram::Params::new("en");
    let mut stream = Stream::connect(&key()?, &params)?;
    let mut finals = Vec::new();
    for chunk in pcm.chunks(2048) {
        stream.send_audio(chunk)?;
        std::thread::sleep(Duration::from_millis(20));
        for h in stream.poll()? {
            if let Heard::Final(t) = h {
                finals.push(t);
            }
        }
    }
    for h in stream.finish(Duration::from_secs(4)) {
        if let Heard::Final(t) = h {
            finals.push(t);
        }
    }
    if std::env::var("LAB_SHOW").is_ok() {
        eprintln!("heard: {finals:?}");
    }
    Ok(finals.join(" ").split_whitespace().count())
}

/// Re-run the canceller on a recorded mic and its aligned reference, with LAB_TAPS / LAB_MU.
fn offline(mic: &str, reference: &str) -> Result<(), String> {
    let raw = load16(mic)?;
    let refc = load16(reference)?;
    let taps = std::env::var("LAB_TAPS").ok().and_then(|v| v.parse().ok()).unwrap_or(audio::AEC_TAPS);
    let mu = std::env::var("LAB_MU").ok().and_then(|v| v.parse().ok()).unwrap_or(0.5f32);
    let mut c = audio::EchoCanceller::new(taps);
    c.mu = mu;
    let mut clean = raw.clone();
    let passes: usize = std::env::var("LAB_PASSES").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    for _ in 0..passes {
        clean = raw.clone();
        for (m, r) in clean.chunks_mut(1024).zip(refc.chunks(1024)) {
            if m.len() == r.len() && r.iter().any(|&v| v != 0) {
                c.process(m, r);
            }
        }
    }
    let active: Vec<usize> = refc.iter().enumerate().filter(|(_, &v)| v != 0).map(|(i, _)| i).collect();
    let (a, b) = (active[0], *active.last().unwrap());
    let h = a + (b - a) / 3;
    println!("ambient (before playback) rms {:.0}", audio::rms(&raw[..a.saturating_sub(800).max(1)]));
    println!("taps {taps} mu {mu}: removed {:.1} dB (after first third), {:.1} dB over all", 20.0 * (audio::rms(&raw[h..b]) / audio::rms(&clean[h..b]).max(1.0)).log10(), 20.0 * (audio::rms(&raw[a..b]) / audio::rms(&clean[a..b]).max(1.0)).log10());
    if std::env::var("LAB_STT").is_ok() {
        println!("raw words {} cancelled words {}", transcribe(&raw)?, transcribe(&clean)?);
    }
    Ok(())
}

fn words(path: &str) -> Result<(), String> {
    let pcm = load16(path)?;
    let from = 16_000.min(pcm.len() / 4);
    println!(
        "{}: ambient rms {:.0}, rms after 1 s {:.0}, Deepgram heard {} words",
        path.rsplit('/').next().unwrap_or(""),
        audio::rms(&pcm[..from]),
        audio::rms(&pcm[from..]),
        transcribe(&pcm)?
    );
    Ok(())
}
