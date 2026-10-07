//! Our camera in a video call: capture, H.264, and the access units the
//! library sends.
//!
//! The library never sees pixels. It wants complete H.264 Annex-B access
//! units, one per item, paced at 15 frames a second (its default RTP stride).
//! [`Camera`] opens the default camera, crops the centre into a portrait
//! [`WIDTH`]×[`HEIGHT`] picture, and encodes it with `openh264` on its own
//! thread. A phone lays a call out upright, so the wide webcam frame is not
//! sent whole.
//!
//! Capture is implemented on Linux. Elsewhere [`Camera::open`] fails and the
//! call still receives the other side's video.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Profile};
use openh264::formats::YUVBuffer;
use whatsapp_rust::async_channel::{self, TrySendError};

/// Size asked of the camera. Webcams offer landscape modes; the encoded
/// picture is a portrait crop of whatever comes back.
const CAPTURE_WIDTH: u32 = 640;
const CAPTURE_HEIGHT: u32 = 480;

/// Encoded picture size: a 3:4 portrait crop, the same number of pixels as
/// a 640×480 frame, so a phone shows the person upright instead of a wide
/// strip. Both sides are multiples of 16, which the encoder wants for a
/// whole number of macroblocks.
pub const WIDTH: usize = 480;
pub const HEIGHT: usize = 640;
const FPS: u32 = 15;
/// Access units waiting to be sent. A full queue drops the newest.
const QUEUE: usize = 2;

/// The latest picture from our camera, for the small view beside the other
/// person's. The camera thread replaces it; the interface uploads a texture.
#[derive(Clone)]
pub struct Preview {
    frame: Arc<Mutex<Option<Arc<egui::ColorImage>>>>,
    waker: crate::backend::Waker,
}

impl Preview {
    pub fn new(waker: crate::backend::Waker) -> Self {
        Self {
            frame: Arc::new(Mutex::new(None)),
            waker,
        }
    }

    fn publish(&self, image: egui::ColorImage) {
        *self
            .frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(image));
        self.waker.wake();
    }

    /// The newest picture, if the camera has produced one.
    pub fn latest(&self) -> Option<Arc<egui::ColorImage>> {
        self.frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Preview")
    }
}

/// A small RGB picture of a camera frame, at most 320 pixels wide.
pub(crate) fn preview_image(rgb: &[u8], width: usize, height: usize) -> egui::ColorImage {
    let (ox, oy, crop_w, crop_h) = cover(width, height);
    let (crop_w, crop_h) = (crop_w.max(1), crop_h.max(1));
    let long = crop_w.max(crop_h);
    let (out_w, out_h) = if long > 320 {
        let scale = 320.0 / long as f32;
        (
            ((crop_w as f32) * scale).round().max(1.0) as usize,
            ((crop_h as f32) * scale).round().max(1.0) as usize,
        )
    } else {
        (crop_w, crop_h)
    };
    let mut image = egui::ColorImage {
        size: [out_w, out_h],
        source_size: egui::vec2(out_w as f32, out_h as f32),
        pixels: vec![egui::Color32::BLACK; out_w * out_h],
    };
    for row in 0..out_h {
        let source_row = oy + row * crop_h / out_h;
        for column in 0..out_w {
            let source_column = ox + column * crop_w / out_w;
            let index = (source_row * width + source_column) * 3;
            if let Some(pixel) = rgb.get(index..index + 3) {
                image.pixels[row * out_w + column] =
                    egui::Color32::from_rgb(pixel[0], pixel[1], pixel[2]);
            }
        }
    }
    image
}

/// The source rectangle that fills [`WIDTH`]×[`HEIGHT`] without stretching:
/// `(x, y, width, height)`. A wider camera loses its sides, so the person
/// in the middle is what a phone shows.
fn cover(src_w: usize, src_h: usize) -> (usize, usize, usize, usize) {
    if src_w == 0 || src_h == 0 {
        return (0, 0, src_w, src_h);
    }
    let target = WIDTH as f64 / HEIGHT as f64;
    let source = src_w as f64 / src_h as f64;
    if source > target {
        let w = ((src_h as f64) * target).round() as usize;
        let w = w.clamp(1, src_w);
        ((src_w - w) / 2, 0, w, src_h)
    } else {
        let h = ((src_w as f64) / target).round() as usize;
        let h = h.clamp(1, src_h);
        (0, (src_h - h) / 2, src_w, h)
    }
}

