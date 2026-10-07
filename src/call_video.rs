//! The other side's video in a call, received and shown, and our camera when
//! it can be opened.
//!
//! The library hands over H.264 access units in Annex B form
//! ([`VideoFrame`]). [`Reception`] decodes them with `openh264` (as `video`
//! does for video messages) on a thread of its own, and puts only the newest
//! decoded picture, still YUV, into a [`Feed`]: a picture the interface has
//! not taken by the time the next one is ready is dropped. The interface
//! thread's [`Screen`] converts the newest picture to RGB with
//! `video::convert` and uploads it into a single texture.
//!
//! A decoder that starts, falls behind, or fails cannot use the units that
//! follow until the next IDR. It skips them and asks the other side for a
//! keyframe each time; the library throttles those requests.

use std::sync::{Arc, Mutex, OnceLock};

use egui::{ColorImage, TextureHandle, TextureOptions};
use whatsapp_rust::async_channel;
use whatsapp_rust::voip::VideoFrame;
use whatsapp_rust::wacore::voip::h264::au_has_idr;

use crate::backend::Waker;

/// Access units the library may queue before the decoder takes them, about
/// 0.4 s at 20 frames a second. The library drops units that find it full.
const QUEUE: usize = 8;
/// Longest side, in pixels, a picture is converted to for the screen.
const SIDE: u32 = 960;

/// One decoded picture, YUV 4:2:0 as the decoder left it.
pub struct Picture {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    strides: (usize, usize, usize),
    size: (usize, usize),
    /// Clockwise quarter turns that make it upright.
    turns: u8,
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Picture")
            .field("size", &self.size)
            .field("turns", &self.turns)
            .finish_non_exhaustive()
    }
}

impl Picture {
    fn of(yuv: &openh264::decoder::DecodedYUV<'_>) -> Option<Self> {
        use openh264::formats::YUVSource;

        let picture = Self {
            y: yuv.y().to_vec(),
            u: yuv.u().to_vec(),
            v: yuv.v().to_vec(),
            strides: yuv.strides(),
            size: yuv.dimensions(),
            turns: 0,
        };
        picture.whole().then_some(picture)
    }

    /// Whether the planes hold every sample the conversion reads.
    fn whole(&self) -> bool {
        let (width, height) = self.size;
        if width == 0 || height == 0 {
            return false;
        }
        // The last sample of each plane the conversion reads.
        let luma = (height - 1) * self.strides.0 + width - 1;
        let chroma = |stride: usize| (height - 1) / 2 * stride + (width - 1) / 2;
        luma < self.y.len()
            && chroma(self.strides.1) < self.u.len()
            && chroma(self.strides.2) < self.v.len()
    }

    /// Width and height as decoded, before turning.
    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    /// The upright picture in RGB, no larger than `side` on its longest side.
    fn image(&self, side: u32) -> ColorImage {
        let (width, height) = crate::video::fitted(self.size.0 as u32, self.size.1 as u32, side);
        let planes = crate::video::Planes {
            y: &self.y,
            u: &self.u,
            v: &self.v,
            strides: self.strides,
            size: self.size,
        };
        crate::video::convert(&planes, (width as usize, height as usize), self.turns)
    }

    /// A synthetic picture for demos and tests: a soft colour gradient.
    #[cfg(any(test, feature = "demo"))]
    pub fn pattern(width: usize, height: usize) -> Self {
        let (half_width, half_height) = (width.div_ceil(2), height.div_ceil(2));
        let y = (0..height)
            .flat_map(|row| (0..width).map(move |column| (row, column)))
            .map(|(row, column)| (60 + 120 * (row + column) / (width + height)) as u8)
            .collect();
        let u = (0..half_height)
            .flat_map(|_| (0..half_width).map(|column| (150 - 40 * column / half_width) as u8))
            .collect();
        let v = (0..half_height)
            .flat_map(|row| (0..half_width).map(move |_| (110 + 40 * row / half_height) as u8))
            .collect();
        Self {
            y,
            u,
            v,
            strides: (width, half_width, half_width),
            size: (width, height),
            turns: 0,
        }
    }
}

