//! Audio playback, voice-message recording, and the audio of a call.
//!
//! Input and output devices are opened on demand and released when idle.

use std::collections::HashMap;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rodio::Source;
use rodio::buffer::SamplesBuffer;
use whatsapp_rust::async_channel::{self, TrySendError};
use whatsapp_rust::voip::audio::{WA_FRAME_SAMPLES, WA_SAMPLE_RATE};

use crate::backend::Waker;
use crate::voice;

/// Maximum recording length. The phone uses a shorter limit.
const LONGEST_RECORDING: Duration = Duration::from_secs(15 * 60);

fn mono() -> NonZero<u16> {
    NonZero::<u16>::MIN
}

fn rate() -> NonZero<u32> {
    NonZero::new(voice::RATE).expect("48 kHz is not zero")
}

/// Opens the default output device for playback.
///
/// rodio reports the sink's drop through `stderr` by default. A desktop launch
/// can have that closed: ZapFast inherits `stderr` from whatever started it,
/// and that process can exit while ZapFast runs on. Rust ignores `SIGPIPE`, so
/// the next write there fails with `Broken pipe` and the print macro panics,
/// which aborts the whole app in a release build. Keep it off, and report
/// failures of our own through the log instead.
pub fn open_output() -> Result<rodio::MixerDeviceSink, rodio::DeviceSinkError> {
    let mut output = rodio::DeviceSinkBuilder::open_default_sink()?;
    output.log_on_drop(false);
    Ok(output)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Loading,
    Playing,
    Paused,
}

/// Playback state for one message.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub state: State,
    pub position: Duration,
    pub total: Duration,
}

impl Status {
    const IDLE: Self = Self {
        state: State::Idle,
        position: Duration::ZERO,
        total: Duration::ZERO,
    };
}

/// Playback speeds, in ascending order, matching the phone.
pub const SPEEDS: [f32; 5] = [1.0, 1.25, 1.5, 1.75, 2.0];

/// Speeds the speed chip cycles through, as on the phone. The others are
/// chosen from the message menu.
pub const CYCLED_SPEEDS: [f32; 3] = [1.0, 1.5, 2.0];

/// The speed after `speed` when the chip is clicked: the next faster cycled
/// speed, wrapping from 2x back to 1x.
pub fn next_cycled_speed(speed: f32) -> f32 {
    CYCLED_SPEEDS
        .into_iter()
        .find(|&candidate| candidate > speed)
        .unwrap_or(CYCLED_SPEEDS[0])
}

/// The supported speed nearest to `speed`; non-finite speeds give 1x.
pub fn supported_speed(speed: f32) -> f32 {
    if !speed.is_finite() {
        return SPEEDS[0];
    }
    SPEEDS
        .into_iter()
        .min_by(|a, b| (a - speed).abs().total_cmp(&(b - speed).abs()))
        .unwrap_or(SPEEDS[0])
}

/// Label for a playback speed, like `1x`, `1.25x`, or `1.5x`.
pub fn speed_label(speed: f32) -> String {
    if speed.fract() == 0.0 {
        format!("{}x", speed as i32)
    } else {
        // Keep both decimals for 1.25 and 1.75; drop the trailing zero on 1.5.
        let text = format!("{speed:.2}");
        let text = text.trim_end_matches('0').trim_end_matches('.');
        format!("{text}x")
    }
}

type Decoded = Arc<Mutex<Option<Result<Vec<f32>, String>>>>;

/// Plays one clip at a time through the default output device.
pub struct Player {
    waker: Waker,
    output: Option<(rodio::MixerDeviceSink, rodio::Player)>,
    loaded: Option<Loaded>,
    decoding: Option<Decoding>,
    /// Playback speed applied to the current clip and to later ones.
    speed: f32,
    /// Time-compressed copies of the loaded clip, one per speed already
    /// built, dropped when the clip changes.
    stretches: Vec<(f32, Arc<Vec<f32>>)>,
    /// Compression being built for the loaded message.
    stretching: Option<Stretching>,
    /// Generated waveforms for clips that did not include one.
    bars: HashMap<String, Vec<u8>>,
    /// Message whose clip just played to its end, waiting to be taken.
    finished: Option<String>,
}

struct Loaded {
    message: String,
    samples: Arc<Vec<f32>>,
    /// Samples queued in the sink: the clip itself or its compression.
    buffer: Arc<Vec<f32>>,
    /// Speed the queued buffer represents; 1 plays the clip as recorded.
    factor: f32,
    /// Restart position in the clip's own timeline.
    base: Duration,
    paused: bool,
    done: bool,
}

struct Stretching {
    factor: f32,
    slot: StretchedSlot,
    /// Set when this job is replaced or the clip changes, so the worker
    /// stops instead of piling up behind the next one.
    cancelled: Arc<AtomicBool>,
}

impl Drop for Stretching {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

type StretchedSlot = Arc<Mutex<Option<Arc<Vec<f32>>>>>;

struct Decoding {
    message: String,
    /// Requested start position after decoding, from 0 to 1.
    start: f32,
    slot: Decoded,
}

impl Player {
    pub fn new(waker: Waker) -> Self {
        Self {
            waker,
            output: None,
            loaded: None,
            decoding: None,
            speed: SPEEDS[0],
            stretches: Vec::new(),
            stretching: None,
            bars: HashMap::new(),
            finished: None,
        }
    }