/// Shows [`Preview`] in one texture, on the interface thread.
#[derive(Default)]
pub struct SelfView {
    shown: Option<Arc<egui::ColorImage>>,
    texture: Option<egui::TextureHandle>,
}

impl SelfView {
    /// The texture to draw, or `None` while there is no picture.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        preview: Option<&Preview>,
    ) -> Option<egui::TextureHandle> {
        let Some(preview) = preview else {
            self.shown = None;
            self.texture = None;
            return None;
        };
        let Some(image) = preview.latest() else {
            return self.texture.clone();
        };
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| Arc::ptr_eq(shown, &image))
        {
            return self.texture.clone();
        }
        self.shown = Some(Arc::clone(&image));
        let image = (*image).clone();
        match &mut self.texture {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.texture =
                    Some(ctx.load_texture("call-self", image, egui::TextureOptions::LINEAR));
            }
        }
        self.texture.clone()
    }
}

/// The default camera, encoding until dropped.
pub struct Camera {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    source: async_channel::Receiver<Vec<u8>>,
}

impl Camera {
    /// Opens the default camera and blocks until it has encoded a picture,
    /// or until `timeout`. The thread keeps going after this returns.
    pub fn open(timeout: Duration, name: &str, preview: Preview) -> Result<Self, String> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (timeout, name, preview);
            return Err("Sending your camera is not available on this system yet.".to_owned());
        }
        #[cfg(target_os = "linux")]
        {
            let (frames, source) = async_channel::bounded(QUEUE);
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let chosen = name.to_owned();
            let thread = std::thread::Builder::new()
                .name("call-camera".into())
                .spawn(move || capture(&stop_thread, &frames, &ready_tx, &chosen, &preview))
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
/// [`WIDTH`] by [`HEIGHT`], cropped from the centre to the portrait frame.
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
    let (ox, oy, crop_w, crop_h) = cover(src_w, src_h);
    for row in 0..HEIGHT {
        let sy = oy + row * crop_h / HEIGHT;
        for col in 0..WIDTH {
            let sx = ox + col * crop_w / WIDTH;
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
    name: &str,
    preview: &Preview,
) {
    let mut camera = match open_device(name) {
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
    if let Err(error) = send_one(&mut camera, &mut encoder, frames, preview) {
        let _ = ready.send(Err(error));
        return;
    }
    let _ = ready.send(Ok(()));
    let period = Duration::from_millis(1000 / u64::from(FPS));
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        if send_one(&mut camera, &mut encoder, frames, preview).is_err() {
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
    preview: &Preview,
) -> Result<(), String> {
    use nokhwa::pixel_format::RgbFormat;

    let frame = camera
        .frame()
        .map_err(|error| format!("The camera stopped: {error}"))?;
    let image = frame
        .decode_image::<RgbFormat>()
        .map_err(|error| format!("A camera picture could not be read: {error}"))?;
    preview.publish(preview_image(
        image.as_raw(),
        image.width() as usize,
        image.height() as usize,
    ));
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

/// Names of the cameras Settings can offer. Empty when this system does not
/// send video, or when none can be listed.
pub fn cameras() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        // A webcam often has a second node for metadata under the same name.
        let mut names: Vec<String> = Vec::new();
        for camera in nokhwa::query(nokhwa::utils::ApiBackend::Auto).unwrap_or_default() {
            let name = camera.human_name();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
    #[cfg(not(target_os = "linux"))]
    Vec::new()
}

#[cfg(target_os = "linux")]
fn open_device(name: &str) -> Result<nokhwa::Camera, String> {
    let mut last = "No camera was found.".to_owned();
    for index in camera_candidates(name) {
        match open_index(index) {
            Ok(camera) => return Ok(camera),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// Every node with `name`, or every node when `name` is empty.
///
/// A webcam often has a second node, under the same name, that reports no
/// picture formats. Opening that one fails with "Failed to Fufill", so the
/// caller tries each until one accepts a format.
#[cfg(target_os = "linux")]
fn camera_candidates(name: &str) -> Vec<nokhwa::utils::CameraIndex> {
    let found = nokhwa::query(nokhwa::utils::ApiBackend::Auto)
        .unwrap_or_default()
        .into_iter()
        .filter(|camera| name.is_empty() || camera.human_name() == name)
        .map(|camera| camera.index().clone())
        .collect::<Vec<_>>();
    if found.is_empty() {
        vec![nokhwa::utils::CameraIndex::Index(0)]
    } else {
        found
    }
}

/// Opens `index`. The closest 640×480 YUYV mode first, then any mode the
/// RGB decoder can read, so a camera that only offers MJPEG still opens.
#[cfg(target_os = "linux")]
fn open_index(index: nokhwa::utils::CameraIndex) -> Result<nokhwa::Camera, String> {
    use nokhwa::Camera;
    use nokhwa::pixel_format::RgbFormat;
    use nokhwa::utils::{
        CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
    };

    let requests = [
        RequestedFormatType::Closest(CameraFormat::new(
            Resolution::new(CAPTURE_WIDTH, CAPTURE_HEIGHT),
            FrameFormat::YUYV,
            FPS,
        )),
        RequestedFormatType::None,
    ];
    let mut last = "No camera was found.".to_owned();
    for requested in requests {
        let mut camera =
            match Camera::new(index.clone(), RequestedFormat::new::<RgbFormat>(requested)) {
                Ok(camera) => camera,
                Err(error) => {
                    last = format!("No camera was found: {error}");
                    continue;
                }
            };
        match camera.open_stream() {
            Ok(()) => return Ok(camera),
            Err(error) => last = format!("The camera could not be opened: {error}"),
        }
    }
    Err(last)
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
    preview: Preview,
}

impl Sending {
    pub fn silent(waker: crate::backend::Waker) -> Self {
        Self {
            inner: Mutex::new(Outbound::silent(waker)),
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

    /// Our picture, shared with the camera thread once it is open.
    pub fn preview(&self) -> Preview {
        self.lock().preview.clone()
    }

    /// Opens the camera, waiting up to three seconds. On failure the channel
    /// stays silent and [`Sending::notice`] says why.
    pub fn arm(&self, name: &str) -> bool {
        self.lock().arm(Duration::from_secs(3), name)
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
    fn silent(waker: crate::backend::Waker) -> Self {
        let (hold, source) = async_channel::bounded(1);
        Self {
            source,
            hold: Some(hold),
            camera: None,
            sending: false,
            notice: None,
            preview: Preview::new(waker),
        }
    }

    fn arm(&mut self, timeout: Duration, name: &str) -> bool {
        self.halt(true);
        self.notice = None;
        match Camera::open(timeout, name, self.preview.clone()) {
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

    /// A webcam can list a second node with the same name and no picture
    /// formats. Opening by that name, and with no name, has to skip it.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "opens a camera on this machine"]
    fn a_listed_camera_opens_past_a_node_with_no_formats() {
        let name = cameras().into_iter().next().expect("a camera is listed");
        drop(open_device(&name).expect("the named camera opens"));
        drop(open_device("").expect("the default camera opens"));
    }

    #[test]
    fn a_picture_scales_to_an_even_i420_frame() {
        let rgb = vec![255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let yuv = scale_rgb_to_i420(&rgb, 2, 2);
        assert_eq!(yuv.len(), WIDTH * HEIGHT * 3 / 2);
        assert!(yuv[..WIDTH * HEIGHT].iter().any(|sample| *sample > 16));
    }

    #[test]
    fn a_preview_is_a_small_copy_of_the_picture() {
        let rgb = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let image = preview_image(&rgb, 2, 2);
        assert_eq!(image.size, [2, 2]);
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(10, 20, 30));
        assert_eq!(image.pixels[1], egui::Color32::from_rgb(40, 50, 60));
        let wide = preview_image(&[0, 0, 0], 640, 480);
        assert_eq!(wide.size, [240, 320]);
    }

    #[test]
    fn a_wide_picture_keeps_its_centre() {
        // 4×2: white only in the two middle columns. The portrait crop drops
        // the sides, so the frame is bright. White only on the left edge is
        // dropped, so that frame stays black.
        let mut middle = vec![0u8; 4 * 2 * 3];
        for column in 1..3 {
            for row in 0..2 {
                let i = (row * 4 + column) * 3;
                middle[i..i + 3].fill(255);
            }
        }
        let yuv = scale_rgb_to_i420(&middle, 4, 2);
        assert!(yuv[..WIDTH * HEIGHT].iter().all(|sample| *sample > 200));

        let mut edge = vec![0u8; 4 * 2 * 3];
        for row in 0..2 {
            edge[row * 4 * 3..row * 4 * 3 + 3].fill(255);
        }
        let yuv = scale_rgb_to_i420(&edge, 4, 2);
        assert!(yuv[..WIDTH * HEIGHT].iter().all(|sample| *sample == 16));
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
        // The 1×1 source fills the portrait frame; the buffer is a real I420 frame.
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
