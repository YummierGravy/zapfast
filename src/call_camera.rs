//! Our camera in a video call: capture, H.264, and the access units the
//! library sends.
//!
//! The library never sees pixels. It wants complete H.264 Annex-B access
//! units, one per item, paced at 15 frames a second (its default RTP stride).
//! [`Camera`] opens the default camera, scales each picture to
//! [`WIDTH`]×[`HEIGHT`], and encodes it with `openh264` on its own thread.
//!
//! Capture is implemented on Linux. Elsewhere [`Camera::open`] fails and the
//! call still receives the other side's video.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Profile};
use openh264::formats::YUVBuffer;
use whatsapp_rust::async_channel::{self, TrySendError};

/// Encoded picture size. Both sides are multiples of 16, which the encoder
/// wants for a whole number of macroblocks.
pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 480;
const FPS: u32 = 15;
/// Access units waiting to be sent. A full queue drops the newest.
const QUEUE: usize = 2;

/// The default camera, encoding until dropped.
pub struct Camera {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    source: async_channel::Receiver<Vec<u8>>,
}

impl Camera {
    /// Opens the default camera and blocks until it has encoded a picture,
    /// or until `timeout`. The thread keeps going after this returns.
    pub fn open(timeout: Duration) -> Result<Self, String> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = timeout;
            return Err("Sending your camera is not available on this system yet.".to_owned());
        }
        #[cfg(target_os = "linux")]
        {
            let (frames, source) = async_channel::bounded(QUEUE);
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let thread = std::thread::Builder::new()
                .name("call-camera".into())
                .spawn(move || capture(&stop_thread, &frames, &ready_tx))
                .map_err(|error| format!("Could not start the camera: {error}"))?;
            let camera = Self {
                stop,
                thread: Some(thread),
                source,
            };
            match ready_rx.recv_timeout(timeout) {
                Ok(Ok(())) => Ok(camera),
                Ok(Err(error)) => {
                    drop(camera);
                    Err(error)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    drop(camera);
                    Err("The camera did not start.".to_owned())
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    drop(camera);
                    Err("The camera stopped before it started.".to_owned())
                }
            }
        }
    }

    /// Where encoded access units are read. Cloning does not take them away
    /// from this camera.
    pub fn source(&self) -> async_channel::Receiver<Vec<u8>> {
        self.source.clone()
    }
}

impl Camera {
    /// Asks the thread to leave and waits for it, so the device can be opened
    /// again. Call this from a blocking thread.
    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Detach. Joining here can block the worker on a camera read that
        // never returns; the thread leaves once that read does.
        drop(self.thread.take());
    }
}

/// Scales `rgb` (packed RGB, `src_w` by `src_h`) into an I420 picture of
/// [`WIDTH`] by [`HEIGHT`].
fn scale_rgb_to_i420(rgb: &[u8], src_w: usize, src_h: usize) -> Vec<u8> {
    let mut yuv = vec![0u8; WIDTH * HEIGHT * 3 / 2];
    let (y_plane, chroma) = yuv.split_at_mut(WIDTH * HEIGHT);
    let (u_plane, v_plane) = chroma.split_at_mut(WIDTH * HEIGHT / 4);
    if src_w == 0 || src_h == 0 || rgb.len() < src_w * src_h * 3 {
        // A black frame: Y 16, chroma 128.
        y_plane.fill(16);
        u_plane.fill(128);
        v_plane.fill(128);
        return yuv;
    }
    for row in 0..HEIGHT {
        let sy = row * src_h / HEIGHT;
        for col in 0..WIDTH {
            let sx = col * src_w / WIDTH;
            let i = (sy * src_w + sx) * 3;
            let (r, g, b) = (rgb[i] as i32, rgb[i + 1] as i32, rgb[i + 2] as i32);
            // BT.601, limited range.
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[row * WIDTH + col] = y.clamp(0, 255) as u8;
            if row % 2 == 0 && col % 2 == 0 {
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                let c = (row / 2) * (WIDTH / 2) + col / 2;
                u_plane[c] = u.clamp(0, 255) as u8;
                v_plane[c] = v.clamp(0, 255) as u8;
            }
        }
    }
    yuv
}

fn encoder() -> Result<Encoder, String> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(800_000))
        .max_frame_rate(FrameRate::from_hz(FPS as f32))
        .profile(Profile::Baseline)
        .intra_frame_period(IntraFramePeriod::from_num_frames(FPS * 2))
        .skip_frames(false);
    Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
        .map_err(|error| format!("The video encoder could not start: {error}"))
}

fn encode(encoder: &mut Encoder, i420: &[u8]) -> Result<Vec<u8>, String> {
    let yuv = YUVBuffer::from_vec(i420.to_vec(), WIDTH, HEIGHT);
    let stream = encoder
        .encode(&yuv)
        .map_err(|error| format!("A camera picture could not be encoded: {error}"))?;
    Ok(stream.to_vec())
}

#[cfg(target_os = "linux")]
fn capture(
    stop: &AtomicBool,
    frames: &async_channel::Sender<Vec<u8>>,
    ready: &std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    let mut camera = match open_device() {
        Ok(camera) => camera,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut encoder = match encoder() {
        Ok(encoder) => encoder,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if let Err(error) = send_one(&mut camera, &mut encoder, frames) {
        let _ = ready.send(Err(error));
        return;
    }
    let _ = ready.send(Ok(()));
    let period = Duration::from_millis(1000 / u64::from(FPS));
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        if send_one(&mut camera, &mut encoder, frames).is_err() {
            return;
        }
        let deadline = started + period;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let slice =
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now()));
            if slice.is_zero() {
                break;
            }
            std::thread::sleep(slice);
        }
    }
}

