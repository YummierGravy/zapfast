//! Audio playback, voice-message recording, and the audio of a call.
//!
//! Input and output devices are opened on demand and released when idle.

use std::collections::HashMap;
#[cfg(not(target_os = "linux"))]
use std::collections::VecDeque;
use std::num::NonZero;
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "linux"))]
use std::sync::Condvar;
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

/// Microphone, speaker, and camera chosen in Settings. An empty name follows
/// the system default (the first camera, on Linux).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallDevices {
    pub microphone: String,
    pub speaker: String,
    pub camera: String,
}

/// Names the settings pickers can offer. Listing can take a moment, so it
/// runs off the interface thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceList {
    pub microphones: Vec<String>,
    pub speakers: Vec<String>,
    pub cameras: Vec<String>,
}

/// The input, output, and camera devices this computer has right now.
pub fn list_devices() -> DeviceList {
    let (microphones, speakers) = match pipewire_endpoints() {
        Some(endpoints) => (
            endpoint_labels(&endpoints, true),
            endpoint_labels(&endpoints, false),
        ),
        None => (
            inputs().into_iter().map(|(name, _)| name).collect(),
            outputs().into_iter().map(|(name, _)| name).collect(),
        ),
    };
    DeviceList {
        microphones,
        speakers,
        cameras: crate::call_camera::cameras(),
    }
}

/// `(name, driver)` of a cpal device. On ALSA the driver is the PCM id.
fn describe(device: &rodio::Device) -> Option<(String, Option<String>)> {
    use rodio::DeviceTrait;
    let description = device.description().ok()?;
    Some((
        description.name().to_string(),
        description.driver().map(str::to_owned),
    ))
}

fn outputs() -> Vec<(String, rodio::Device)> {
    use rodio::cpal::traits::HostTrait;
    let Ok(devices) = rodio::cpal::default_host().output_devices() else {
        return Vec::new();
    };
    choices(devices.filter_map(|device| {
        let (name, driver) = describe(&device)?;
        Some((name, driver, device))
    }))
}

fn inputs() -> Vec<(String, rodio::microphone::Input)> {
    let Ok(inputs) = rodio::microphone::available_inputs() else {
        return Vec::new();
    };
    choices(inputs.into_iter().filter_map(|input| {
        let (name, driver) = describe(&input.clone().into_inner())?;
        Some((name, driver, input))
    }))
}

/// The devices worth offering, each under one name.
///
/// ALSA lists every PCM of every card: surround layouts, S/PDIF, raw `hw`
/// and `plughw`, `dmix`, resamplers, and the sound servers. A desktop has
/// four or five devices and that comes to a hundred entries. Keep one per
/// card (its `sysdefault`, else `front`) and each HDMI output; the sound
/// server is what **System default** already plays through.
fn choices<T>(devices: impl Iterator<Item = (String, Option<String>, T)>) -> Vec<(String, T)> {
    let mut kept: Vec<(String, u8, String, T)> = Vec::new();
    for (name, driver, device) in devices {
        if name.is_empty() || driver.as_deref() == Some("null") {
            continue;
        }
        let (key, rank, name) = if cfg!(target_os = "linux") {
            let Some(pcm) = driver.as_deref() else {
                continue;
            };
            let Some((key, rank, card)) = alsa_choice(pcm) else {
                continue;
            };
            if pcm.starts_with("hdmi:") && unplugged_hdmi(&name) {
                continue;
            }
            // A card's `front` PCM can be described only by its purpose.
            let name = if name.contains(',') {
                name
            } else {
                format!("{card}, {name}")
            };
            (key, rank, name)
        } else {
            (name.clone(), 0, name)
        };
        match kept.iter_mut().find(|entry| entry.0 == key) {
            Some(entry) if rank < entry.1 => *entry = (key, rank, name, device),
            Some(_) => {}
            None => kept.push((key, rank, name, device)),
        }
    }
    let mut out: Vec<(String, T)> = Vec::new();
    for (_, _, name, device) in kept {
        if !out.iter().any(|(seen, _)| *seen == name) {
            out.push((name, device));
        }
    }
    out
}

/// Which card an ALSA PCM id stands for, how good a stand-in it is (lower is
/// better), and the card's id. `None` for PCMs not worth offering.
fn alsa_choice(pcm: &str) -> Option<(String, u8, String)> {
    let (kind, rest) = pcm.split_once(':')?;
    let card = rest
        .split(',')
        .find_map(|part| part.strip_prefix("CARD="))?
        .to_owned();
    match kind {
        "sysdefault" => Some((card.clone(), 0, card)),
        "front" => Some((card.clone(), 1, card)),
        "hdmi" => Some((pcm.to_owned(), 0, card)),
        _ => None,
    }
}