    /// Current playback speed multiplier.
    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// Sets the playback speed for the clip playing now and for later ones.
    ///
    /// Speeds above 1x play a time-compressed copy of the clip, once it has
    /// been built, so the voice keeps its pitch. Until then playback
    /// continues at the speed already queued. Any other speed, such as a
    /// hand-edited setting, snaps to the nearest one in [`SPEEDS`], so a speed
    /// control always shows it. Returns the speed that applies.
    pub fn set_speed(&mut self, speed: f32) -> f32 {
        self.speed = supported_speed(speed);
        self.apply_speed();
        self.ensure_stretch();
        self.speed
    }

    /// Whether `message` is still playing at an earlier speed while the
    /// compression for the chosen one builds.
    pub fn preparing_speed(&self, message: &str) -> bool {
        self.loaded.as_ref().is_some_and(|loaded| {
            loaded.message == message && !loaded.done && loaded.factor != self.speed
        })
    }

    /// The samples that play at `speed` and the speed they represent: the
    /// clip itself at 1x, its compression once built, and otherwise whatever
    /// is queued, so a speed still building does not drop playback to 1x.
    fn buffer_for(
        loaded: &Loaded,
        stretches: &[(f32, Arc<Vec<f32>>)],
        speed: f32,
    ) -> (Arc<Vec<f32>>, f32) {
        if speed <= 1.0 {
            return (Arc::clone(&loaded.samples), 1.0);
        }
        match stretches.iter().find(|(factor, _)| *factor == speed) {
            Some((factor, compressed)) => (Arc::clone(compressed), *factor),
            None => (Arc::clone(&loaded.buffer), loaded.factor),
        }
    }

    /// Restarts playback on the buffer for the current speed, keeping the
    /// position, when it differs from what is queued.
    fn apply_speed(&mut self) {
        let Some(loaded) = self.loaded.as_ref() else {
            return;
        };
        let (wanted, _) = Self::buffer_for(loaded, &self.stretches, self.speed);
        if self.output.is_none() || Arc::ptr_eq(&wanted, &loaded.buffer) {
            return;
        }
        let total = clip_length(loaded.samples.len());
        let fraction = if total > Duration::ZERO {
            (self.status(&loaded.message).position.as_secs_f64() / total.as_secs_f64()) as f32
        } else {
            0.0
        }
        .clamp(0.0, 1.0);
        let paused = loaded.paused;
        if self.restart(fraction).is_ok() && paused {
            if let Some((_, sink)) = &self.output {
                sink.pause();
            }
            if let Some(loaded) = self.loaded.as_mut() {
                loaded.paused = true;
            }
        }
    }

    /// Builds the compression for the current speed in the background, if it
    /// is still missing. Replacing an outstanding job cancels it, and so does
    /// going back to 1x, which needs none.
    fn ensure_stretch(&mut self) {
        let factor = self.speed;
        if factor <= 1.0 {
            self.stretching = None;
            return;
        }
        if self.stretches.iter().any(|(built, _)| *built == factor)
            || self
                .stretching
                .as_ref()
                .is_some_and(|job| job.factor == factor)
        {
            return;
        }
        let Some(loaded) = &self.loaded else {
            return;
        };
        let samples = Arc::clone(&loaded.samples);
        let waker = self.waker.clone();
        let slot: StretchedSlot = Default::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_slot = Arc::clone(&slot);
        let thread_cancelled = Arc::clone(&cancelled);
        let spawned = std::thread::Builder::new()
            .name("voice-stretch".to_owned())
            .spawn(move || {
                let Some(compressed) =
                    crate::timestretch::speed_up_unless(&samples, factor, &thread_cancelled)
                else {
                    return;
                };
                *thread_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(compressed));
                waker.wake();
            });
        if spawned.is_ok() {
            self.stretching = Some(Stretching {
                factor,
                slot,
                cancelled,
            });
        }
    }

    /// Plays or pauses a message. Finished clips restart; new clips decode first.
    pub fn toggle(&mut self, message: &str, path: &Path) -> Result<(), String> {
        match self.loaded.as_mut() {
            Some(loaded) if loaded.message == message => {
                if loaded.done {
                    return self.restart(0.0);
                }
                if let Some((_, sink)) = &self.output {
                    if loaded.paused {
                        sink.play();
                    } else {
                        sink.pause();
                    }
                    loaded.paused = !loaded.paused;
                }
                Ok(())
            }
            _ => self.load(message, path, 0.0),
        }
    }

    /// Seeks to a fraction from 0 to 1 and starts playback.
    pub fn seek(&mut self, message: &str, path: &Path, fraction: f32) -> Result<(), String> {
        match &self.loaded {
            Some(loaded) if loaded.message == message => self.restart(fraction),
            _ => self.load(message, path, fraction),
        }
    }

    /// Clears the loaded clip and releases the output device.
    pub fn stop(&mut self) {
        self.output = None;
        self.loaded = None;
        self.decoding = None;
        self.stretches.clear();
        self.stretching = None;
        // A clip the reader stopped is not one that played to its end.
        self.finished = None;
    }

    /// Takes the message whose clip just played to its end, once. The app uses
    /// it to carry on with the next unplayed voice message.
    pub fn take_finished(&mut self) -> Option<String> {
        self.finished.take()
    }

    /// Whether audio is currently playing.
    pub fn is_playing(&self) -> bool {
        self.decoding.is_some()
            || self
                .loaded
                .as_ref()
                .is_some_and(|loaded| !loaded.paused && !loaded.done)
    }