/// Clockwise quarter turns that undo the rotation the other side's camera
/// reported. WhatsApp's orientation counts the other way.
fn upright(orientation: u8) -> u8 {
    (4 - (orientation & 3)) % 4
}

#[derive(Default)]
struct Slot {
    newest: Option<Picture>,
    /// Whether the other side's video is on, as far as pictures and their
    /// signalling tell.
    showing: bool,
    /// Pictures replaced before the interface took them.
    late: u64,
}

struct Shared {
    slot: Mutex<Slot>,
    waker: Waker,
}

/// The hand-off between a call's decode thread and the interface: the
/// newest picture, and whether the other side's video is on. Clones share it.
#[derive(Clone)]
pub struct Feed(Arc<Shared>);

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Feed")
    }
}

impl PartialEq for Feed {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Feed {}

/// What the interface finds in a [`Feed`].
#[derive(Debug)]
pub enum Newest {
    /// The other side's video is off, or no picture has arrived yet.
    Off,
    /// Nothing new since the last picture taken.
    Unchanged,
    Picture(Picture),
}

impl Feed {
    pub fn new(waker: Waker) -> Self {
        Self(Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            waker,
        }))
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.0.slot.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Hands over a decoded picture in place of one not yet taken. A picture
    /// means the other side's video is on.
    pub fn put(&self, picture: Picture) {
        {
            let mut slot = self.slot();
            if slot.newest.replace(picture).is_some() {
                slot.late += 1;
            }
            slot.showing = true;
        }
        self.0.waker.wake();
    }

    /// The other side turned their video off: the last picture goes, and
    /// the next one turns it back on.
    pub fn stopped(&self) {
        {
            let mut slot = self.slot();
            slot.newest = None;
            slot.showing = false;
        }
        self.0.waker.wake();
    }

    /// Takes the newest picture.
    pub fn take(&self) -> Newest {
        let mut slot = self.slot();
        if !slot.showing {
            return Newest::Off;
        }
        slot.newest
            .take()
            .map_or(Newest::Unchanged, Newest::Picture)
    }

    /// How many pictures were dropped for a newer one.
    pub fn late(&self) -> u64 {
        self.slot().late
    }
}

/// Shows one call's video in a single texture, on the interface thread.
#[derive(Default)]
pub struct Screen {
    feed: Option<Feed>,
    texture: Option<TextureHandle>,
}

impl Screen {
    /// Uploads `feed`'s newest picture, if there is one, and returns the
    /// texture to draw, or `None` while there is no video to show. Without
    /// a feed it lets go of the texture.
    pub fn show(&mut self, ctx: &egui::Context, feed: Option<&Feed>) -> Option<TextureHandle> {
        if self.feed.as_ref() != feed {
            self.feed = feed.cloned();
            self.texture = None;
        }
        match self.feed.as_ref()?.take() {
            Newest::Off => self.texture = None,
            Newest::Unchanged => {}
            Newest::Picture(picture) => {
                let image = picture.image(SIDE);
                match &mut self.texture {
                    Some(texture) => texture.set(image, TextureOptions::LINEAR),
                    None => {
                        self.texture =
                            Some(ctx.load_texture("call-video", image, TextureOptions::LINEAR));
                    }
                }
            }
        }
        self.texture.clone()
    }
}

/// Asks the other side for a keyframe, once there is a call to ask on.
#[derive(Clone, Default)]
struct Ask(Arc<OnceLock<Box<dyn Fn() + Send + Sync>>>);

impl Ask {
    fn ask(&self) {
        if let Some(ask) = self.0.get() {
            ask();
        }
    }
}

/// Receives the other side's video for one call, and sends ours when the
/// camera is on. Dropping it stops both threads.
pub struct Reception {
    feed: Feed,
    frames: async_channel::Sender<VideoFrame>,
    camera: std::sync::Arc<crate::call_camera::Sending>,
    ask: Ask,
}

