//! Hands-on voice checks against the real services and the real audio devices.
//!
//!   voice_tool gen "<text>" <out.wav> [voice]   synthesise with Murf into a 24 kHz WAV
//!   voice_tool stt <in.wav> [language]          stream a WAV to Deepgram at real-time pace
//!   voice_tool play <in.wav>                    play a WAV through the default speakers
//!
//! Keys come from the environment (`MURF_API_KEY`, `DEEPGRAM_API_KEY`) and are never printed.

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use api::deepgram::{self, Heard, Stream};
use api::murf::{self, Murf};
use platform::audio;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("gen") if args.len() >= 3 => gen(&args[1], &args[2], args.get(3).map(String::as_str)),
        Some("stt") if args.len() >= 2 => stt(&args[1], args.get(2).map(String::as_str).unwrap_or("en")),
        Some("play") if args.len() >= 2 => play(&args[1]),
        _ => Err("usage: voice_tool gen <text> <out.wav> [voice] | stt <in.wav> [lang] | play <in.wav>".into()),
    };
    if let Err(e) = code {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn gen(text: &str, out: &str, voice: Option<&str>) -> Result<(), String> {
    let client = Murf::from_env().ok_or("MURF_API_KEY is not set")?;
    let voice = voice.map(str::to_string).unwrap_or_else(murf::voice_from_env);
    let began = Instant::now();
    let mut first = None;
    let mut samples: Vec<i16> = Vec::new();
    client.stream(text, &voice, |chunk| {
        first.get_or_insert(began.elapsed());
        samples.extend_from_slice(chunk);
        true
    })?;
    std::fs::write(out, audio::to_wav(&samples, murf::SAMPLE_RATE)).map_err(|e| e.to_string())?;
    println!(
        "murf voice={voice} first_audio_ms={} total_ms={} audio_s={:.2} -> {out}",
        first.map_or(0, |d| d.as_millis()),
        began.elapsed().as_millis(),
        samples.len() as f64 / f64::from(murf::SAMPLE_RATE)
    );
    Ok(())
}

fn load(path: &str) -> Result<(u32, Vec<i16>), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    audio::parse_wav(&bytes).ok_or_else(|| format!("{path} is not a 16-bit PCM WAV"))
}

fn stt(path: &str, language: &str) -> Result<(), String> {
    let (rate, samples) = load(path)?;
    let mut pcm = audio::resample(&samples, rate, audio::SAMPLE_RATE);
    pcm.extend(std::iter::repeat_n(0i16, audio::SAMPLE_RATE as usize * 2)); // two seconds of quiet
    let key = std::env::var("DEEPGRAM_API_KEY").map_err(|_| "DEEPGRAM_API_KEY is not set")?;
    let params = deepgram::Params::new(&deepgram::language_for(language));
    let began = Instant::now();
    let mut stream = Stream::connect(key.trim(), &params)?;
    println!("connected in {} ms (language {})", began.elapsed().as_millis(), params.language);
    let sent = Instant::now();
    let mut first_interim = None;
    let mut finals = Vec::new();
    for chunk in pcm.chunks(1024) {
        stream.send_audio(chunk)?;
        std::thread::sleep(Duration::from_millis(64));
        for heard in stream.poll()? {
            match heard {
                Heard::Interim(t) => {
                    first_interim.get_or_insert(sent.elapsed());
                    println!("  interim @{:>5} ms: {t}", sent.elapsed().as_millis());
                }
                Heard::Final(t) => {
                    println!("  FINAL   @{:>5} ms: {t}", sent.elapsed().as_millis());
                    finals.push(t);
                }
            }
        }
    }
    for heard in stream.finish(Duration::from_secs(3)) {
        if let Heard::Final(t) = heard {
            println!("  FINAL (flush): {t}");
            finals.push(t);
        }
    }
    println!(
        "heard={:?} first_interim_ms={}",
        finals.join(" "),
        first_interim.map_or(-1, |d| d.as_millis() as i64)
    );
    Ok(())
}

fn play(path: &str) -> Result<(), String> {
    let (rate, samples) = load(path)?;
    let (tx, rx) = channel();
    for chunk in samples.chunks(2048) {
        tx.send(chunk.to_vec()).map_err(|e| e.to_string())?;
    }
    drop(tx);
    let began = Instant::now();
    let end = audio::play_stream(rate, &rx, || false, 50, || {
        println!("playback started at {} ms", began.elapsed().as_millis());
    })?;
    println!("playback {end:?} after {} ms", began.elapsed().as_millis());
    Ok(())
}