    pub fn status(&self, message: &str) -> Status {
        if let Some(decoding) = &self.decoding
            && decoding.message == message
        {
            return Status {
                state: State::Loading,
                ..Status::IDLE
            };
        }
        match &self.loaded {
            Some(loaded) if loaded.message == message => {
                let total = clip_length(loaded.samples.len());
                if loaded.done {
                    return Status {
                        state: State::Idle,
                        position: Duration::ZERO,
                        total,
                    };
                }
                let position = self
                    .output
                    .as_ref()
                    .map(|(_, sink)| loaded.base + sink.get_pos().mul_f32(loaded.factor))
                    .unwrap_or(loaded.base)
                    .min(total);
                Status {
                    state: if loaded.paused {
                        State::Paused
                    } else {
                        State::Playing
                    },
                    position,
                    total,
                }
            }
            _ => Status::IDLE,
        }
    }

    /// Generated waveform for a decoded clip.
    pub fn bars(&self, message: &str) -> Option<&[u8]> {
        self.bars.get(message).map(Vec::as_slice)
    }

    /// Handles completed decodes and finished playback once per frame.
    pub fn poll(&mut self) -> Result<(), String> {
        let decoded = self.decoding.as_ref().and_then(|decoding| {
            decoding
                .slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
        });
        if let Some(result) = decoded {
            let Decoding { message, start, .. } = self.decoding.take().expect("just seen");
            let samples = result?;
            if samples.is_empty() {
                return Err("The clip is empty".to_owned());
            }
            let samples = Arc::new(samples);
            self.bars
                .entry(message.clone())
                .or_insert_with(|| voice::waveform(&samples));
            self.loaded = Some(Loaded {
                message,
                buffer: Arc::clone(&samples),
                factor: 1.0,
                samples,
                base: Duration::ZERO,
                paused: false,
                done: false,
            });
            self.restart(start)?;
            self.ensure_stretch();
        }
        let compressed = self
            .stretching
            .as_ref()
            .and_then(|job| job.slot.lock().unwrap_or_else(|p| p.into_inner()).take());
        if let Some(samples) = compressed {
            let factor = self.stretching.take().expect("just seen").factor;
            self.stretches.push((factor, samples));
            self.apply_speed();
            // The speed may have moved on while this compression built.
            self.ensure_stretch();
        }
        let ended = match (&mut self.loaded, &self.output) {
            (Some(loaded), Some((_, sink))) if !loaded.done && !loaded.paused && sink.empty() => {
                loaded.done = true;
                true
            }
            _ => false,
        };
        if ended {
            // Release the device after playback ends.
            self.output = None;
            self.finished = self.loaded.as_ref().map(|loaded| loaded.message.clone());
        }
        Ok(())
    }

    fn load(&mut self, message: &str, path: &Path, start: f32) -> Result<(), String> {
        self.stop();
        let slot: Decoded = Default::default();
        let path = path.to_owned();
        let waker = self.waker.clone();
        let thread_slot = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("voice-decode".to_owned())
            .spawn(move || {
                let result = decode_file(&path);
                *thread_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
                waker.wake();
            });
        if let Err(error) = spawned {
            return Err(format!("Could not decode audio: {error}"));
        }
        self.decoding = Some(Decoding {
            message: message.to_owned(),
            start,
            slot,
        });
        Ok(())
    }

    /// Plays the loaded clip from a fraction from 0 to 1.
    fn restart(&mut self, fraction: f32) -> Result<(), String> {
        let Some(loaded) = self.loaded.as_mut() else {
            return Ok(());
        };
        // The sink always plays at 1x: running it faster sharpens the voice,
        // so speeds above 1x queue a time-compressed copy of the clip.
        let (buffer, factor) = Self::buffer_for(loaded, &self.stretches, self.speed);
        let total = clip_length(loaded.samples.len());
        let offset = ((fraction.clamp(0.0, 1.0) * buffer.len() as f32) as usize).min(buffer.len());
        if self.output.is_none() {
            let device = open_output().map_err(|error| format!("No sound output: {error}"))?;
            let sink = rodio::Player::connect_new(device.mixer());
            self.output = Some((device, sink));
        }
        let (_, sink) = self.output.as_ref().expect("just opened");
        sink.clear();
        sink.append(SamplesBuffer::new(
            mono(),
            rate(),
            buffer[offset..].to_vec(),
        ));
        sink.play();
        loaded.buffer = buffer;
        loaded.factor = factor;
        loaded.base =
            Duration::from_secs_f64(fraction.clamp(0.0, 1.0) as f64 * total.as_secs_f64());
        loaded.paused = false;
        loaded.done = false;
        Ok(())
    }
}

fn clip_length(samples: usize) -> Duration {
    Duration::from_secs_f64(samples as f64 / f64::from(voice::RATE))
}

/// Decodes a file to mono 48 kHz samples. OGG/Opus uses `voice`; other
/// supported formats use rodio.
fn decode_file(path: &Path) -> Result<Vec<f32>, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("Could not read the audio: {error}"))?;
    if bytes.starts_with(b"OggS")
        && let Ok(samples) = voice::decode(&bytes)
    {
        return Ok(samples);
    }
    let file =
        std::fs::File::open(path).map_err(|error| format!("Could not read the audio: {error}"))?;
    let decoder = rodio::Decoder::new(std::io::BufReader::new(file))
        .map_err(|error| format!("Could not decode the audio: {error}"))?;
    let channels = decoder.channels().get();
    let rate = decoder.sample_rate().get();
    let interleaved: Vec<f32> = decoder.collect();
    Ok(voice::mono_at_rate(&interleaved, channels, rate))
}

type Outcome = Arc<Mutex<Option<Result<Vec<f32>, String>>>>;

/// Records until told to stop, pushing a level per 50 ms, and returns the
/// mono 48 kHz samples.
type Take = fn(&AtomicBool, &Mutex<Vec<f32>>, &Waker) -> Result<Vec<f32>, String>;