/// The kernel names an HDMI or DisplayPort output after the monitor on it,
/// and leaves a port with nothing attached as "HDMI 0", "HDMI 1", and so on.
fn unplugged_hdmi(name: &str) -> bool {
    let port = name.rsplit(", ").next().unwrap_or(name);
    port.strip_prefix("HDMI ")
        .is_some_and(|number| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
}

fn named_output(name: &str) -> Option<rodio::Device> {
    if name.is_empty() {
        return None;
    }
    outputs()
        .into_iter()
        .find(|(shown, _)| shown == name)
        .map(|(_, device)| device)
}

fn named_input(name: &str) -> Option<rodio::microphone::Input> {
    if name.is_empty() {
        return None;
    }
    inputs()
        .into_iter()
        .find(|(shown, _)| shown == name)
        .map(|(_, input)| input)
}

/// One PipeWire sink or source: the label Settings shows, and the node name
/// the sound server opens.
struct PwEndpoint {
    label: String,
    target: String,
    input: bool,
}

fn endpoint_labels(endpoints: &[PwEndpoint], input: bool) -> Vec<String> {
    endpoints
        .iter()
        .filter(|endpoint| endpoint.input == input)
        .map(|endpoint| endpoint.label.clone())
        .collect()
}

/// PipeWire's sinks and sources, when the sound server is running.
///
/// `None` when `pw-dump` is missing or fails, so a machine without PipeWire
/// keeps the ALSA list. Opening one of these by its ALSA `sysdefault` name
/// asks dsnoop for the hardware, which fails while PipeWire already holds
/// the headset (`unable to open slave`).
fn pipewire_endpoints() -> Option<Vec<PwEndpoint>> {
    let output = std::process::Command::new("pw-dump")
        .arg("Node")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(parse_pipewire_dump(&text))
}

fn parse_pipewire_dump(json: &str) -> Vec<PwEndpoint> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(nodes) = value.as_array() else {
        return Vec::new();
    };
    let mut endpoints = Vec::new();
    for node in nodes {
        let props = &node["info"]["props"];
        let input = match props["media.class"].as_str() {
            Some("Audio/Source") => true,
            Some("Audio/Sink") => false,
            _ => continue,
        };
        let Some(target) = props["node.name"].as_str() else {
            continue;
        };
        if target.ends_with(".monitor") {
            continue;
        }
        let Some(label) = props["node.description"]
            .as_str()
            .filter(|label| !label.is_empty())
        else {
            continue;
        };
        if endpoints
            .iter()
            .any(|endpoint: &PwEndpoint| endpoint.input == input && endpoint.label == label)
        {
            continue;
        }
        endpoints.push(PwEndpoint {
            label: label.to_owned(),
            target: target.to_owned(),
            input,
        });
    }
    endpoints
}

/// Opens an audio device with `PIPEWIRE_NODE` naming `node`, or as the user
/// started ZapFast when `node` is `None`.
///
/// The PipeWire ALSA plugin, behind both the `pipewire` and the `default`
/// PCM, reads that variable when a PCM opens. A call opens its microphone
/// and its speaker at the same moment, and a ringtone or notification can
/// open beside them, so every open goes through this lock: otherwise a
/// speaker opening while the microphone's node is set attaches to the
/// microphone, never runs, and stalls the call's whole receive path.
fn with_pipewire_node<T>(node: Option<&str>, open: impl FnOnce() -> T) -> T {
    static OPEN: Mutex<()> = Mutex::new(());
    static STARTED_WITH: std::sync::OnceLock<Option<std::ffi::OsString>> =
        std::sync::OnceLock::new();
    let _guard = OPEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let original = STARTED_WITH.get_or_init(|| std::env::var_os("PIPEWIRE_NODE"));
    let set = |value: Option<&std::ffi::OsStr>| {
        // SAFETY: only called with `OPEN` held. ZapFast changes this variable
        // nowhere else, and the plugin reads it while a PCM opens, which
        // happens inside this same critical section.
        unsafe {
            match value {
                Some(value) => std::env::set_var("PIPEWIRE_NODE", value),
                None => std::env::remove_var("PIPEWIRE_NODE"),
            }
        }
    };
    match node {
        Some(node) => set(Some(std::ffi::OsStr::new(node))),
        None => set(original.as_deref()),
    }
    let result = open();
    set(original.as_deref());
    result
}

/// The `pipewire` playback PCM, which is what `PIPEWIRE_NODE` selects through.
fn pipewire_output() -> Option<rodio::Device> {
    use rodio::cpal::traits::HostTrait;
    rodio::cpal::default_host()
        .output_devices()
        .ok()?
        .find(|device| {
            describe(device).and_then(|(_, driver)| driver).as_deref() == Some("pipewire")
        })
}

/// The `pipewire` capture PCM.
fn pipewire_input() -> Option<rodio::microphone::Input> {
    rodio::microphone::available_inputs()
        .ok()?
        .into_iter()
        .find(|input| {
            describe(&input.clone().into_inner())
                .and_then(|(_, driver)| driver)
                .as_deref()
                == Some("pipewire")
        })
}

/// cpal reports a PipeWire timestamp a fraction of a millisecond behind the
/// trigger as a stream error, and skips writing that period. Say so once.
/// Call audio does not use this path; ringtones and message playback do.
fn stream_error(error: rodio::cpal::StreamError) {
    static REPORTED: AtomicBool = AtomicBool::new(false);
    let message = error.to_string();
    if message.contains("was earlier than get_trigger_htstamp")
        && REPORTED.swap(true, Ordering::Relaxed)
    {
        return;
    }
    log::warn!("audio stream error: {message}");
}

/// Opens the default output device for playback.
///
/// rodio reports the sink's drop through `stderr` by default. A desktop launch
/// can have that closed: ZapFast inherits `stderr` from whatever started it,
/// and that process can exit while ZapFast runs on. Rust ignores `SIGPIPE`, so
/// the next write there fails with `Broken pipe` and the print macro panics,
/// which aborts the whole app in a release build. Keep it off, and report
/// failures of our own through the log instead.
pub fn open_output(preferred: &str) -> Result<rodio::MixerDeviceSink, rodio::DeviceSinkError> {
    if !preferred.is_empty() {
        match pipewire_endpoints() {
            Some(endpoints) => {
                let target = endpoints
                    .iter()
                    .find(|endpoint| !endpoint.input && endpoint.label == preferred)
                    .map(|endpoint| endpoint.target.clone());
                if let (Some(target), Some(device)) = (target, pipewire_output()) {
                    let opened = with_pipewire_node(Some(&target), || open_device_output(device));
                    if opened.is_ok() {
                        return opened;
                    }
                }
            }
            None => {
                if let Some(device) = named_output(preferred) {
                    let opened = with_pipewire_node(None, || open_device_output(device));
                    if opened.is_ok() {
                        return opened;
                    }
                }
            }
        }
    }
    with_pipewire_node(None, open_default_output)
}

fn open_device_output(
    device: rodio::Device,
) -> Result<rodio::MixerDeviceSink, rodio::DeviceSinkError> {
    let mut output = rodio::DeviceSinkBuilder::from_device(device)?
        .with_error_callback(stream_error)
        .open_stream()?;
    output.log_on_drop(false);
    Ok(output)
}