#[cfg(target_os = "linux")]
fn send_one(
    camera: &mut nokhwa::Camera,
    encoder: &mut Encoder,
    frames: &async_channel::Sender<Vec<u8>>,
) -> Result<(), String> {
    use nokhwa::pixel_format::RgbFormat;

    let frame = camera
        .frame()
        .map_err(|error| format!("The camera stopped: {error}"))?;
    let image = frame
        .decode_image::<RgbFormat>()
        .map_err(|error| format!("A camera picture could not be read: {error}"))?;
    let i420 = scale_rgb_to_i420(
        image.as_raw(),
        image.width() as usize,
        image.height() as usize,
    );
    let access_unit = encode(encoder, &i420)?;
    if access_unit.is_empty() {
        return Ok(());
    }
    match frames.try_send(access_unit) {
        Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
        Err(TrySendError::Closed(_)) => Err("The call stopped reading the camera.".to_owned()),
    }
}

#[cfg(target_os = "linux")]
fn open_device() -> Result<nokhwa::Camera, String> {
    use nokhwa::Camera;
    use nokhwa::pixel_format::RgbFormat;
    use nokhwa::utils::{
        CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
    };

    let requested =
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(CameraFormat::new(
            Resolution::new(WIDTH as u32, HEIGHT as u32),
            FrameFormat::YUYV,
            FPS,
        )));
    let mut camera = Camera::new(CameraIndex::Index(0), requested)
        .map_err(|error| format!("No camera was found: {error}"))?;
    camera
        .open_stream()
        .map_err(|error| format!("The camera could not be opened: {error}"))?;
    Ok(camera)
}

/// Whether our camera is on, shared by the call and the tasks that change it.
pub struct Sending {
    inner: Mutex<Outbound>,
}

struct Outbound {
    source: async_channel::Receiver<Vec<u8>>,
    /// Keeps the channel open while nothing is sending, so the library's
    /// read does not end.
    hold: Option<async_channel::Sender<Vec<u8>>>,
    camera: Option<Camera>,
    sending: bool,
    /// Why the camera is off, when the person should be told.
    notice: Option<String>,
}

impl Sending {
    pub fn silent() -> Self {
        Self {
            inner: Mutex::new(Outbound::silent()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Outbound> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The channel the library reads. Empty until [`Sending::arm`].
    pub fn source(&self) -> async_channel::Receiver<Vec<u8>> {
        self.lock().source.clone()
    }

    pub fn sending(&self) -> bool {
        self.lock().sending
    }

    /// A sentence for the interface when the camera could not be used.
    pub fn notice(&self) -> Option<String> {
        self.lock().notice.clone()
    }

    /// Opens the camera, waiting up to three seconds. On failure the channel
    /// stays silent and [`Sending::notice`] says why.
    pub fn arm(&self) -> bool {
        self.lock().arm(Duration::from_secs(3))
    }

    /// Closes the camera and waits for its thread. The channel stays open
    /// and silent. Call this from a blocking thread.
    pub fn halt(&self) {
        self.lock().halt(true);
    }

    /// Asks the camera thread to leave without waiting. The channel stays
    /// open and silent.
    pub fn release(&self) {
        self.lock().halt(false);
    }
}

impl Outbound {
    fn silent() -> Self {
        let (hold, source) = async_channel::bounded(1);
        Self {
            source,
            hold: Some(hold),
            camera: None,
            sending: false,
            notice: None,
        }
    }

    fn arm(&mut self, timeout: Duration) -> bool {
        self.halt(true);
        self.notice = None;
        match Camera::open(timeout) {
            Ok(camera) => {
                self.source = camera.source();
                self.hold = None;
                self.camera = Some(camera);
                self.sending = true;
                true
            }
            Err(error) => {
                self.notice = Some(error);
                false
            }
        }
    }

    fn halt(&mut self, join: bool) {
        if let Some(camera) = self.camera.take()
            && join
        {
            camera.shutdown();
        }
        self.sending = false;
        if self.hold.is_none() {
            let (hold, source) = async_channel::bounded(1);
            self.hold = Some(hold);
            self.source = source;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_scales_to_an_even_i420_frame() {
        let rgb = vec![255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let yuv = scale_rgb_to_i420(&rgb, 2, 2);
        assert_eq!(yuv.len(), WIDTH * HEIGHT * 3 / 2);
        assert!(yuv[..WIDTH * HEIGHT].iter().any(|sample| *sample > 16));
    }

    #[test]
    fn a_short_picture_becomes_a_black_frame() {
        let yuv = scale_rgb_to_i420(&[], 0, 0);
        assert!(yuv[..WIDTH * HEIGHT].iter().all(|sample| *sample == 16));
        assert!(yuv[WIDTH * HEIGHT..].iter().all(|sample| *sample == 128));
    }

    #[test]
    fn the_first_encoded_picture_is_a_keyframe() {
        let mut encoder = encoder().expect("encoder");
        let yuv = scale_rgb_to_i420(&[40, 80, 120], 1, 1);
        // The 1×1 source is stretched; the buffer is a real I420 frame.
        let access_unit = encode(&mut encoder, &yuv).expect("encoded");
        assert!(
            access_unit.starts_with(&[0, 0, 0, 1]) || access_unit.starts_with(&[0, 0, 1]),
            "Annex-B start code"
        );
        assert!(
            whatsapp_rust::wacore::voip::h264::au_has_idr(&access_unit),
            "the first picture is an IDR"
        );
        let next = encode(&mut encoder, &yuv).expect("second");
        assert!(!next.is_empty());
    }
}