/// Records from the default microphone until told to stop.
pub struct Recorder {
    started: Instant,
    stop: Arc<AtomicBool>,
    /// Loudness for each recorded 50 ms segment.
    levels: Arc<Mutex<Vec<f32>>>,
    outcome: Outcome,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    pub fn start(waker: Waker) -> Self {
        Self::spawn(waker, record)
    }

    /// Records a synthetic voice instead of the microphone, at the pace a
    /// real take would, for offline demos: the waveform grows while it runs
    /// and sending it yields that many seconds of a speech-like tone.
    #[cfg(any(test, feature = "demo"))]
    pub fn simulated(waker: Waker) -> Self {
        Self::spawn(waker, rehearse)
    }

    fn spawn(waker: Waker, body: Take) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let levels: Arc<Mutex<Vec<f32>>> = Default::default();
        let outcome: Outcome = Default::default();
        let spawned = {
            let stop = Arc::clone(&stop);
            let levels = Arc::clone(&levels);
            let outcome = Arc::clone(&outcome);
            std::thread::Builder::new()
                .name("voice-record".to_owned())
                .spawn(move || {
                    let result = body(&stop, &levels, &waker);
                    *outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
                    waker.wake();
                })
        };
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(error) => {
                *outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(Err(error.to_string()));
                None
            }
        };
        Self {
            started: Instant::now(),
            stop,
            levels,
            outcome,
            thread,
        }
    }

    /// Simulated recorder for demos and tests.
    #[cfg(any(test, feature = "demo"))]
    pub fn rehearsal() -> Self {
        let levels: Vec<f32> = (0..90)
            .map(|index| 0.05 + 0.2 * ((index as f32 * 0.6).sin().abs()))
            .collect();
        Self {
            started: Instant::now() - Duration::from_millis(4_500),
            stop: Arc::new(AtomicBool::new(true)),
            levels: Arc::new(Mutex::new(levels)),
            outcome: Default::default(),
            thread: None,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn levels(&self) -> Vec<f32> {
        self.levels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Error that stopped recording early.
    pub fn failure(&self) -> Option<String> {
        match self
            .outcome
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            Some(Err(error)) => Some(error.clone()),
            _ => None,
        }
    }

    /// Stops and returns mono 48 kHz samples.
    pub fn finish(mut self) -> Result<Vec<f32>, String> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.outcome
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_else(|| Err("No audio was recorded".to_owned()))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A speech-like tone for [`Recorder::simulated`], one level per 50 ms.
#[cfg(any(test, feature = "demo"))]
fn rehearse(
    stop: &AtomicBool,
    levels: &Mutex<Vec<f32>>,
    waker: &Waker,
) -> Result<Vec<f32>, String> {
    let segment = voice::RATE as usize / 20;
    let mut samples = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let start = samples.len();
        samples.extend((start..start + segment).map(|index| {
            let t = index as f32 / voice::RATE as f32;
            (t * 180.0 * std::f32::consts::TAU).sin()
                * 0.35
                * ((t * 2.3).sin() * (t * 0.9).cos()).abs()
        }));
        let peak = samples[start..]
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        levels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(peak * 0.7);
        waker.wake();
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(samples)
}

/// Opens the default microphone in its own format.
fn open_microphone() -> Result<rodio::microphone::Microphone, String> {
    rodio::microphone::MicrophoneBuilder::new()
        .default_device()
        .map_err(|error| format!("No microphone available: {error}"))?
        .default_config()
        .map_err(|error| format!("The microphone has no supported format: {error}"))?
        .open_stream()
        .map_err(|error| format!("Could not open the microphone: {error}"))
}

fn record(stop: &AtomicBool, levels: &Mutex<Vec<f32>>, waker: &Waker) -> Result<Vec<f32>, String> {
    let mut microphone = open_microphone()?;
    let channels = microphone.channels().get();
    let rate = microphone.sample_rate().get();
    let chunk = (rate as usize * usize::from(channels) / 20).max(1);
    let started = Instant::now();
    let mut heard = Vec::new();
    while !stop.load(Ordering::Relaxed) && started.elapsed() < LONGEST_RECORDING {
        let before = heard.len();
        heard.extend(microphone.by_ref().take(chunk));
        let taken = &heard[before..];
        if taken.is_empty() {
            break;
        }
        let loudness = (taken.iter().map(|s| s * s).sum::<f32>() / taken.len() as f32).sqrt();
        levels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(loudness);
        waker.wake();
        if taken.len() < chunk {
            // The device disappeared before recording stopped.
            break;
        }
    }
    if heard.is_empty() {
        return Err("The microphone did not record any audio".to_owned());
    }
    Ok(voice::mono_at_rate(&heard, channels, rate))
}

/// Samples in each frame the call takes from the microphone: 60 ms at 16 kHz.
const CALL_FRAME: usize = WA_FRAME_SAMPLES;
/// Frames the call may leave unread before the microphone drops the newest.
const CAPTURE_QUEUE: usize = 3;
/// Frames the call may have queued for the speaker before it drops the newest,
/// so a stall cannot build up seconds of delay.
const PLAYOUT_QUEUE: usize = 8;

/// The ring of an incoming call, synthesized here and repeated until dropped.
pub struct Ringtone {
    // The sink plays only while its device is open.
    _device: rodio::MixerDeviceSink,
    sink: rodio::Player,
}

impl Ringtone {
    pub fn start() -> Result<Self, String> {
        let device = open_output().map_err(|error| format!("No sound output: {error}"))?;
        let sink = rodio::Player::connect_new(device.mixer());
        sink.append(SamplesBuffer::new(mono(), rate(), ring_cycle()).repeat_infinite());
        sink.play();
        Ok(Self {
            _device: device,
            sink,
        })
    }
}

impl Drop for Ringtone {
    fn drop(&mut self) {
        self.sink.stop();
    }
}

/// Two short double tones and a pause, the cadence of a phone ringing.
fn ring_cycle() -> Vec<f32> {
    let rate = voice::RATE as f32;
    let tone = |seconds: f32| {
        let length = (seconds * rate) as usize;
        let fade = (0.02 * rate) as usize;
        (0..length).map(move |n| {
            let t = n as f32 / rate;
            let wave = (std::f32::consts::TAU * 440.0 * t).sin()
                + (std::f32::consts::TAU * 480.0 * t).sin();
            // Ramps at both ends keep the tone from clicking.
            let edge = n.min(length - 1 - n).min(fade) as f32 / fade as f32;
            wave * 0.5 * 0.25 * edge
        })
    };
    let silence = |seconds: f32| std::iter::repeat_n(0.0, (seconds * rate) as usize);
    tone(0.4)
        .chain(silence(0.2))
        .chain(tone(0.4))
        .chain(silence(2.0))
        .collect()
}

/// The ends of a call's audio that the library holds: it reads microphone
/// frames from `source` and writes what the other side said to `sink`. Both
/// carry mono `i16` samples, and each `source` frame has exactly 960 of them.
pub struct CallEndpoints {
    pub source: async_channel::Receiver<Vec<i16>>,
    pub sink: async_channel::Sender<Vec<i16>>,
}

/// The default microphone and speaker for the length of one call.
///
/// Both devices are opened by [`CallAudio::start`] and released when this is
/// dropped; nothing holds them in between, and voice messages keep using them
/// as before. The device threads are not the interface thread.
pub struct CallAudio {
    stop: Arc<AtomicBool>,
    /// The speaker thread keeps its device until this is dropped.
    _speaker: std::sync::mpsc::Sender<()>,
}

impl CallAudio {
    /// Opens the microphone and the speaker, and returns the endpoints for the
    /// library. `sink_rate` is the sample rate of the frames the call writes
    /// to the sink; the speaker converts it to the device's own.
    ///
    /// Opening a device can take a moment, so the caller must not be the
    /// interface thread.
    pub fn start(sink_rate: u32) -> Result<(Self, CallEndpoints), String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (frames, source) = async_channel::bounded(CAPTURE_QUEUE);
        let (sink, playout) = async_channel::bounded(PLAYOUT_QUEUE);
        let (opened, opening) = std::sync::mpsc::sync_channel(1);
        let microphone = {
            let stop = Arc::clone(&stop);
            let opened = opened.clone();
            std::thread::Builder::new()
                .name("call-microphone".to_owned())
                .spawn(move || capture(&stop, &frames, &opened))
                .map_err(|error| format!("Could not start the microphone: {error}"))
        };
        // From here the guard stops the microphone on any early return.
        let audio_stop = Arc::clone(&stop);
        let (keep, release) = std::sync::mpsc::channel();
        let audio = Self {
            stop: audio_stop,
            _speaker: keep,
        };
        microphone?;
        opening
            .recv()
            .map_err(|_| "The microphone stopped before it opened".to_owned())??;
        std::thread::Builder::new()
            .name("call-speaker".to_owned())
            .spawn(move || play_out(sink_rate, playout, release, &opened))
            .map_err(|error| format!("Could not start the speaker: {error}"))?;
        opening
            .recv()
            .map_err(|_| "The speaker stopped before it opened".to_owned())??;
        Ok((audio, CallEndpoints { source, sink }))
    }
}

impl Drop for CallAudio {
    fn drop(&mut self) {
        // The microphone thread notices within one chunk, and the speaker
        // thread when `_speaker` drops after this; both release their device.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Cuts a stream of samples into frames of [`CALL_FRAME`], carrying what is
/// left over into the next push.
#[derive(Default)]
struct Framer {
    pending: Vec<i16>,
}

impl Framer {
    /// The whole frames that `samples` completes, in order.
    fn push(&mut self, samples: &[i16]) -> Vec<Vec<i16>> {
        self.pending.extend_from_slice(samples);
        let whole = self.pending.len() / CALL_FRAME * CALL_FRAME;
        let frames = self.pending[..whole]
            .as_chunks::<CALL_FRAME>()
            .0
            .iter()
            .map(|frame| frame.to_vec())
            .collect();
        self.pending.drain(..whole);
        frames
    }
}

fn to_pcm(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

fn from_pcm(sample: i16) -> f32 {
    f32::from(sample) / 32_768.0
}

type Opened = std::sync::mpsc::SyncSender<Result<(), String>>;

/// Reads the microphone until told to stop, or until the device or the call
/// goes away, and sends 960-sample 16 kHz mono frames. A frame the call has
/// no room for is dropped, so a slow reader never holds up the device.
fn capture(stop: &AtomicBool, frames: &async_channel::Sender<Vec<i16>>, opened: &Opened) {
    let mut microphone = match open_microphone() {
        Ok(microphone) => {
            let _ = opened.send(Ok(()));
            microphone
        }
        Err(error) => {
            let _ = opened.send(Err(error));
            return;
        }
    };
    let channels = microphone.channels().get();
    let rate = microphone.sample_rate().get();
    // Read 20 ms at a time so a stop is noticed promptly.
    let chunk = (rate as usize * usize::from(channels) / 50).max(1);
    let mut stream = voice::MonoStream::new(channels, rate, WA_SAMPLE_RATE);
    let mut framer = Framer::default();
    while !stop.load(Ordering::Relaxed) {
        let heard: Vec<f32> = microphone.by_ref().take(chunk).collect();
        let pcm: Vec<i16> = stream.push(&heard).into_iter().map(to_pcm).collect();
        for frame in framer.push(&pcm) {
            match frames.try_send(frame) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Closed(_)) => return,
            }
        }
        if heard.len() < chunk {
            // The device disappeared during the call. Dropping `frames`
            // closes the source, which the library treats as a silent mic.
            log::warn!("The call's microphone stopped delivering audio");
            return;
        }
    }
}

/// Plays what the call writes to the sink until `release` is dropped.
fn play_out(
    rate: u32,
    playout: async_channel::Receiver<Vec<i16>>,
    release: std::sync::mpsc::Receiver<()>,
    opened: &Opened,
) {
    let output = open_output()
        .map_err(|error| format!("No sound output: {error}"))
        .map(|device| {
            let player = rodio::Player::connect_new(device.mixer());
            player.append(Playout::new(rate, playout));
            (device, player)
        });
    // Keep the device and the player alive until the call lets go of `release`.
    let _output = match output {
        Ok(output) => {
            let _ = opened.send(Ok(()));
            output
        }
        Err(error) => {
            let _ = opened.send(Err(error));
            return;
        }
    };
    let _ = release.recv();
}

/// Endless mono source of the frames a call delivers. It plays silence while
/// none are waiting and ends only when the call closes the channel.
struct Playout {
    rate: NonZero<u32>,
    frames: async_channel::Receiver<Vec<i16>>,
    current: std::vec::IntoIter<i16>,
}

impl Playout {
    fn new(rate: u32, frames: async_channel::Receiver<Vec<i16>>) -> Self {
        Self {
            rate: NonZero::new(rate).unwrap_or(NonZero::new(WA_SAMPLE_RATE).expect("not zero")),
            frames,
            current: Vec::new().into_iter(),
        }
    }
}

impl Iterator for Playout {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if let Some(sample) = self.current.next() {
                return Some(from_pcm(sample));
            }
            match self.frames.try_recv() {
                Ok(frame) => self.current = frame.into_iter(),
                Err(async_channel::TryRecvError::Empty) => return Some(0.0),
                Err(async_channel::TryRecvError::Closed) => return None,
            }
        }
    }
}