fn open_default_output() -> Result<rodio::MixerDeviceSink, rodio::DeviceSinkError> {
    use rodio::cpal::traits::HostTrait;
    match rodio::cpal::default_host().default_output_device() {
        Some(device) => open_device_output(device),
        None => {
            let mut output = rodio::DeviceSinkBuilder::open_default_sink()?;
            output.log_on_drop(false);
            Ok(output)
        }
    }
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
    /// Speaker from Settings. Empty follows the system default.
    speaker: String,
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
            speaker: String::new(),
        }
    }

    /// Uses this speaker the next time a clip opens the output. An empty name
    /// follows the system default. A change closes the device a clip has open.
    pub fn set_speaker(&mut self, speaker: &str) {
        if self.speaker == speaker {
            return;
        }
        self.speaker = speaker.to_owned();
        self.output = None;
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
            let device =
                open_output(&self.speaker).map_err(|error| format!("No sound output: {error}"))?;
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

/// Records from the chosen microphone until told to stop. An empty name
/// follows the system default.
pub struct Recorder {
    started: Instant,
    stop: Arc<AtomicBool>,
    /// Loudness for each recorded 50 ms segment.
    levels: Arc<Mutex<Vec<f32>>>,
    outcome: Outcome,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    pub fn start(waker: Waker, microphone: String) -> Self {
        Self::spawn(waker, move |stop, levels, waker| {
            record(stop, levels, waker, &microphone)
        })
    }

    /// Records a synthetic voice instead of the microphone, at the pace a
    /// real take would, for offline demos: the waveform grows while it runs
    /// and sending it yields that many seconds of a speech-like tone.
    #[cfg(any(test, feature = "demo"))]
    pub fn simulated(waker: Waker) -> Self {
        Self::spawn(waker, rehearse)
    }

    fn spawn(
        waker: Waker,
        body: impl FnOnce(&AtomicBool, &Mutex<Vec<f32>>, &Waker) -> Result<Vec<f32>, String>
        + Send
        + 'static,
    ) -> Self {
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

/// Opens `preferred` when it is connected, otherwise the default microphone.
/// A name that is no longer connected, such as one saved before the lists
/// changed, opens the default rather than failing the call.
fn open_microphone(preferred: &str) -> Result<rodio::microphone::Microphone, String> {
    if !preferred.is_empty() {
        match pipewire_endpoints() {
            Some(endpoints) => {
                let target = endpoints
                    .iter()
                    .find(|endpoint| endpoint.input && endpoint.label == preferred)
                    .map(|endpoint| endpoint.target.clone());
                if let (Some(target), Some(input)) = (target, pipewire_input()) {
                    match with_pipewire_node(Some(&target), || open_input(Some(input))) {
                        Ok(microphone) => return Ok(microphone),
                        Err(error) => log::warn!("{error}; using the default microphone"),
                    }
                }
            }
            None => {
                if let Some(input) = named_input(preferred) {
                    match with_pipewire_node(None, || open_input(Some(input))) {
                        Ok(microphone) => return Ok(microphone),
                        Err(error) => log::warn!("{error}; using the default microphone"),
                    }
                }
            }
        }
    }
    with_pipewire_node(None, || open_input(None))
}

fn open_input(
    input: Option<rodio::microphone::Input>,
) -> Result<rodio::microphone::Microphone, String> {
    let builder = rodio::microphone::MicrophoneBuilder::new();
    let builder = match input {
        Some(input) => builder
            .device(input)
            .map_err(|error| format!("Could not use that microphone: {error}"))?,
        None => builder
            .default_device()
            .map_err(|error| format!("No microphone available: {error}"))?,
    };
    builder
        .default_config()
        .map_err(|error| format!("The microphone has no supported format: {error}"))?
        .open_stream()
        .map_err(|error| format!("Could not open the microphone: {error}"))
}

fn record(
    stop: &AtomicBool,
    levels: &Mutex<Vec<f32>>,
    waker: &Waker,
    microphone_name: &str,
) -> Result<Vec<f32>, String> {
    let mut microphone = open_microphone(microphone_name)?;
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
    pub fn start(speaker: &str) -> Result<Self, String> {
        let device = open_output(speaker).map_err(|error| format!("No sound output: {error}"))?;
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

fn samples_for(rate: f32, seconds: f32) -> usize {
    (seconds * rate) as usize
}

/// A faded tone at `hz`, quiet enough to sit under a voice. The fade keeps
/// the ends from clicking.
fn burst(rate: f32, hz: &[f32], seconds: f32) -> impl Iterator<Item = f32> {
    let length = samples_for(rate, seconds).max(1);
    let fade = samples_for(rate, 0.02).max(1);
    (0..length).map(move |n| {
        let t = n as f32 / rate;
        let wave = hz
            .iter()
            .map(|hz| (std::f32::consts::TAU * hz * t).sin())
            .sum::<f32>();
        let edge = n.min(length - 1 - n).min(fade) as f32 / fade as f32;
        // One note or several share the same peak.
        wave / hz.len() as f32 * 0.25 * edge
    })
}

fn quiet(rate: f32, seconds: f32) -> impl Iterator<Item = f32> {
    std::iter::repeat_n(0.0, samples_for(rate, seconds))
}

/// Two short double tones and a pause, the cadence of a phone ringing here.
fn ring_cycle() -> Vec<f32> {
    let rate = voice::RATE as f32;
    let tone = |seconds: f32| burst(rate, &[440.0, 480.0], seconds);
    tone(0.4)
        .chain(quiet(rate, 0.2))
        .chain(tone(0.4))
        .chain(quiet(rate, 2.0))
        .collect()
}

/// One longer tone and a pause: the ringback of a call placed here, at `rate`.
fn ringback_cycle(rate: u32) -> Vec<f32> {
    let rate = rate as f32;
    burst(rate, &[440.0, 480.0], 1.0)
        .chain(quiet(rate, 2.0))
        .collect()
}

/// Two short rising notes, the sound of a line opening, at `rate`.
fn connect_tone(rate: u32) -> Vec<f32> {
    let rate = rate as f32;
    burst(rate, &[660.0], 0.09)
        .chain(quiet(rate, 0.04))
        .chain(burst(rate, &[880.0], 0.16))
        .collect()
}

/// The ends of a call's audio that the library holds: it reads microphone
/// frames from `source` and writes what the other side said to `sink`. Both
/// carry mono `i16` samples, and each `source` frame has exactly 960 of them.
pub struct CallEndpoints {
    pub source: async_channel::Receiver<Vec<i16>>,
    pub sink: async_channel::Sender<Vec<i16>>,
}

/// The microphone and speaker for the length of one call.
///
/// Both devices are opened by [`CallAudio::start`] and released when this is
/// dropped. [`CallAudio::set_devices`] points either one at another device
/// without ending the call. The device threads are not the interface thread.
pub struct CallAudio {
    stop: Arc<AtomicBool>,
    /// Set once the line is open. The speaker thread reads it.
    line_open: Arc<AtomicBool>,
    microphone: Arc<Mutex<String>>,
    speaker: Arc<Mutex<String>>,
    /// The speaker thread keeps its device until this is dropped.
    _speaker: std::sync::mpsc::Sender<()>,
}

impl CallAudio {
    /// The line is open: the speaker stops any ringback and plays the connect tone.
    pub fn line_open(&self) {
        self.line_open.store(true, Ordering::Relaxed);
    }

    /// Points the open call at another microphone or speaker. An empty name
    /// follows the system default. A name that is not connected is left as it
    /// was, and the call keeps going.
    pub fn set_devices(&self, microphone: &str, speaker: &str) {
        *self
            .microphone
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = microphone.to_owned();
        *self
            .speaker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = speaker.to_owned();
    }

    /// Opens the microphone and the speaker, and returns the endpoints for the
    /// library. `sink_rate` is the sample rate of the frames the call writes
    /// to the sink; the speaker converts it to the device's own.
    ///
    /// `ringback` plays a local ring until [`CallAudio::line_open`]: a call
    /// placed here has not been answered yet. A call answered here passes
    /// `false` and plays the other side until the line is open, then the
    /// connect tone.
    ///
    /// Opening a device can take a moment, so the caller must not be the
    /// interface thread.
    pub fn start(
        sink_rate: u32,
        ringback: bool,
        microphone: String,
        speaker: String,
    ) -> Result<(Self, CallEndpoints), String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (frames, source) = async_channel::bounded(CAPTURE_QUEUE);
        let (sink, playout) = async_channel::bounded(PLAYOUT_QUEUE);
        let (opened, opening) = std::sync::mpsc::sync_channel(1);
        let microphone_name = Arc::new(Mutex::new(microphone));
        let speaker_name = Arc::new(Mutex::new(speaker));
        let microphone = {
            let stop = Arc::clone(&stop);
            let opened = opened.clone();
            let microphone_name = Arc::clone(&microphone_name);
            std::thread::Builder::new()
                .name("call-microphone".to_owned())
                .spawn(move || capture(&stop, &frames, &opened, &microphone_name))
                .map_err(|error| format!("Could not start the microphone: {error}"))
        };
        // From here the guard stops the microphone on any early return.
        let audio_stop = Arc::clone(&stop);
        let line_open = Arc::new(AtomicBool::new(false));
        let speaker_open = Arc::clone(&line_open);
        let speaker_choice = Arc::clone(&speaker_name);
        let (keep, release) = std::sync::mpsc::channel();
        let audio = Self {
            stop: audio_stop,
            line_open,
            microphone: microphone_name,
            speaker: speaker_name,
            _speaker: keep,
        };
        microphone?;
        opening
            .recv()
            .map_err(|_| "The microphone stopped before it opened".to_owned())??;
        std::thread::Builder::new()
            .name("call-speaker".to_owned())
            .spawn(move || {
                play_out(
                    sink_rate,
                    playout,
                    release,
                    &opened,
                    ringback,
                    speaker_open,
                    &speaker_choice,
                )
            })
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
fn capture(
    stop: &AtomicBool,
    frames: &async_channel::Sender<Vec<i16>>,
    opened: &Opened,
    microphone_name: &Mutex<String>,
) {
    let mut device = None;
    let mut open_name = String::new();
    let mut told = false;
    let mut stream = None;
    let mut chunk = 1;
    let mut framer = Framer::default();
    // A device that refused to open. Trying it again on every pass floods the
    // log and the sound server; wait a second, and keep the microphone that
    // is already open.
    let mut rejected = String::new();
    let mut rejected_at = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let wanted = microphone_name
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if device.is_none() || wanted != open_name {
            let due = rejected != wanted || rejected_at.elapsed() >= Duration::from_secs(1);
            if due {
                match open_microphone(&wanted) {
                    Ok(microphone) => {
                        let channels = microphone.channels().get();
                        let rate = microphone.sample_rate().get();
                        chunk = (rate as usize * usize::from(channels) / 50).max(1);
                        stream = Some(voice::MonoStream::new(channels, rate, WA_SAMPLE_RATE));
                        device = Some(microphone);
                        open_name = wanted;
                        rejected.clear();
                        if !told {
                            let _ = opened.send(Ok(()));
                            told = true;
                        }
                    }
                    Err(error) => {
                        if !told {
                            let _ = opened.send(Err(error));
                            return;
                        }
                        if rejected != wanted {
                            log::warn!("could not switch the call's microphone: {error}");
                            rejected = wanted.clone();
                        }
                        rejected_at = Instant::now();
                        if device.is_none() {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            }
        }
        let Some(microphone) = device.as_mut() else {
            continue;
        };
        let Some(stream) = stream.as_mut() else {
            continue;
        };
        // Read 20 ms at a time so a stop, or a new device, is noticed promptly.
        let heard: Vec<f32> = microphone.by_ref().take(chunk).collect();
        let pcm: Vec<i16> = stream.push(&heard).into_iter().map(to_pcm).collect();
        for frame in framer.push(&pcm) {
            match frames.try_send(frame) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Closed(_)) => return,
            }
        }
        if heard.len() < chunk {
            // The device disappeared. The next pass opens it again, or the
            // default if that name is gone.
            log::warn!("The call's microphone stopped delivering audio");
            device = None;
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Samples waiting for the speaker. The producer blocks while the device is
/// ahead, and a new device reads the same queue, so the speaker can change
/// without ending the call.
#[cfg(not(target_os = "linux"))]
struct SamplePipe {
    samples: Mutex<VecDeque<f32>>,
    ready: Condvar,
    done: AtomicBool,
}

#[cfg(not(target_os = "linux"))]
impl SamplePipe {
    fn push(&self, sample: f32) {
        let mut guard = self
            .samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while guard.len() > 8_000 && !self.done.load(Ordering::Relaxed) {
            let (next, _) = self
                .ready
                .wait_timeout(guard, Duration::from_millis(40))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = next;
        }
        if self.done.load(Ordering::Relaxed) {
            return;
        }
        guard.push_back(sample);
    }

    fn pop(&self) -> Option<f32> {
        let mut guard = self
            .samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sample = guard.pop_front();
        if guard.len() < 4_000 {
            self.ready.notify_one();
        }
        sample
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Relaxed);
        self.ready.notify_all();
    }
}

#[cfg(not(target_os = "linux"))]
struct PipeSource {
    pipe: Arc<SamplePipe>,
    rate: NonZero<u32>,
}

#[cfg(not(target_os = "linux"))]
impl Iterator for PipeSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if let Some(sample) = self.pipe.pop() {
            return Some(sample);
        }
        if self.pipe.done.load(Ordering::Relaxed) {
            None
        } else {
            Some(0.0)
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl Source for PipeSource {
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

#[cfg(not(target_os = "linux"))]
fn attach_speaker(
    name: &str,
    rate: u32,
    pipe: &Arc<SamplePipe>,
) -> Result<(rodio::MixerDeviceSink, rodio::Player), String> {
    let device = open_output(name).map_err(|error| format!("No sound output: {error}"))?;
    let player = rodio::Player::connect_new(device.mixer());
    let rate = NonZero::new(rate).unwrap_or(NonZero::new(WA_SAMPLE_RATE).expect("not zero"));
    player.append(PipeSource {
        pipe: Arc::clone(pipe),
        rate,
    });
    Ok((device, player))
}

/// Plays what the call writes to the sink until `release` is dropped.
/// `ringback` is the local ring of a call placed here, until `line_open`.
/// `speaker` is read throughout, so a change opens that device instead.
///
/// On Linux this writes the PCM itself. cpal's ALSA host refuses the whole
/// period when PipeWire's hardware timestamp is a fraction of a millisecond
/// behind the trigger, which is every period on this plugin, so the callback
/// never runs and the call is silent.
#[cfg(target_os = "linux")]
fn play_out(
    rate: u32,
    playout: async_channel::Receiver<Vec<i16>>,
    release: std::sync::mpsc::Receiver<()>,
    opened: &Opened,
    ringback: bool,
    line_open: Arc<AtomicBool>,
    speaker: &Mutex<String>,
) {
    let mut wanted = speaker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let mut playback = match AlsaPlayback::open(&wanted, rate) {
        Ok(playback) => playback,
        Err(error) => {
            let _ = opened.send(Err(error));
            return;
        }
    };
    let _ = opened.send(Ok(()));
    log::info!("call speaker: {}", speaker_label(&wanted));
    let mut playout = Playout::call(rate, playout, ringback, line_open);
    let mut rejected = String::new();
    let mut rejected_at = Instant::now();
    let mut chunk = Vec::with_capacity(480);
    loop {
        match release.try_recv() {
            Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        let next = speaker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if next != wanted && (next != rejected || rejected_at.elapsed() >= Duration::from_secs(1)) {
            match AlsaPlayback::open(&next, rate) {
                Ok(opened_device) => {
                    playback = opened_device;
                    wanted = next;
                    rejected.clear();
                    log::info!("call speaker: {}", speaker_label(&wanted));
                }
                Err(error) => {
                    if next != rejected {
                        log::warn!("could not switch the call's speaker: {error}");
                        rejected = next;
                    }
                    rejected_at = Instant::now();
                }
            }
        }
        chunk.clear();
        for _ in 0..480 {
            let Some(sample) = playout.next() else {
                return;
            };
            chunk.push(sample);
        }
        if let Err(error) = playback.write(&chunk) {
            log::warn!("the call's speaker stopped: {error}");
            match AlsaPlayback::open(&wanted, rate) {
                Ok(again) => playback = again,
                Err(error) => {
                    log::warn!("could not reopen the call's speaker: {error}");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn speaker_label(name: &str) -> &str {
    if name.is_empty() {
        "system default"
    } else {
        name
    }
}

/// A call's speaker, opened directly so a PipeWire timestamp cannot cancel the write.
#[cfg(target_os = "linux")]
struct AlsaPlayback {
    pcm: alsa::PCM,
    /// Device rate the resampler aims at. The PCM may have chosen another.
    resample: voice::MonoStream,
}

#[cfg(target_os = "linux")]
impl AlsaPlayback {
    fn open(name: &str, source_rate: u32) -> Result<Self, String> {
        let pcm = open_playback_pcm(name)?;
        let device_rate = configure_playback(&pcm, source_rate)?;
        Ok(Self {
            pcm,
            resample: voice::MonoStream::new(1, source_rate, device_rate),
        })
    }

    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        let converted = self.resample.push(samples);
        let pcm: Vec<i16> = converted.into_iter().map(to_pcm).collect();
        write_playback(&self.pcm, &pcm)
    }
}

/// The named PipeWire sink, or the system default when `name` is empty or gone.
#[cfg(target_os = "linux")]
fn open_playback_pcm(name: &str) -> Result<alsa::PCM, String> {
    if !name.is_empty()
        && let Some(endpoints) = pipewire_endpoints()
        && let Some(target) = endpoints
            .iter()
            .find(|endpoint| !endpoint.input && endpoint.label == name)
            .map(|endpoint| endpoint.target.clone())
    {
        let opened = with_pipewire_node(Some(&target), || {
            alsa::PCM::new("pipewire", alsa::Direction::Playback, false)
        });
        if let Ok(pcm) = opened {
            return Ok(pcm);
        }
        log::warn!("could not open the call's speaker {name}; using the default");
    }
    if !name.is_empty() && pipewire_endpoints().is_none() {
        let pcm_id = outputs().into_iter().find_map(|(shown, device)| {
            (shown == name)
                .then(|| describe(&device).and_then(|(_, driver)| driver))
                .flatten()
        });
        if let Some(pcm_id) = pcm_id {
            let opened = with_pipewire_node(None, || {
                alsa::PCM::new(&pcm_id, alsa::Direction::Playback, false)
            });
            if let Ok(pcm) = opened {
                return Ok(pcm);
            }
            log::warn!("could not open the call's speaker {name}; using the default");
        }
    }
    with_pipewire_node(None, || {
        alsa::PCM::new("default", alsa::Direction::Playback, false)
    })
    .map_err(|error| format!("No sound output: {error}"))
}

/// Mono 16-bit playback at the nearest rate to `source_rate`.
#[cfg(target_os = "linux")]
fn configure_playback(pcm: &alsa::PCM, source_rate: u32) -> Result<u32, String> {
    // A short buffer first. Some plugins reject a requested period, and then
    // the same device opens with whatever buffer it chooses.
    apply_playback(pcm, source_rate, true).or_else(|_| apply_playback(pcm, source_rate, false))
}

#[cfg(target_os = "linux")]
fn apply_playback(pcm: &alsa::PCM, source_rate: u32, short: bool) -> Result<u32, String> {
    let params = alsa::pcm::HwParams::any(pcm).map_err(|error| error.to_string())?;
    params
        .set_access(alsa::pcm::Access::RWInterleaved)
        .map_err(|error| error.to_string())?;
    params
        .set_format(alsa::pcm::Format::s16())
        .map_err(|error| error.to_string())?;
    params.set_channels(1).map_err(|error| error.to_string())?;
    let _ = params.set_rate_resample(true);
    let rate = params
        .set_rate_near(source_rate, alsa::ValueOr::Nearest)
        .map_err(|error| error.to_string())?;
    if short {
        // About 40 ms periods and a 120 ms buffer. The plugin's own default
        // holds closer to a second, which is a long delay on a call.
        let period = i64::from(rate / 25).max(1);
        let _ = params.set_period_size_near(period, alsa::ValueOr::Nearest);
        let _ = params.set_buffer_size_near(period.saturating_mul(3));
    }
    pcm.hw_params(&params).map_err(|error| error.to_string())?;
    let software = pcm.sw_params_current().map_err(|error| error.to_string())?;
    software
        .set_start_threshold(1)
        .map_err(|error| error.to_string())?;
    pcm.sw_params(&software)
        .map_err(|error| error.to_string())?;
    pcm.prepare().map_err(|error| error.to_string())?;
    Ok(rate.max(1))
}

/// Writes `samples` (mono) and recovers once from an underrun.
#[cfg(target_os = "linux")]
fn write_playback(pcm: &alsa::PCM, samples: &[i16]) -> Result<(), String> {
    if samples.is_empty() {
        return Ok(());
    }
    let io = pcm.io_i16().map_err(|error| error.to_string())?;
    let mut wrote = 0;
    let mut recovered = false;
    while wrote < samples.len() {
        match io.writei(&samples[wrote..]) {
            Ok(0) => return Err("The speaker accepted no audio.".to_owned()),
            Ok(frames) => wrote += frames,
            Err(error) if error.errno() == libc::EPIPE && !recovered => {
                pcm.prepare().map_err(|error| error.to_string())?;
                recovered = true;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn play_out(
    rate: u32,
    playout: async_channel::Receiver<Vec<i16>>,
    release: std::sync::mpsc::Receiver<()>,
    opened: &Opened,
    ringback: bool,
    line_open: Arc<AtomicBool>,
    speaker: &Mutex<String>,
) {
    let pipe = Arc::new(SamplePipe {
        samples: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
        done: AtomicBool::new(false),
    });
    let mut wanted = speaker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let mut held = match attach_speaker(&wanted, rate, &pipe) {
        Ok(held) => held,
        Err(error) => {
            let _ = opened.send(Err(error));
            return;
        }
    };
    let _ = opened.send(Ok(()));
    let mut playout = Playout::call(rate, playout, ringback, line_open);
    let mut rejected = String::new();
    let mut rejected_at = Instant::now();
    loop {
        match release.try_recv() {
            Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                pipe.finish();
                drop(held);
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        let next = speaker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if next != wanted && (next != rejected || rejected_at.elapsed() >= Duration::from_secs(1)) {
            match attach_speaker(&next, rate, &pipe) {
                Ok(opened_device) => {
                    held = opened_device;
                    wanted = next;
                    rejected.clear();
                }
                Err(error) => {
                    if next != rejected {
                        log::warn!("could not switch the call's speaker: {error}");
                        rejected = next;
                    }
                    rejected_at = Instant::now();
                }
            }
        }
        for _ in 0..480 {
            let Some(sample) = playout.next() else {
                pipe.finish();
                drop(held);
                return;
            };
            pipe.push(sample);
        }
    }
}

/// Endless mono source of the frames a call delivers. It plays silence while
/// none are waiting and ends only when the call closes the channel.
///
/// A call placed here plays ringback instead of those frames until the line
/// opens, then a short connect tone mixed with them. A call answered here
/// plays the frames at once, and the same tone once the line opens.
struct Playout {
    rate: NonZero<u32>,
    frames: async_channel::Receiver<Vec<i16>>,
    current: std::vec::IntoIter<i16>,
    /// Frames taken from the call, for the log when it ends. The library
    /// delivers one every 20 ms even when the other side has said nothing,
    /// so a count of frames is not a count of their audio.
    remote_frames: u64,
    /// Frames that were not silence. The first of these is their audio
    /// actually starting.
    sound_frames: u64,
    /// Ringback of a call placed here. Empty once the line is open.
    ringback: Vec<f32>,
    ring_at: usize,
    open: Arc<AtomicBool>,
    connect: std::vec::IntoIter<f32>,
    /// The connect tone has been queued. A plain playout sets this so it
    /// never plays one.
    connect_started: bool,
}

impl Playout {
    /// The other side only, with no local tone. Tests of the frame path use this.
    #[cfg(test)]
    fn new(rate: u32, frames: async_channel::Receiver<Vec<i16>>) -> Self {
        let mut playout = Self::call(rate, frames, false, Arc::new(AtomicBool::new(true)));
        playout.connect_started = true;
        playout
    }

    /// A call's speaker. `ringback` holds the local ring until `open` is set.
    fn call(
        rate: u32,
        frames: async_channel::Receiver<Vec<i16>>,
        ringback: bool,
        open: Arc<AtomicBool>,
    ) -> Self {
        let rate = NonZero::new(rate).unwrap_or(NonZero::new(WA_SAMPLE_RATE).expect("not zero"));
        Self {
            ringback: if ringback {
                ringback_cycle(rate.get())
            } else {
                Vec::new()
            },
            ring_at: 0,
            open,
            connect: Vec::new().into_iter(),
            connect_started: false,
            rate,
            frames,
            current: Vec::new().into_iter(),
            remote_frames: 0,
            sound_frames: 0,
        }
    }

    /// The next sample from the other side: silence when none is waiting,
    /// `None` when the call has closed the channel.
    fn remote(&mut self) -> Option<f32> {
        loop {
            if let Some(sample) = self.current.next() {
                return Some(from_pcm(sample));
            }
            match self.frames.try_recv() {
                Ok(frame) => {
                    self.remote_frames += 1;
                    if frame.iter().any(|sample| *sample != 0) {
                        self.sound_frames += 1;
                        if self.sound_frames == 1 {
                            log::info!("call speaker: the other side's audio started");
                        }
                    }
                    self.current = frame.into_iter();
                }
                Err(async_channel::TryRecvError::Empty) => return Some(0.0),
                Err(async_channel::TryRecvError::Closed) => return None,
            }
        }
    }

    /// Drops one waiting frame so a ringback cannot fill the queue. `false`
    /// once the call has closed the channel.
    fn discard_remote(&mut self) -> bool {
        self.current = Vec::new().into_iter();
        match self.frames.try_recv() {
            Ok(_) | Err(async_channel::TryRecvError::Empty) => true,
            Err(async_channel::TryRecvError::Closed) => false,
        }
    }
}

impl Iterator for Playout {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let open = self.open.load(Ordering::Relaxed);
        if !self.ringback.is_empty() && !open {
            if !self.discard_remote() {
                return None;
            }
            let sample = self.ringback[self.ring_at];
            self.ring_at = (self.ring_at + 1) % self.ringback.len();
            return Some(sample);
        }
        if open && !self.connect_started {
            self.connect_started = true;
            self.connect = connect_tone(self.rate.get()).into_iter();
        }
        if let Some(tone) = self.connect.next() {
            let remote = self.remote()?;
            return Some((remote + tone).clamp(-1.0, 1.0));
        }
        self.remote()
    }
}

impl Drop for Playout {
    fn drop(&mut self) {
        log::info!(
            "call speaker: {} frames with sound, {} frames in all",
            self.sound_frames,
            self.remote_frames
        );
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
    fn pipewire_lists_sinks_and_sources_and_skips_monitors() {
        let dump = r#"[
            {"info":{"props":{"media.class":"Audio/Sink","node.name":"alsa_output.pebble","node.description":"Pebble V3 Analog Stereo"}}},
            {"info":{"props":{"media.class":"Audio/Sink","node.name":"alsa_output.corsair","node.description":"CORSAIR Headset Analog Stereo"}}},
            {"info":{"props":{"media.class":"Audio/Source","node.name":"alsa_input.corsair","node.description":"CORSAIR Headset Mono"}}},
            {"info":{"props":{"media.class":"Audio/Source","node.name":"alsa_output.corsair.monitor","node.description":"CORSAIR Headset Monitor"}}},
            {"info":{"props":{"media.class":"Video/Source","node.name":"v4l2.kiyo","node.description":"Razer Kiyo"}}}
        ]"#;
        let endpoints = parse_pipewire_dump(dump);
        let labels: Vec<(&str, bool)> = endpoints
            .iter()
            .map(|endpoint| (endpoint.label.as_str(), endpoint.input))
            .collect();
        assert_eq!(
            labels,
            [
                ("Pebble V3 Analog Stereo", false),
                ("CORSAIR Headset Analog Stereo", false),
                ("CORSAIR Headset Mono", true),
            ]
        );
        assert_eq!(endpoints[2].target, "alsa_input.corsair");
    }

    /// Opens the first PipeWire microphone and reads a moment of it.
    /// `cargo test audio::tests::pipewire_microphone -- --ignored --nocapture`.
    #[test]
    #[ignore = "opens a microphone on this machine"]
    fn pipewire_microphone_delivers_samples_on_this_machine() {
        let list = list_devices();
        eprintln!("microphones: {:?}", list.microphones);
        eprintln!("speakers: {:?}", list.speakers);
        for speaker in &list.speakers {
            open_output(speaker).unwrap_or_else(|error| panic!("speaker {speaker}: {error}"));
        }
        let name = list
            .microphones
            .first()
            .cloned()
            .expect("PipeWire lists a microphone");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let opened = open_microphone(&name)
                .map(|mut microphone| microphone.by_ref().take(1_600).count());
            let _ = tx.send(opened);
        });
        let opened = rx
            .recv_timeout(Duration::from_secs(4))
            .expect("the microphone produced nothing before the timeout");
        let count = opened.expect("the microphone opened");
        assert_eq!(count, 1_600, "the microphone stalled");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn alsa_lists_one_device_per_card_and_each_hdmi_output() {
        let pcm = |id: &str, name: &str| (name.to_owned(), Some(id.to_owned()), id.to_owned());
        let kept = choices(
            [
                pcm("null", "Discard all samples"),
                pcm("pipewire", "PipeWire Sound Server"),
                pcm("default", "Default ALSA Output"),
                pcm("front:CARD=Generic", "Front output / input"),
                pcm(
                    "surround51:CARD=Generic",
                    "HD-Audio Generic, ALC1220 Analog",
                ),
                pcm("hw:CARD=0,DEV=0", "HD-Audio Generic, ALC1220 Analog"),
                pcm("front:CARD=Kiyo,DEV=0", "Razer Kiyo, USB Audio"),
                pcm("sysdefault:CARD=Kiyo", "Razer Kiyo, USB Audio"),
                pcm("iec958:CARD=Kiyo,DEV=0", "Razer Kiyo, USB Audio"),
                pcm("usbstream:CARD=Kiyo", "Razer Kiyo"),
                pcm("hdmi:CARD=HDMI,DEV=0", "HDA ATI HDMI, Dell AW3423DW"),
                pcm("hdmi:CARD=HDMI,DEV=1", "HDA ATI HDMI, HDMI 1"),
                pcm("hdmi:CARD=HDMI,DEV=2", "HDA ATI HDMI, HDMI 2"),
            ]
            .into_iter(),
        );
        let kept: Vec<(&str, &str)> = kept
            .iter()
            .map(|(name, id)| (name.as_str(), id.as_str()))
            .collect();
        assert_eq!(
            kept,
            [
                ("Generic, Front output / input", "front:CARD=Generic"),
                ("Razer Kiyo, USB Audio", "sysdefault:CARD=Kiyo"),
                ("HDA ATI HDMI, Dell AW3423DW", "hdmi:CARD=HDMI,DEV=0"),
            ]
        );
    }

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
    fn the_ringback_and_the_connect_tone_are_quiet() {
        let ringback = ringback_cycle(16_000);
        let tone = connect_tone(16_000);
        assert_eq!(ringback.len(), 16_000 * 3);
        assert!(tone.len() < 16_000, "the connect tone is under a second");
        for cycle in [&ringback, &tone] {
            assert!(cycle.iter().all(|sample| sample.abs() <= 0.25));
            assert!(cycle.iter().any(|sample| sample.abs() > 0.1));
            assert_eq!(cycle[0], 0.0);
        }
        assert_ne!(
            ringback, tone,
            "a line opening does not sound like the ringback"
        );
    }

    #[test]
    fn an_outgoing_call_rings_until_the_line_opens() {
        let (sender, receiver) = async_channel::bounded(4);
        let open = Arc::new(AtomicBool::new(false));
        let mut playout = Playout::call(16_000, receiver, true, Arc::clone(&open));
        sender.try_send(vec![16_384; 8]).unwrap();
        assert_eq!(playout.next(), Some(0.0), "the ringback fades in");
        assert!(sender.is_empty(), "remote audio waits out the ringback");
        let mut heard = false;
        for _ in 0..16_000 {
            let sample = playout.next().unwrap();
            assert!(sample.abs() <= 0.25);
            heard |= sample.abs() > 0.1;
        }
        assert!(heard, "the ringback is audible");

        open.store(true, Ordering::Relaxed);
        let tone_len = connect_tone(16_000).len();
        let mut peak = 0.0f32;
        for _ in 0..tone_len {
            let sample = playout.next().unwrap();
            peak = peak.max(sample.abs());
            assert!(sample.abs() <= 1.0);
        }
        assert!(peak > 0.1, "the connect tone plays as the line opens");
        assert_eq!(playout.next(), Some(0.0), "silence until the other side");
        sender.try_send(vec![16_384]).unwrap();
        assert!((playout.next().unwrap() - 0.5).abs() < 0.02);
    }

    #[test]
    fn an_answered_call_plays_the_other_side_then_the_connect_tone() {
        let (sender, receiver) = async_channel::bounded(4);
        let open = Arc::new(AtomicBool::new(false));
        let mut playout = Playout::call(16_000, receiver, false, Arc::clone(&open));
        sender.try_send(vec![16_384]).unwrap();
        assert!((playout.next().unwrap() - 0.5).abs() < 0.02);
        open.store(true, Ordering::Relaxed);
        let mut peak = 0.0f32;
        for _ in 0..connect_tone(16_000).len() {
            peak = peak.max(playout.next().unwrap().abs());
        }
        assert!(peak > 0.1);
        assert_eq!(playout.next(), Some(0.0));
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
        let (audio, endpoints) =
            CallAudio::start(16_000, false, String::new(), String::new()).expect("opens");
        audio.line_open();
        let started = Instant::now();
        let mut written = 0;
        let mut frames = 0;
        while started.elapsed() < Duration::from_millis(2_000) {
            if endpoints.sink.try_send(vec![0; 960]).is_ok() {
                written += 1;
            }
            while let Ok(frame) = endpoints.source.try_recv() {
                assert_eq!(frame.len(), 960);
                frames += 1;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("{frames} microphone frames read, {written} speaker frames taken");
        assert!(frames > 25, "the microphone stopped delivering");
        // 2 s of 60 ms frames is about 33; a speaker that never runs takes
        // only what fits in the queue.
        assert!(written > 25, "the speaker stopped taking frames");
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
        let recorder = Recorder::start(Waker::default(), String::new());
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