impl Reception {
    /// Starts the decode thread. Each picture handed over wakes the window.
    pub fn start(waker: Waker) -> std::io::Result<Self> {
        let feed = Feed::new(waker);
        let ask = Ask::default();
        let (frames, incoming) = async_channel::bounded(QUEUE);
        {
            let feed = feed.clone();
            let ask = ask.clone();
            std::thread::Builder::new()
                .name("call-video".into())
                .spawn(move || run(&incoming, &feed, &ask))?;
        }
        Ok(Self {
            feed,
            frames,
            camera: std::sync::Arc::new(crate::call_camera::Sending::silent()),
            ask,
        })
    }

    pub fn feed(&self) -> Feed {
        self.feed.clone()
    }

    /// Where the library writes the other side's access units.
    pub fn sink(&self) -> async_channel::Sender<VideoFrame> {
        self.frames.clone()
    }

    /// Where the library reads our encoded pictures. Empty until
    /// [`Reception::arm_camera`].
    pub fn source(&self) -> async_channel::Receiver<Vec<u8>> {
        self.camera.source()
    }

    /// The camera state, shared with the task that opens it.
    pub fn camera(&self) -> std::sync::Arc<crate::call_camera::Sending> {
        std::sync::Arc::clone(&self.camera)
    }

    /// Opens the default camera. False leaves the source silent; [`Reception::notice`]
    /// then says why.
    pub fn arm_camera(&self) -> bool {
        self.camera.arm()
    }

    /// Whether pictures are being encoded for the other side.
    pub fn sending(&self) -> bool {
        self.camera.sending()
    }

    /// Why the camera is off, when the person should be told.
    pub fn notice(&self) -> Option<String> {
        self.camera.notice()
    }

    /// Stops encoding. The source stays open and silent.
    pub fn halt_camera(&self) {
        self.camera.halt()
    }

    /// How to ask the other side for a keyframe, once the call exists.
    pub fn on_loss(&self, ask: impl Fn() + Send + Sync + 'static) {
        let _ = self.ask.0.set(Box::new(ask));
    }
}

impl Drop for Reception {
    fn drop(&mut self) {
        self.frames.close();
        self.camera.release();
    }
}

/// One access unit as the decode thread needs it.
struct Unit {
    data: Vec<u8>,
    /// It carries an IDR picture: the decoder can start from it.
    idr: bool,
    turns: u8,
    generation: u64,
}

impl Unit {
    fn of(frame: VideoFrame) -> Self {
        Self {
            idr: au_has_idr(&frame.data),
            turns: upright(frame.orientation),
            generation: frame.generation,
            data: frame.data,
        }
    }
}

/// The unit could not be decoded.
#[derive(Debug)]
struct Corrupt;

trait Decoder {
    /// Decodes one access unit, and copies the picture it completes out of
    /// the decoder when `keep` is set.
    fn decode(&mut self, data: &[u8], keep: bool) -> Result<Option<Picture>, Corrupt>;
}

struct H264(openh264::decoder::Decoder);

impl H264 {
    fn new() -> Result<Self, openh264::Error> {
        // As for video messages: flushing after every unit would stop a
        // stream with B-frames. Baseline streams, what calls send, have
        // none, and their pictures come out at once either way.
        openh264::decoder::Decoder::with_api_config(
            openh264::OpenH264API::from_source(),
            openh264::decoder::DecoderConfig::new()
                .flush_after_decode(openh264::decoder::Flush::NoFlush),
        )
        .map(Self)
    }
}

impl Decoder for H264 {
    fn decode(&mut self, data: &[u8], keep: bool) -> Result<Option<Picture>, Corrupt> {
        match self.0.decode(data) {
            Ok(Some(yuv)) if keep => Ok(Picture::of(&yuv)),
            Ok(_) => Ok(None),
            Err(_) => Err(Corrupt),
        }
    }
}

/// What became of one unit.
#[derive(Debug, Default)]
struct Decoded {
    /// The picture to hand over.
    picture: Option<Picture>,
    /// Ask the other side for a keyframe.
    ask: bool,
}