impl Source for Playout {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> NonZero<u16> {
        mono()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        self.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Temporary recording path used before sending and archiving.
#[allow(dead_code)]
pub fn recording_path(dir: &Path) -> PathBuf {
    dir.join(format!("voice-{}.ogg", crate::util::now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clip_that_finished_is_handed_back_once() {
        let mut player = Player::new(crate::backend::Waker::default());
        player.finished = Some("clip".into());
        assert_eq!(player.take_finished().as_deref(), Some("clip"));
        assert_eq!(player.take_finished(), None, "the app takes it once");
        player.finished = Some("other".into());
        player.stop();
        assert_eq!(
            player.take_finished(),
            None,
            "a clip the reader stopped is not one that played to its end"
        );
    }

    #[test]
    fn the_ring_is_quiet_and_whole_seconds_long() {
        let cycle = ring_cycle();
        assert_eq!(cycle.len(), (voice::RATE as f32 * 3.0) as usize);
        assert!(cycle.iter().all(|sample| sample.abs() <= 0.25));
        assert!(cycle.iter().any(|sample| sample.abs() > 0.1));
        assert_eq!(cycle[0], 0.0);
    }

    #[test]
    fn speed_labels_match_the_button() {
        assert_eq!(speed_label(SPEEDS[0]), "1x");
        assert_eq!(speed_label(SPEEDS[1]), "1.25x");
        assert_eq!(speed_label(SPEEDS[2]), "1.5x");
        assert_eq!(speed_label(SPEEDS[3]), "1.75x");
        assert_eq!(speed_label(SPEEDS[4]), "2x");
    }

    #[test]
    fn the_chip_cycles_like_the_phone() {
        assert_eq!(next_cycled_speed(1.0), 1.5);
        assert_eq!(next_cycled_speed(1.5), 2.0);
        assert_eq!(next_cycled_speed(2.0), 1.0);
        // A speed chosen from the menu moves on to the next faster one.
        assert_eq!(next_cycled_speed(1.25), 1.5);
        assert_eq!(next_cycled_speed(1.75), 2.0);
    }

    #[test]
    fn unsupported_speeds_snap_to_the_nearest_supported_one() {
        assert_eq!(supported_speed(1.3), 1.25);
        assert_eq!(supported_speed(1.4), 1.5);
        assert_eq!(supported_speed(1.8), 1.75);
        assert_eq!(supported_speed(0.5), 1.0);
        assert_eq!(supported_speed(4.0), 2.0);
        assert_eq!(supported_speed(f32::INFINITY), 1.0);
        for speed in SPEEDS {
            assert_eq!(supported_speed(speed), speed);
        }
        let mut player = Player::new(Waker::default());
        assert_eq!(player.set_speed(1.3), 1.25);
        assert_eq!(player.speed(), 1.25);
    }

    #[test]
    fn setting_a_speed_clamps_to_the_supported_range() {
        let mut player = Player::new(Waker::default());
        assert_eq!(player.speed(), SPEEDS[0]);
        player.set_speed(1.75);
        assert_eq!(player.speed(), 1.75);
        // Beyond the fastest speed clamps to it.
        player.set_speed(4.0);
        assert_eq!(player.speed(), SPEEDS[SPEEDS.len() - 1]);
        // A non-finite speed plays at 1x.
        player.set_speed(f32::NAN);
        assert_eq!(player.speed(), SPEEDS[0]);
    }

    #[test]
    fn a_speed_still_building_keeps_the_queued_one() {
        let samples = Arc::new(vec![0.0; 12]);
        let one_and_a_half = Arc::new(vec![0.0; 8]);
        let double = Arc::new(vec![0.0; 6]);
        let loaded = Loaded {
            message: "clip".to_owned(),
            samples: Arc::clone(&samples),
            buffer: Arc::clone(&one_and_a_half),
            factor: 1.5,
            base: Duration::ZERO,
            paused: false,
            done: false,
        };
        let mut stretches = vec![(1.5, Arc::clone(&one_and_a_half))];

        let (buffer, factor) = Player::buffer_for(&loaded, &stretches, 2.0);
        assert!(Arc::ptr_eq(&buffer, &one_and_a_half));
        assert_eq!(factor, 1.5);

        stretches.push((2.0, Arc::clone(&double)));
        let (buffer, factor) = Player::buffer_for(&loaded, &stretches, 2.0);
        assert!(Arc::ptr_eq(&buffer, &double));
        assert_eq!(factor, 2.0);
        // Both built speeds stay available when cycling back.
        let (buffer, _) = Player::buffer_for(&loaded, &stretches, 1.5);
        assert!(Arc::ptr_eq(&buffer, &one_and_a_half));
        let (buffer, factor) = Player::buffer_for(&loaded, &stretches, 1.0);
        assert!(Arc::ptr_eq(&buffer, &samples));
        assert_eq!(factor, 1.0);
    }

    #[test]
    fn a_speed_is_preparing_until_the_clip_plays_at_it() {
        let samples = Arc::new(vec![0.0; 12]);
        let mut player = Player::new(Waker::default());
        player.loaded = Some(Loaded {
            message: "clip".to_owned(),
            buffer: Arc::clone(&samples),
            samples,
            factor: 1.0,
            base: Duration::ZERO,
            paused: true,
            done: false,
        });
        assert!(!player.preparing_speed("clip"));
        player.speed = 2.0;
        assert!(player.preparing_speed("clip"));
        assert!(!player.preparing_speed("another clip"));
        if let Some(loaded) = player.loaded.as_mut() {
            loaded.factor = 2.0;
        }
        assert!(!player.preparing_speed("clip"));
    }

    #[test]
    fn unusable_speeds_are_kept_in_range() {
        let mut player = Player::new(Waker::default());
        player.set_speed(f32::NAN);
        assert_eq!(player.speed(), 1.0);
        player.set_speed(f32::INFINITY);
        assert_eq!(player.speed(), 1.0);
        player.set_speed(50.0);
        assert_eq!(player.speed(), 2.0);
        player.set_speed(-3.0);
        assert_eq!(player.speed(), 1.0);
    }

    #[test]
    fn going_back_to_one_x_cancels_the_outstanding_compression() {
        let mut player = Player::new(Waker::default());
        let cancelled = Arc::new(AtomicBool::new(false));
        player.set_speed(2.0);
        player.stretching = Some(Stretching {
            factor: 2.0,
            slot: Default::default(),
            cancelled: Arc::clone(&cancelled),
        });
        player.set_speed(1.0);
        assert!(player.stretching.is_none());
        assert!(cancelled.load(Ordering::Relaxed));
    }

    #[test]
    fn call_endpoints_are_what_the_library_takes() {
        fn takes<S: whatsapp_rust::voip::AudioSource, K: whatsapp_rust::voip::AudioSink>(
            _: S,
            _: K,
        ) {
        }
        let (sink, source) = async_channel::bounded(1);
        takes(source, sink);
    }

    #[test]
    fn frames_are_cut_at_960_samples() {
        let samples: Vec<i16> = (0..CALL_FRAME as i16 * 3).collect();
        let mut framer = Framer::default();
        let frames = framer.push(&samples);
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|frame| frame.len() == 960));
        assert_eq!(frames.concat(), samples);
        assert!(
            framer.pending.is_empty(),
            "an exact boundary leaves nothing"
        );
        assert!(framer.push(&[]).is_empty());
    }

    #[test]
    fn a_partial_frame_waits_for_the_rest() {
        let mut framer = Framer::default();
        assert!(framer.push(&[1; 959]).is_empty());
        let frames = framer.push(&[2; 1]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0][958], 1);
        assert_eq!(frames[0][959], 2);
        // A push that spans two frames leaves the remainder behind.
        let frames = framer.push(&[3; 960 + 5]);
        assert_eq!(frames.len(), 1);
        assert_eq!(framer.pending, vec![3; 5]);
    }

