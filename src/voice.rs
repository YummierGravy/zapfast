//! WhatsApp voice-message encoding and decoding: mono 48 kHz Opus in OGG.
//!
//! libopus is built into the app.

use std::io::Cursor;

/// Opus sample rate used for every mono clip.
pub const RATE: u32 = 48_000;
/// One Opus frame: 20 ms.
const FRAME: usize = 960;
/// Maximum decoded Opus packet size: 120 ms of stereo.
const LONGEST_PACKET: usize = 5760 * 2;
/// Number of bars in a WhatsApp voice-message waveform.
pub const BARS: usize = 64;
/// Target bitrate for speech, similar to the phone.
const BITRATE: i32 = 32_000;

/// Mono samples at `RATE` from an OGG/Opus file.
pub fn decode(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let mut reader = ogg::PacketReader::new(Cursor::new(bytes));
    let mut stream: Option<Stream> = None;
    let mut out = Vec::new();
    let mut scratch = vec![0f32; LONGEST_PACKET];
    loop {
        let packet = match reader.read_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => return Err(format!("bad OGG stream: {error}")),
        };
        let Some(current) = stream.as_mut() else {
            stream = Some(Stream::open(&packet.data)?);
            continue;
        };
        if !current.tagged {
            // Skip the optional Opus comment header.
            current.tagged = true;
            continue;
        }
        let frames = current
            .decoder
            .decode_float(&packet.data, &mut scratch, false)
            .map_err(|error| format!("bad Opus packet: {error}"))?;
        let decoded = &scratch[..frames * current.channels];
        let mono: Vec<f32> = if current.channels == 2 {
            decoded
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[left, right]| (left + right) * 0.5)
                .collect()
        } else {
            decoded.to_vec()
        };
        // Remove the encoder lookahead from the decoded output.
        let skip = current.skip.min(mono.len());
        current.skip -= skip;
        out.extend_from_slice(&mono[skip..]);
    }
    if stream.is_none() {
        return Err("not an OGG stream".to_owned());
    }
    Ok(out)
}

struct Stream {
    decoder: opus::Decoder,
    channels: usize,
    skip: usize,
    tagged: bool,
}

impl Stream {
    /// Reads the Opus identification header.
    fn open(head: &[u8]) -> Result<Self, String> {
        let head = head
            .strip_prefix(b"OpusHead")
            .ok_or_else(|| "not an Opus stream".to_owned())?;
        if head.len() < 11 {
            return Err("truncated Opus header".to_owned());
        }
        let channels = usize::from(head[1]);
        let skip = usize::from(u16::from_le_bytes([head[2], head[3]]));
        let layout = match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            other => return Err(format!("{other} channels")),
        };
        let decoder = opus::Decoder::new(RATE, layout).map_err(|error| error.to_string())?;
        Ok(Self {
            decoder,
            channels,
            skip,
            tagged: false,
        })
    }
}

/// An OGG/Opus file from mono samples at `RATE`.
pub fn encode(samples: &[f32]) -> Result<Vec<u8>, String> {
    use ogg::PacketWriteEndInfo::{EndPage, EndStream, NormalPacket};
    let mut encoder = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)
        .map_err(|error| error.to_string())?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(BITRATE))
        .map_err(|error| error.to_string())?;
    let pre_skip = encoder
        .get_lookahead()
        .map_err(|error| error.to_string())?
        .max(0) as usize;
    let serial = 0x5641_5341;
    let io = |error: std::io::Error| error.to_string();
    let mut writer = ogg::PacketWriter::new(Vec::new());
    writer
        .write_packet(opus_head(pre_skip as u16), serial, EndPage, 0)
        .map_err(io)?;
    writer
        .write_packet(opus_tags(), serial, EndPage, 0)
        .map_err(io)?;
    // Append silence to flush encoder lookahead. The final granule trims it.
    let frames = (samples.len() + pre_skip).div_ceil(FRAME).max(1);
    let mut frame = vec![0f32; FRAME];
    let mut packet = vec![0u8; 4000];
    for index in 0..frames {
        frame.fill(0.0);
        let start = index * FRAME;
        if start < samples.len() {
            let end = (start + FRAME).min(samples.len());
            frame[..end - start].copy_from_slice(&samples[start..end]);
        }
        let written = encoder
            .encode_float(&frame, &mut packet)
            .map_err(|error| error.to_string())?;
        let last = index + 1 == frames;
        let granule = if last {
            (pre_skip + samples.len()) as u64
        } else {
            ((index + 1) * FRAME) as u64
        };
        let end = if last {
            EndStream
        } else if (index + 1) % 50 == 0 {
            EndPage
        } else {
            NormalPacket
        };
        writer
            .write_packet(packet[..written].to_vec(), serial, end, granule)
            .map_err(io)?;
    }
    Ok(writer.into_inner())
}