/// Whether the decoder can take the next unit, and for which media
/// generation.
#[derive(Default)]
struct Decode {
    synced: bool,
    generation: Option<u64>,
}

impl Decode {
    /// Decodes `unit` unless the decoder is waiting for an IDR. `full` means
    /// the queue was full, so the library may have dropped units before
    /// this one; `newest` that no other unit waits behind it, so its picture
    /// is worth handing over.
    fn unit(
        &mut self,
        decoder: &mut impl Decoder,
        unit: &Unit,
        full: bool,
        newest: bool,
    ) -> Decoded {
        if self.generation != Some(unit.generation) {
            // A new generation's units do not follow on from the last one's.
            self.generation = Some(unit.generation);
            self.synced = false;
        }
        if full {
            self.synced = false;
        }
        if !self.synced {
            if !unit.idr {
                return Decoded {
                    picture: None,
                    ask: true,
                };
            }
            self.synced = true;
        }
        match decoder.decode(&unit.data, newest) {
            Ok(picture) => Decoded {
                picture: picture.map(|picture| Picture {
                    turns: unit.turns,
                    ..picture
                }),
                ask: false,
            },
            Err(Corrupt) => {
                self.synced = false;
                Decoded {
                    picture: None,
                    ask: true,
                }
            }
        }
    }
}