    #[test]
    fn no_sample_is_lost_or_repeated_whatever_the_push_sizes() {
        let total = CALL_FRAME * 7 + 123;
        let samples: Vec<i16> = (0..total).map(|i| (i % 30_011) as i16).collect();
        for piece in [1usize, 7, 320, 959, 960, 961, 5_000] {
            let mut framer = Framer::default();
            let mut joined = Vec::new();
            for part in samples.chunks(piece) {
                for frame in framer.push(part) {
                    assert_eq!(frame.len(), CALL_FRAME);
                    joined.extend(frame);
                }
            }
            joined.extend(&framer.pending);
            assert_eq!(joined, samples, "pieces of {piece}");
        }
    }

    #[test]
    fn samples_convert_to_pcm_and_back() {
        assert_eq!(to_pcm(0.0), 0);
        assert_eq!(to_pcm(1.0), i16::MAX);
        assert_eq!(to_pcm(-1.0), -i16::MAX);
        assert_eq!(to_pcm(3.0), i16::MAX, "clipping stays in range");
        assert!((from_pcm(to_pcm(0.5)) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn the_speaker_plays_frames_then_silence() {
        let (sender, receiver) = async_channel::bounded(2);
        let mut playout = Playout::new(16_000, receiver);
        assert_eq!(playout.sample_rate().get(), 16_000);
        assert_eq!(playout.channels().get(), 1);
        assert_eq!(playout.next(), Some(0.0), "silence before any frame");
        sender.try_send(vec![16_384, -16_384]).unwrap();
        assert_eq!(playout.next(), Some(0.5));
        assert_eq!(playout.next(), Some(-0.5));
        assert_eq!(playout.next(), Some(0.0));
        drop(sender);
        assert_eq!(playout.next(), None, "ends when the call closes it");
    }

    /// Opens both devices and listens for a second:
    /// `cargo test audio::tests::call -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a microphone and a speaker"]
    fn call_audio_runs_on_this_machine() {
        let (audio, endpoints) = CallAudio::start(16_000).expect("opens");
        std::thread::sleep(Duration::from_millis(1_000));
        let mut frames = 0;
        while let Ok(frame) = endpoints.source.try_recv() {
            assert_eq!(frame.len(), 960);
            frames += 1;
        }
        eprintln!("{frames} frames waiting");
        assert!(frames > 0);
        drop(audio);
    }

    /// Plays a one-second test tone:
    /// `cargo test audio::tests::plays -- --ignored --nocapture`.
    #[test]
    #[ignore = "makes a sound on this machine"]
    fn plays_a_clip_on_this_machine() {
        let dir = std::env::temp_dir();
        let path = dir.join("zapfast-audio-test.ogg");
        let tone: Vec<f32> = (0..voice::RATE)
            .map(|i| (i as f32 * 330.0 * std::f32::consts::TAU / voice::RATE as f32).sin() * 0.3)
            .collect();
        std::fs::write(&path, voice::encode(&tone).expect("encodes")).expect("written");
        let mut player = Player::new(Waker::default());
        player.toggle("clip", &path).expect("starts decoding");
        assert_eq!(player.status("clip").state, State::Loading);
        let started = Instant::now();
        let mut seen_playing = false;
        while started.elapsed() < Duration::from_secs(3) {
            player.poll().expect("plays");
            let status = player.status("clip");
            if status.state == State::Playing && status.position > Duration::from_millis(300) {
                seen_playing = true;
                eprintln!("playing at {:?} of {:?}", status.position, status.total);
            }
            if seen_playing && status.state == State::Idle {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(seen_playing, "never heard it playing");
        assert_eq!(player.status("clip").state, State::Idle, "ends on its own");
        assert_eq!(player.bars("clip").map(<[u8]>::len), Some(voice::BARS));
        let _ = std::fs::remove_file(path);
    }

    /// Plays a two-second tone at double speed and checks the position
    /// outruns the clock:
    /// `cargo test audio::tests::doubles -- --ignored --nocapture`.
    #[test]
    #[ignore = "makes a sound on this machine"]
    fn doubles_the_position_rate_on_this_machine() {
        let dir = std::env::temp_dir();
        let path = dir.join("zapfast-audio-speed-test.ogg");
        let tone: Vec<f32> = (0..voice::RATE * 2)
            .map(|i| (i as f32 * 330.0 * std::f32::consts::TAU / voice::RATE as f32).sin() * 0.3)
            .collect();
        std::fs::write(&path, voice::encode(&tone).expect("encodes")).expect("written");
        let mut player = Player::new(Waker::default());
        player.set_speed(2.0);
        player.toggle("clip", &path).expect("starts decoding");
        let started = Instant::now();
        let mut seen: Vec<(Duration, Duration)> = Vec::new();
        while started.elapsed() < Duration::from_secs(6) {
            player.poll().expect("plays");
            let status = player.status("clip");
            // The clip starts at 1x while its compression builds; measure
            // only after the compressed buffer has taken over.
            if status.state == State::Playing && status.position > Duration::from_millis(600) {
                seen.push((started.elapsed(), status.position));
            }
            if status.state == State::Idle && !seen.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let (first_wall, first_position) = seen.first().expect("played");
        let (last_wall, last_position) = seen.last().expect("played");
        let wall = *last_wall - *first_wall;
        let advanced = *last_position - *first_position;
        assert!(wall > Duration::from_millis(200), "played for {wall:?}");
        assert!(
            advanced.as_secs_f32() >= 1.5 * wall.as_secs_f32(),
            "position advanced {advanced:?} over {wall:?} of wall time"
        );
        let _ = std::fs::remove_file(path);
    }

    /// Records one second from the default microphone:
    /// `cargo test audio::tests::records -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a microphone"]
    fn records_a_second_on_this_machine() {
        let recorder = Recorder::start(Waker::default());
        std::thread::sleep(Duration::from_millis(1_000));
        assert!(recorder.failure().is_none(), "{:?}", recorder.failure());
        let levels = recorder.levels();
        let heard = recorder.finish().expect("something was heard");
        eprintln!("{} samples, {} level readings", heard.len(), levels.len());
        assert!(
            heard.len() > voice::RATE as usize * 8 / 10,
            "{}",
            heard.len()
        );
        assert!(levels.len() >= 15, "{}", levels.len());
    }
}