fn opus_head(pre_skip: u16) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(1);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&RATE.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    head
}

fn opus_tags() -> Vec<u8> {
    let vendor = b"ZapFast";
    let mut tags = Vec::with_capacity(20 + vendor.len());
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes());
    tags
}

/// Normalizes a quiet recording to just below full scale, with capped gain.
pub fn normalize(samples: &mut [f32]) {
    let peak = samples
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    if peak <= 0.0 {
        return;
    }
    let gain = (0.89 / peak).clamp(1.0, 10.0);
    if gain > 1.0 {
        for sample in samples {
            *sample *= gain;
        }
    }
}

/// Builds WhatsApp's 0–100 waveform bars from clip loudness.
pub fn waveform(samples: &[f32]) -> Vec<u8> {
    if samples.is_empty() {
        return vec![0; BARS];
    }
    let slice = samples.len().div_ceil(BARS);
    let loudness: Vec<f32> = samples
        .chunks(slice)
        .map(|chunk| (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt())
        .collect();
    let loudest = loudness.iter().copied().fold(0.0f32, f32::max);
    let mut bars: Vec<u8> = loudness
        .iter()
        .map(|value| {
            if loudest > 0.0 {
                (value / loudest * 100.0).round() as u8
            } else {
                0
            }
        })
        .collect();
    bars.resize(BARS, 0);
    bars
}

/// Mixes interleaved channels to mono and resamples to `RATE`.
pub fn mono_at_rate(interleaved: &[f32], channels: u16, rate: u32) -> Vec<f32> {
    mono_resampled(interleaved, channels, rate, RATE)
}

/// Mixes interleaved channels to mono and resamples from `rate` to `target`.
pub fn mono_resampled(interleaved: &[f32], channels: u16, rate: u32, target: u32) -> Vec<f32> {
    let channels = usize::from(channels.max(1));
    let mono: Vec<f32> = interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    if rate == target || rate == 0 || target == 0 || mono.is_empty() {
        return mono;
    }
    let ratio = f64::from(rate) / f64::from(target);
    let count = (mono.len() as f64 / ratio).floor() as usize;
    (0..count)
        .map(|index| {
            let position = index as f64 * ratio;
            let left = position.floor() as usize;
            let t = (position - left as f64) as f32;
            let a = mono[left.min(mono.len() - 1)];
            let b = mono.get(left + 1).copied().unwrap_or(a);
            a + (b - a) * t
        })
        .collect()
}

/// [`mono_resampled`] for a stream that arrives in pieces of any size, such
/// as a microphone during a call. Whatever ends a piece, whether part of an
/// interleaved frame or part of an interpolation step, is carried into the
/// next one, so no sample is lost or repeated at the seams.
pub struct MonoStream {
    channels: usize,
    /// Input samples per output sample; 1 when the rates match.
    ratio: f64,
    /// Interleaved samples of a frame that has not arrived whole.
    partial: Vec<f32>,
    /// Mono samples not yet passed over. Output position `position` counts
    /// from its first sample, and may lie past its end after a large step.
    tail: Vec<f32>,
    position: f64,
}

impl MonoStream {
    /// A stream of `channels` interleaved channels at `rate`, resampled to
    /// `target`. A zero rate leaves the samples as they are.
    pub fn new(channels: u16, rate: u32, target: u32) -> Self {
        let ratio = if rate == 0 || target == 0 {
            1.0
        } else {
            f64::from(rate) / f64::from(target)
        };
        Self {
            channels: usize::from(channels.max(1)),
            ratio,
            partial: Vec::new(),
            tail: Vec::new(),
            position: 0.0,
        }
    }

    /// Takes the next interleaved samples and returns the mono samples at
    /// the target rate that they complete.
    pub fn push(&mut self, interleaved: &[f32]) -> Vec<f32> {
        self.partial.extend_from_slice(interleaved);
        let whole = self.partial.len() / self.channels * self.channels;
        let channels = self.channels as f32;
        self.tail.extend(
            self.partial[..whole]
                .chunks_exact(self.channels)
                .map(|frame| frame.iter().sum::<f32>() / channels),
        );
        self.partial.drain(..whole);
        if self.ratio == 1.0 {
            self.position = 0.0;
            return std::mem::take(&mut self.tail);
        }
        let mut out = Vec::new();
        // Interpolation needs the sample after the one the position is in.
        while (self.position as usize) + 1 < self.tail.len() {
            let left = self.position as usize;
            let t = (self.position - left as f64) as f32;
            let (a, b) = (self.tail[left], self.tail[left + 1]);
            out.push(a + (b - a) * t);
            self.position += self.ratio;
        }
        let passed = (self.position as usize).min(self.tail.len());
        self.tail.drain(..passed);
        self.position -= passed as f64;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds: f32) -> Vec<f32> {
        (0..(RATE as f32 * seconds) as usize)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.5)
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn a_clip_survives_the_trip_through_opus() {
        let original = tone(1.0);
        let bytes = encode(&original).expect("encodes");
        assert!(bytes.starts_with(b"OggS"));
        let decoded = decode(&bytes).expect("decodes");
        let drift = decoded.len() as i64 - original.len() as i64;
        assert!(
            drift.abs() <= FRAME as i64,
            "{} samples back",
            decoded.len()
        );
        let end = decoded.len().min(original.len());
        let (before, after) = (rms(&original[4800..end]), rms(&decoded[4800..end]));
        assert!((before - after).abs() < 0.08, "{before} in, {after} out");
    }

    #[test]
    fn the_waveform_follows_the_loudness() {
        let mut samples = tone(1.0);
        let count = samples.len() as f32;
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample *= index as f32 / count;
        }
        let bars = waveform(&samples);
        assert_eq!(bars.len(), BARS);
        assert_eq!(bars[BARS - 1], 100);
        assert!(bars[0] < 10, "{}", bars[0]);
        assert!(bars.windows(2).all(|pair| pair[0] <= pair[1] + 2));
        assert_eq!(waveform(&[]), vec![0; BARS]);
    }

    #[test]
    fn any_input_becomes_mono_at_48k() {
        let stereo: Vec<f32> = (0..44_100 * 2)
            .map(|i| if i % 2 == 0 { 1.0 } else { 0.0 })
            .collect();
        let mono = mono_at_rate(&stereo, 2, 44_100);
        assert!((mono.len() as i64 - 48_000).abs() <= 2, "{}", mono.len());
        assert!(mono.iter().all(|v| (v - 0.5).abs() < 1e-6));
        assert_eq!(mono_at_rate(&[0.25; 10], 1, RATE), vec![0.25; 10]);
    }

    #[test]
    fn a_quiet_recording_is_brought_up_but_not_blasted() {
        let mut quiet: Vec<f32> = tone(0.1).iter().map(|sample| sample * 0.2).collect();
        normalize(&mut quiet);
        let peak = quiet.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!((peak - 0.89).abs() < 0.01, "{peak}");
        // Cap gain at 20 dB.
        let mut faint = vec![0.001f32, -0.002];
        normalize(&mut faint);
        assert!((faint[1] + 0.02).abs() < 1e-6, "{}", faint[1]);
        // Do not reduce already loud recordings.
        let mut loud = vec![0.95f32];
        normalize(&mut loud);
        assert_eq!(loud, vec![0.95]);
        normalize(&mut []);
    }

    #[test]
    fn what_is_not_opus_is_refused() {
        assert!(decode(b"not an ogg file at all").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn call_rates_come_out_at_16k() {
        for rate in [44_100u32, 48_000] {
            let input = vec![0.25f32; rate as usize];
            let out = mono_resampled(&input, 1, rate, 16_000);
            assert!(
                (out.len() as i64 - 16_000).abs() <= 2,
                "{rate}: {}",
                out.len()
            );
            assert!(out.iter().all(|v| (v - 0.25).abs() < 1e-6));
        }
        // A rate that already matches is only downmixed.
        assert_eq!(mono_resampled(&[0.5; 8], 1, 16_000, 16_000), vec![0.5; 8]);
    }

    #[test]
    fn a_resampled_ramp_keeps_its_slope() {
        // 48 kHz to 16 kHz reads every third sample.
        let ramp: Vec<f32> = (0..4_800).map(|i| i as f32).collect();
        let out = mono_resampled(&ramp, 1, 48_000, 16_000);
        assert_eq!(out.len(), 1_600);
        for (index, value) in out.iter().enumerate() {
            assert!(
                (value - 3.0 * index as f32).abs() < 1e-3,
                "{index}: {value}"
            );
        }
    }

    #[test]
    fn stereo_is_averaged_before_it_is_resampled() {
        let stereo: Vec<f32> = (0..48_000 * 2)
            .map(|i| if i % 2 == 0 { 1.0 } else { -0.5 })
            .collect();
        let out = mono_resampled(&stereo, 2, 48_000, 16_000);
        assert!((out.len() as i64 - 16_000).abs() <= 2, "{}", out.len());
        assert!(out.iter().all(|v| (v - 0.25).abs() < 1e-6));
    }

    #[test]
    fn a_stream_in_pieces_matches_the_clip_in_one() {
        for (channels, rate) in [(1u16, 48_000u32), (2, 44_100), (2, 48_000), (1, 16_000)] {
            let interleaved: Vec<f32> = (0..rate as usize * usize::from(channels) / 2)
                .map(|i| ((i * 7) % 101) as f32 / 101.0 - 0.5)
                .collect();
            let whole = mono_resampled(&interleaved, channels, rate, 16_000);
            for piece in [1usize, 3, 441, 1_000, 4_096] {
                let mut stream = MonoStream::new(channels, rate, 16_000);
                let mut joined = Vec::new();
                for part in interleaved.chunks(piece) {
                    joined.extend(stream.push(part));
                }
                // The last sample waits for a successor that never comes.
                let common = joined.len().min(whole.len());
                assert!(whole.len() - common <= 2, "{rate} Hz, pieces of {piece}");
                assert!(joined.len() <= whole.len());
                for (index, (a, b)) in joined.iter().zip(&whole).enumerate() {
                    assert!(
                        (a - b).abs() < 1e-4,
                        "{channels} ch {rate} Hz, pieces of {piece}, sample {index}"
                    );
                }
            }
        }
    }

    /// Decodes the file in `ZAPFAST_OGG_PROBE`:
    /// `ZAPFAST_OGG_PROBE=note.ogg cargo test voice::tests::probe -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a file to look at"]
    fn probe() {
        let Some(path) = std::env::var_os("ZAPFAST_OGG_PROBE") else {
            return;
        };
        let bytes = std::fs::read(path).expect("readable");
        let started = std::time::Instant::now();
        let samples = decode(&bytes).expect("decodes");
        eprintln!(
            "{} samples ({:.2} s), loudness {:.3}, in {:?}; bars {:?}",
            samples.len(),
            samples.len() as f32 / RATE as f32,
            rms(&samples),
            started.elapsed(),
            &waveform(&samples)[..8]
        );
        assert!(!samples.is_empty());
    }
}