/// The decode thread: runs until the library and [`Reception`] let go of
/// the queue.
fn run(frames: &async_channel::Receiver<VideoFrame>, feed: &Feed, ask: &Ask) {
    let mut decoder = match H264::new() {
        Ok(decoder) => decoder,
        Err(error) => {
            log::warn!("the call's video decoder could not start: {error}");
            return;
        }
    };
    let mut decode = Decode::default();
    let mut skipped = 0u64;
    while let Ok(frame) = frames.recv_blocking() {
        // Still full after this one came off: units that arrived meanwhile
        // were dropped by the library.
        let full = frames.len() + 1 >= QUEUE;
        let newest = frames.is_empty();
        let decoded = decode.unit(&mut decoder, &Unit::of(frame), full, newest);
        if decoded.ask {
            skipped += 1;
            ask.ask();
        }
        if let Some(picture) = decoded.picture {
            feed.put(picture);
        }
    }
    log::debug!(
        "call video stopped: {skipped} units skipped, {} pictures replaced unseen",
        feed.late()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const SAMPLE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/video/sample.mp4"
    );

    /// A decoder that decodes units whose first byte is not `BAD` into a
    /// small picture as wide as that byte.
    #[derive(Default)]
    struct Fake {
        decoded: Vec<u8>,
    }

    const BAD: u8 = 0xEE;

    impl Decoder for Fake {
        fn decode(&mut self, data: &[u8], keep: bool) -> Result<Option<Picture>, Corrupt> {
            if data[0] == BAD {
                return Err(Corrupt);
            }
            self.decoded.push(data[0]);
            Ok(keep.then(|| Picture::pattern(usize::from(data[0]), 2)))
        }
    }

    fn unit(tag: u8, idr: bool, generation: u64) -> Unit {
        Unit {
            data: vec![tag],
            idr,
            turns: 1,
            generation,
        }
    }

    /// Runs units through a fresh state, returning for each the width of the
    /// picture handed over (0 for none) and whether a keyframe was asked for.
    fn play(units: &[(Unit, bool, bool)]) -> (Vec<(usize, bool)>, Vec<u8>) {
        let mut decode = Decode::default();
        let mut decoder = Fake::default();
        let outcomes = units
            .iter()
            .map(|(unit, full, newest)| {
                let decoded = decode.unit(&mut decoder, unit, *full, *newest);
                (
                    decoded.picture.map_or(0, |picture| picture.size.0),
                    decoded.ask,
                )
            })
            .collect();
        (outcomes, decoder.decoded)
    }

    #[test]
    fn decoding_starts_at_an_idr_and_asks_for_one_before() {
        let (outcomes, decoded) = play(&[
            (unit(2, false, 0), false, true),
            (unit(3, false, 0), false, true),
            (unit(4, true, 0), false, true),
            (unit(5, false, 0), false, true),
        ]);
        assert_eq!(outcomes, [(0, true), (0, true), (4, false), (5, false)]);
        assert_eq!(
            decoded,
            [4, 5],
            "nothing before the IDR reaches the decoder"
        );
    }

    #[test]
    fn a_decode_error_asks_for_a_keyframe_and_waits_for_it() {
        let (outcomes, decoded) = play(&[
            (unit(4, true, 0), false, true),
            (unit(BAD, false, 0), false, true),
            (unit(6, false, 0), false, true),
            (unit(7, true, 0), false, true),
            (unit(8, false, 0), false, true),
        ]);
        assert_eq!(
            outcomes,
            [(4, false), (0, true), (0, true), (7, false), (8, false)]
        );
        assert_eq!(decoded, [4, 7, 8]);
    }

    #[test]
    fn a_full_queue_skips_to_the_next_idr() {
        let (outcomes, decoded) = play(&[
            (unit(4, true, 0), false, true),
            (unit(5, false, 0), true, false),
            (unit(6, false, 0), false, false),
            (unit(7, true, 0), false, true),
        ]);
        assert_eq!(outcomes, [(4, false), (0, true), (0, true), (7, false)]);
        assert_eq!(decoded, [4, 7]);
        // A full queue that starts at an IDR decodes it.
        let (outcomes, _) = play(&[
            (unit(4, true, 0), false, true),
            (unit(9, true, 0), true, true),
        ]);
        assert_eq!(outcomes, [(4, false), (9, false)]);
    }

    #[test]
    fn a_new_generation_waits_for_its_own_idr() {
        let (outcomes, _) = play(&[
            (unit(4, true, 0), false, true),
            (unit(5, false, 1), false, true),
            (unit(6, true, 1), false, true),
        ]);
        assert_eq!(outcomes, [(4, false), (0, true), (6, false)]);
    }

    #[test]
    fn only_the_newest_unit_is_copied_out() {
        let (outcomes, decoded) = play(&[
            (unit(4, true, 0), false, false),
            (unit(5, false, 0), false, false),
            (unit(6, false, 0), false, true),
        ]);
        assert_eq!(outcomes, [(0, false), (0, false), (6, false)]);
        assert_eq!(
            decoded,
            [4, 5, 6],
            "every unit is decoded for its references"
        );
    }

    #[test]
    fn pictures_keep_the_turns_of_their_unit() {
        let mut decode = Decode::default();
        let decoded = decode.unit(&mut Fake::default(), &unit(4, true, 0), false, true);
        assert_eq!(decoded.picture.map(|picture| picture.turns), Some(1));
        assert_eq!(upright(0), 0);
        assert_eq!(upright(1), 3, "one turn counter-clockwise");
        assert_eq!(upright(2), 2);
        assert_eq!(upright(3), 1);
        assert_eq!(upright(7), 1, "only the rotation bits count");
    }

    #[test]
    fn the_feed_keeps_only_the_newest_picture() {
        let feed = Feed::new(Waker::default());
        assert!(matches!(feed.take(), Newest::Off), "off until a picture");
        feed.put(Picture::pattern(4, 2));
        feed.put(Picture::pattern(6, 2));
        assert_eq!(feed.late(), 1);
        assert!(matches!(feed.take(), Newest::Picture(picture) if picture.size == (6, 2)));
        assert!(matches!(feed.take(), Newest::Unchanged));
        feed.put(Picture::pattern(8, 2));
        feed.stopped();
        assert!(matches!(feed.take(), Newest::Off));
        // The next picture turns the video back on.
        feed.put(Picture::pattern(10, 2));
        assert!(matches!(feed.take(), Newest::Picture(picture) if picture.size == (10, 2)));
        assert_eq!(feed.late(), 1, "a picture cleared by a stop was not late");
    }

    #[test]
    fn the_screen_shows_the_newest_picture_in_one_texture() {
        let ctx = egui::Context::default();
        let mut screen = Screen::default();
        let feed = Feed::new(Waker::default());
        assert!(screen.show(&ctx, Some(&feed)).is_none());
        feed.put(Picture {
            turns: 1,
            ..Picture::pattern(16, 8)
        });
        let first = screen.show(&ctx, Some(&feed)).expect("a picture");
        assert_eq!(first.size(), [8, 16], "turned upright");
        // Nothing new: the same picture stays.
        let same = screen.show(&ctx, Some(&feed)).expect("still showing");
        assert_eq!(same.id(), first.id());
        feed.put(Picture::pattern(2000, 1000));
        let next = screen.show(&ctx, Some(&feed)).expect("a picture");
        assert_eq!(next.id(), first.id(), "uploaded into the same texture");
        assert_eq!(next.size(), [960, 480], "scaled to the screen's limit");
        feed.stopped();
        assert!(screen.show(&ctx, Some(&feed)).is_none());
        // Another call's feed, or none, starts afresh.
        feed.put(Picture::pattern(4, 2));
        let other = Feed::new(Waker::default());
        assert!(screen.show(&ctx, Some(&other)).is_none());
        assert!(screen.show(&ctx, None).is_none());
    }

    #[test]
    fn short_planes_are_refused() {
        let mut picture = Picture::pattern(8, 4);
        assert!(picture.whole());
        picture.u.truncate(3);
        assert!(!picture.whole());
    }

    /// The sample's video track as Annex B access units, parameter sets
    /// first, the way the library hands a call's video over.
    fn sample_units() -> Vec<Vec<u8>> {
        let file = std::fs::File::open(SAMPLE).unwrap();
        let size = file.metadata().unwrap().len();
        let mut mp4 = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).unwrap();
        let track = mp4
            .tracks()
            .values()
            .find(|track| track.track_type().ok() == Some(mp4::TrackType::Video))
            .unwrap();
        let (id, count) = (track.track_id(), track.sample_count());
        let mut parameters = Vec::new();
        crate::animation::push_annex_b(&mut parameters, track.sequence_parameter_set().unwrap());
        crate::animation::push_annex_b(&mut parameters, track.picture_parameter_set().unwrap());
        (1..=count)
            .map(|sample| {
                let sample = mp4.read_sample(id, sample).unwrap().unwrap();
                let mut unit = if sample.is_sync {
                    parameters.clone()
                } else {
                    Vec::new()
                };
                crate::animation::avcc_to_annex_b(&mut unit, &sample.bytes);
                unit
            })
            .collect()
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn received_video_is_decoded_off_the_interface_thread_and_shown() {
        let units = sample_units();
        assert!(au_has_idr(&units[0]) && !au_has_idr(&units[1]));
        let reception = Reception::start(Waker::default()).unwrap();
        let asked = Arc::new(AtomicUsize::new(0));
        {
            let asked = Arc::clone(&asked);
            reception.on_loss(move || {
                asked.fetch_add(1, Ordering::SeqCst);
            });
        }
        let sink = reception.sink();
        // A unit before the first IDR is skipped, with a keyframe asked for.
        sink.send_blocking(VideoFrame::new(units[1].clone()))
            .unwrap();
        wait_until("the keyframe request", || asked.load(Ordering::SeqCst) == 1);
        // Then the stream from its IDR, at a pace the decoder keeps up with.
        for unit in &units {
            sink.send_blocking(VideoFrame::new(unit.clone())).unwrap();
            wait_until("the decoder", || sink.is_empty());
        }
        let feed = reception.feed();
        let ctx = egui::Context::default();
        let mut screen = Screen::default();
        let mut shown = None;
        wait_until("a picture", || {
            shown = screen.show(&ctx, Some(&feed));
            shown.is_some()
        });
        assert_eq!(shown.unwrap().size(), [320, 180]);
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "a clean stream asks nothing"
        );
        // Dropping the reception stops the thread, which lets go of the queue.
        drop(reception);
        wait_until("the decode thread to stop", || sink.receiver_count() == 0);
    }
}
