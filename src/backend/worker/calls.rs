//! Calls: one at a time per account, voice or video.
//!
//! The library rings, answers, places and ends calls (`Client::voip`). This
//! module decides what each of its signals means for the one call an account
//! may have, and tells the interface in the app's own types. [`Calls`] is
//! that decision, kept away from the network and the sound devices so it can
//! be tested; the `Worker` methods below carry out the [`Step`]s it returns.
//!
//! Setting a call up takes seconds (device lookups, the relay, opening the
//! microphone), so it runs in a task that reports back through
//! `RuntimeEvent::Call`. Every report names the call it was for. A call that
//! ended in the meantime (the caller gave up while we answered, the phone
//! answered first, or we hung up while dialling) has the handle its setup
//! made terminated and its audio closed, instead of coming back to life.
//!
//! A video call, placed or answered, sends the chosen camera on Linux. If it
//! cannot be opened, our video is turned off once the relay is up, and only
//! the other side is shown. Turning it off any earlier drops their picture
//! on a call we placed. On Windows and macOS the camera is not sent yet.
//!
//! A voice call switches to video from either side. Turning our camera on
//! asks the other side, and the call stays voice unless they accept within
//! WhatsApp's own five seconds. When they ask, their picture is accepted at
//! once and our camera stays off until the person turns it on: the camera
//! never opens without them.
//!
//! A second offer while a call rings or runs is declined at once. The
//! library at this revision sends a reject without a reason, so the caller
//! sees a decline rather than "busy".
//!
//! WhatsApp's call ids, the callers' ids and the library's errors never
//! reach the log: calls are logged by the number this worker gave them.

use super::*;
use crate::audio::{CallAudio, CallEndpoints};
use crate::call_video::Reception;
use crate::model::{CallEndReason, CallId, CallMedia, CallNote, CallPhase, CallRecord};
use whatsapp_rust::CallError;
use whatsapp_rust::types::call::{CallAction, CallEndedElsewhere, IncomingCall, MissedCall};
use whatsapp_rust::voip::audio::WA_SAMPLE_RATE;
use whatsapp_rust::voip::{
    CallEvent, CallHandle, CallTermination, KeyframeUrgency, VIDEO_UPGRADE_TIMEOUT, VideoState,
    VideoUpgradeToken,
};
use whatsapp_rust::wacore::stanza::call::{REJECT_REASON_BUSY, REJECT_REASON_ENC};
use whatsapp_rust::wacore::voip_control::MediaCloseReason;

/// How many ended calls' WhatsApp ids are remembered, so a repeated or late
/// signal for one of them neither rings nor ends anything again.
const FINISHED: usize = 32;

/// How long a hang-up at shutdown may take to reach the other side.
const STOP_TERMINATE: Duration = Duration::from_secs(3);

/// A call the library is running: its handle, the microphone and speaker
/// feeding it, and the other side's video in a video call. Dropping `audio`
/// closes both devices, and dropping `video` stops its decoder.
pub(super) struct Live {
    handle: CallHandle,
    audio: CallAudio,
    video: Option<Reception>,
}

/// A setup that produced no call.
pub(super) struct Failure {
    reason: CallEndReason,
    /// Why, in words for the interface, when it is something the person can
    /// fix (no microphone).
    notice: Option<String>,
}

/// What a call's task reports back.
pub(super) enum Report {
    /// Answering or placing finished.
    Started {
        id: CallId,
        /// Boxed: a running call is large next to the other reports.
        result: Result<Box<Live>, Failure>,
    },
    /// The library ended the call, or its media failed for good.
    Over { id: CallId, failed: bool },
    /// The microphone's state after a mute or unmute.
    Muted { id: CallId, muted: bool },
    /// Whether our camera is sending, after it was opened or the person
    /// turned it. `notice` is set when turning it on failed.
    Camera {
        id: CallId,
        sending: bool,
        notice: Option<String>,
    },
    /// The other side's `<video>` signalling. `token` answers a request to
    /// switch to video.
    PeerVideo {
        id: CallId,
        state: VideoState,
        token: Option<VideoUpgradeToken>,
    },
    /// A switch between voice and video settled: the call now carries
    /// `video`. `notice` says why a switch the person asked for did not
    /// happen.
    Switched {
        id: CallId,
        video: bool,
        notice: Option<String>,
    },
}

/// What the other side's signalling said about a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Signal {
    Accepted,
    /// `device` means only one of their devices declined, because it is in
    /// another call or could not decrypt the offer; the rest ring on.
    Rejected {
        device: bool,
    },
    Terminated,
}

/// One thing for the worker to do. `H` is a running call's handle.
pub(super) enum Step<H> {
    Emit(Event),
    /// Decline an offer.
    Decline(Box<IncomingCall>),
    /// Open the audio and answer the offer.
    Answer {
        id: CallId,
        offer: Box<IncomingCall>,
    },
    /// Open the audio and call this chat. `video` sends our camera too.
    Place {
        id: CallId,
        chat: ChatId,
        video: bool,
    },
    /// Follow the running call until the library ends it.
    Watch(CallId),
    /// The line is open: stop the ringback and play the connect tone.
    LineOpen(CallId),
    /// Mute or unmute the running call.
    Mute {
        id: CallId,
        muted: bool,
    },
    /// End the call, telling the other side, and close its audio.
    Terminate(H),
    /// The other side or the library already ended the call: tear down
    /// what is left here and close its audio.
    Release(H),
}

enum Origin {
    /// Rang here. The offer is kept to answer or decline it.
    Incoming(Box<IncomingCall>),
    /// Placed here. `busy` once one of the callee's devices declined as busy.
    Outgoing { busy: bool },
}

struct Call<H> {
    id: CallId,
    chat: ChatId,
    origin: Origin,
    /// WhatsApp's id for the call. A placed call learns it from its setup.
    protocol: Option<String>,
    phase: CallPhase,
    live: Option<H>,
    /// What the person asked for. Muting before an answer is local only, so
    /// it is applied again once the call connects.
    muted: bool,
    /// Unix seconds, when the call began, so its chat line stays put.
    started: i64,
    /// Unix milliseconds, once the line is open, for the duration.
    connected_at: Option<i64>,
    /// Another of our devices declined it, rather than answering it.
    declined_elsewhere: bool,
    /// Placed or offered with video.
    video: bool,
    /// Carries video now: placed or offered with it, or switched since.
    carries_video: bool,
}

impl<H> Call<H> {
    fn incoming(&self) -> bool {
        matches!(self.origin, Origin::Incoming(_))
    }
}

/// The account's one call and what is remembered of ended ones.
pub(super) struct Calls<H = Live> {
    next: u64,
    current: Option<Call<H>>,
    /// WhatsApp ids of ended or declined calls, oldest first.
    finished: VecDeque<String>,
    /// Chat lines to file for calls that just changed. The worker drains them.
    logs: Vec<crate::model::CallNote>,
}

impl<H> Default for Calls<H> {
    fn default() -> Self {
        Self {
            next: 0,
            current: None,
            finished: VecDeque::new(),
            logs: Vec::new(),
        }
    }
}

impl Calls<Live> {
    /// The running call's received video, when it has any.
    fn reception(&mut self, id: CallId) -> Option<&mut Reception> {
        self.current
            .as_mut()
            .filter(|call| call.id == id)
            .and_then(|call| call.live.as_mut())
            .and_then(|live| live.video.as_mut())
    }

    /// Gives a call that switched to video a place to receive it.
    fn attach_reception(&mut self, id: CallId, video: Reception) -> Option<&mut Reception> {
        let live = self
            .current
            .as_mut()
            .filter(|call| call.id == id)
            .and_then(|call| call.live.as_mut())?;
        Some(live.video.insert(video))
    }

    /// The running call's speaker, so the line can be marked open.
    fn speaker(&mut self, id: CallId) -> Option<&mut CallAudio> {
        self.current
            .as_mut()
            .filter(|call| call.id == id)
            .and_then(|call| call.live.as_mut())
            .map(|live| &mut live.audio)
    }
}

impl<H> Calls<H> {
    /// Whether a call is ringing, being set up, or running.
    pub(super) fn busy(&self) -> bool {
        self.current.is_some()
    }

    /// The running call's handle.
    pub(super) fn live(&self, id: CallId) -> Option<&H> {
        self.current
            .as_ref()
            .filter(|call| call.id == id)
            .and_then(|call| call.live.as_ref())
    }

    /// Remembers an offer that is left to the other devices, so its later
    /// signals are not mistaken for anything here.
    pub(super) fn ignore(&mut self, protocol: &str) {
        self.remember(protocol.to_owned());
    }

    fn allocate(&mut self) -> CallId {
        self.next += 1;
        CallId(self.next)
    }

    fn remember(&mut self, protocol: String) {
        if self.finished.contains(&protocol) {
            return;
        }
        if self.finished.len() == FINISHED {
            self.finished.pop_front();
        }
        self.finished.push_back(protocol);
    }

    /// Whether the running call carries video, from the start or since a
    /// switch.
    pub(super) fn has_video(&self, id: CallId) -> bool {
        self.current
            .as_ref()
            .is_some_and(|call| call.id == id && call.carries_video)
    }

    /// The call switched between voice and video. The chat line keeps what
    /// the call was placed as.
    pub(super) fn switch_media(&mut self, id: CallId, video: bool, now: i64) -> Vec<Step<H>> {
        let Some(call) = self.current_mut(id) else {
            return Vec::new();
        };
        if call.carries_video == video || call.live.is_none() {
            return Vec::new();
        }
        call.carries_video = video;
        log::info!(
            "call {}: switched to {}",
            id.0,
            if video { "video" } else { "voice" }
        );
        vec![Self::state(call, now)]
    }

    /// The call on this account, when there is one.
    pub(super) fn current_id(&self) -> Option<CallId> {
        self.current.as_ref().map(|call| call.id)
    }

    /// Chat lines written since the last drain.
    pub(super) fn take_logs(&mut self) -> Vec<CallNote> {
        std::mem::take(&mut self.logs)
    }

    fn remember_log(&mut self, record: CallRecord) {
        let note = self.current.as_ref().map(|call| note_of(call, record));
        if let Some(note) = note {
            self.upsert_log(note);
        }
    }

    fn upsert_log(&mut self, note: CallNote) {
        if let Some(existing) = self
            .logs
            .iter_mut()
            .find(|item| item.protocol == note.protocol)
        {
            if existing.record.finished() && !note.record.finished() {
                return;
            }
            *existing = note;
        } else {
            self.logs.push(note);
        }
    }

    /// Whether WhatsApp's id names the current call or an ended one.
    fn known(&self, protocol: &str) -> bool {
        self.current
            .as_ref()
            .is_some_and(|call| call.protocol.as_deref() == Some(protocol))
            || self.finished.iter().any(|known| known == protocol)
    }

    fn current_mut(&mut self, id: CallId) -> Option<&mut Call<H>> {
        self.current.as_mut().filter(|call| call.id == id)
    }

    fn by_protocol(&mut self, protocol: &str) -> Option<&mut Call<H>> {
        self.current
            .as_mut()
            .filter(|call| call.protocol.as_deref() == Some(protocol))
    }

    /// The call's state for the interface. A connected call keeps the moment
    /// it connected, so its timer does not restart when the media changes.
    fn state(call: &Call<H>, now: i64) -> Step<H> {
        let since = match call.phase {
            CallPhase::Connected => call.connected_at.unwrap_or(now),
            _ => now,
        };
        Step::Emit(Event::CallState {
            call: call.id,
            chat: call.chat.clone(),
            phase: call.phase,
            since,
            media: if call.carries_video {
                CallMedia::Video
            } else {
                CallMedia::Voice
            },
        })
    }

    /// Ends the current call, announcing why, and hands back what is left
    /// of it.
    fn take(&mut self, reason: CallEndReason, now: i64, steps: &mut Vec<Step<H>>) -> Call<H> {
        let mut call = self.current.take().expect("a current call");
        if let Some(protocol) = call.protocol.clone() {
            self.remember(protocol);
        }
        log::info!("call {}: ended ({reason:?})", call.id.0);
        call.phase = CallPhase::Ended;
        if let Some(record) = end_record(&call, reason, now) {
            let note = note_of(&call, record);
            self.upsert_log(note);
        }
        steps.push(Self::state(&call, now));
        steps.push(Step::Emit(Event::CallEnded {
            call: call.id,
            chat: call.chat.clone(),
            reason,
        }));
        call
    }

    /// Ends the current call. Its handle, if any, is terminated when the
    /// other side may not know yet, and released otherwise.
    fn finish(&mut self, reason: CallEndReason, terminate: bool, now: i64) -> Vec<Step<H>> {
        let mut steps = Vec::new();
        let call = self.take(reason, now, &mut steps);
        if let Some(live) = call.live {
            steps.push(if terminate {
                Step::Terminate(live)
            } else {
                Step::Release(live)
            });
        }
        steps
    }

    /// An offer rang. It rings here unless a call is already going, when it
    /// is declined.
    pub(super) fn offer(
        &mut self,
        offer: Box<IncomingCall>,
        chat: ChatId,
        name: String,
        media: CallMedia,
        now: i64,
    ) -> Vec<Step<H>> {
        let protocol = offer.action.call_id().to_owned();
        if self.known(&protocol) {
            return Vec::new();
        }
        if self.busy() {
            log::info!("declined a call that rang during another");
            self.remember(protocol);
            return vec![Step::Decline(offer)];
        }
        let id = self.allocate();
        log::info!("call {}: ringing here", id.0);
        let call = Call {
            id,
            chat: chat.clone(),
            origin: Origin::Incoming(offer),
            protocol: Some(protocol),
            phase: CallPhase::Ringing,
            live: None,
            muted: false,
            started: now / 1000,
            connected_at: None,
            declined_elsewhere: false,
            video: media == CallMedia::Video,
            carries_video: media == CallMedia::Video,
        };
        let steps = vec![
            Step::Emit(Event::CallIncoming {
                call: id,
                chat,
                name,
                media,
            }),
            Self::state(&call, now),
        ];
        self.current = Some(call);
        self.remember_log(CallRecord::Incoming);
        steps
    }

    /// The person calls a chat. `video` sends our camera as well as our voice.
    pub(super) fn place(&mut self, chat: ChatId, video: bool, now: i64) -> Vec<Step<H>> {
        if self.busy() {
            return vec![Step::Emit(Event::Error(
                "Another call is already in progress.".to_owned(),
            ))];
        }
        let id = self.allocate();
        log::info!("call {}: calling", id.0);
        let call = Call {
            id,
            chat: chat.clone(),
            origin: Origin::Outgoing { busy: false },
            protocol: None,
            phase: CallPhase::Connecting,
            live: None,
            muted: false,
            started: now / 1000,
            connected_at: None,
            declined_elsewhere: false,
            video,
            carries_video: video,
        };
        let steps = vec![Self::state(&call, now), Step::Place { id, chat, video }];
        self.current = Some(call);
        steps
    }

    /// The person answers the call ringing here.
    pub(super) fn accept(&mut self, id: CallId, now: i64) -> Vec<Step<H>> {
        let Some(call) = self.current_mut(id) else {
            return Vec::new();
        };
        let Origin::Incoming(offer) = &call.origin else {
            return Vec::new();
        };
        if call.phase != CallPhase::Ringing {
            return Vec::new();
        }
        let offer = offer.clone();
        call.phase = CallPhase::Connecting;
        log::info!("call {}: answering", id.0);
        vec![Self::state(call, now), Step::Answer { id, offer }]
    }

    /// The person declines or hangs up. A call ringing here is declined; a
    /// call still being set up ends here, and its setup's report terminates
    /// whatever it made.
    pub(super) fn hang_up(&mut self, id: CallId, now: i64) -> Vec<Step<H>> {
        let Some(call) = self.current_mut(id) else {
            return Vec::new();
        };
        if call.incoming() && call.phase == CallPhase::Ringing {
            let mut steps = Vec::new();
            if let Origin::Incoming(offer) =
                self.take(CallEndReason::Rejected, now, &mut steps).origin
            {
                steps.push(Step::Decline(offer));
            }
            return steps;
        }
        self.finish(CallEndReason::HungUp, true, now)
    }

    /// Answering or placing finished. `Ok` carries the handle and
    /// WhatsApp's id for the call.
    pub(super) fn started(
        &mut self,
        id: CallId,
        result: Result<(H, String), CallEndReason>,
        now: i64,
    ) -> Vec<Step<H>> {
        let waiting = self
            .current_mut(id)
            .is_some_and(|call| call.phase == CallPhase::Connecting && call.live.is_none());
        if !waiting {
            // The call ended while it was being set up.
            return match result {
                Ok((live, _)) => vec![Step::Terminate(live)],
                Err(_) => Vec::new(),
            };
        }
        let (live, protocol) = match result {
            Ok(started) => started,
            Err(reason) => return self.finish(reason, false, now),
        };
        let call = self.current_mut(id).expect("checked above");
        call.live = Some(live);
        call.protocol = Some(protocol);
        call.phase = if call.incoming() {
            CallPhase::Connected
        } else {
            CallPhase::Ringing
        };
        if call.phase == CallPhase::Connected {
            call.connected_at = Some(now);
        }
        let record = if call.phase == CallPhase::Connected {
            CallRecord::Ongoing { since: now }
        } else {
            CallRecord::Outgoing
        };
        let note = note_of(call, record);
        let phase = call.phase;
        let muted = call.muted;
        self.upsert_log(note);
        log::info!("call {}: {:?}", id.0, phase);
        let call = self.current_mut(id).expect("checked above");
        let mut steps = vec![Self::state(call, now)];
        // Answering opens the line now. A placed call is still ringing there.
        if phase == CallPhase::Connected {
            steps.push(Step::LineOpen(id));
        }
        steps.push(Step::Watch(id));
        if muted {
            steps.push(Step::Mute { id, muted: true });
        }
        steps
    }

    /// The other side's signalling for the call WhatsApp calls `protocol`.
    pub(super) fn signal(&mut self, protocol: &str, signal: Signal, now: i64) -> Vec<Step<H>> {
        let Some(call) = self.by_protocol(protocol) else {
            return Vec::new();
        };
        let outgoing = !call.incoming();
        match signal {
            Signal::Accepted if outgoing && call.phase == CallPhase::Ringing => {
                call.phase = CallPhase::Connected;
                call.connected_at = Some(now);
                let note = note_of(call, CallRecord::Ongoing { since: now });
                let id = call.id;
                let muted = call.muted;
                self.upsert_log(note);
                log::info!("call {}: Connected", id.0);
                let call = self
                    .by_protocol(protocol)
                    .expect("the call is still current");
                let mut steps = vec![Self::state(call, now), Step::LineOpen(id)];
                if muted {
                    steps.push(Step::Mute { id, muted: true });
                }
                steps
            }
            Signal::Rejected { device: true } => {
                if let Origin::Outgoing { busy } = &mut call.origin {
                    *busy = true;
                }
                Vec::new()
            }
            Signal::Rejected { device: false } if outgoing && call.phase == CallPhase::Ringing => {
                self.finish(CallEndReason::Rejected, false, now)
            }
            Signal::Terminated => {
                let reason = match (&call.origin, call.phase) {
                    (_, CallPhase::Connected) => CallEndReason::HungUp,
                    (Origin::Outgoing { busy: true }, _) => CallEndReason::Busy,
                    _ => CallEndReason::Missed,
                };
                self.finish(reason, false, now)
            }
            _ => Vec::new(),
        }
    }

    /// A call was missed: the caller gave up on the call ringing here, or
    /// an offer arrived while ZapFast was offline and never rang.
    pub(super) fn missed(&mut self, protocol: &str, chat: ChatId, now: i64) -> Vec<Step<H>> {
        if let Some(call) = self.by_protocol(protocol) {
            if call.incoming() && call.phase != CallPhase::Connected {
                return self.finish(CallEndReason::Missed, false, now);
            }
            return Vec::new();
        }
        if self.known(protocol) {
            return Vec::new();
        }
        let id = self.allocate();
        self.remember(protocol.to_owned());
        self.logs.push(CallNote {
            chat: chat.clone(),
            protocol: protocol.to_owned(),
            video: false,
            outgoing: false,
            record: CallRecord::Missed,
            started: now / 1000,
        });
        log::info!("call {}: missed while offline", id.0);
        vec![Step::Emit(Event::CallEnded {
            call: id,
            chat,
            reason: CallEndReason::Missed,
        })]
    }

    /// Another of this account's devices answered or declined the call
    /// ringing here.
    pub(super) fn elsewhere(&mut self, protocol: &str, declined: bool, now: i64) -> Vec<Step<H>> {
        match self.by_protocol(protocol) {
            Some(call) if call.incoming() && call.phase != CallPhase::Connected => {
                call.declined_elsewhere = declined;
                self.finish(CallEndReason::EndedElsewhere, false, now)
            }
            _ => Vec::new(),
        }
    }

    /// The library ended the running call, or its media failed for good.
    pub(super) fn over(&mut self, id: CallId, failed: bool, now: i64) -> Vec<Step<H>> {
        let Some(call) = self.current_mut(id).filter(|call| call.live.is_some()) else {
            return Vec::new();
        };
        let reason = match (&call.origin, call.phase) {
            _ if failed => CallEndReason::Failed,
            (Origin::Outgoing { busy: true }, CallPhase::Ringing) => CallEndReason::Busy,
            (Origin::Outgoing { .. }, CallPhase::Ringing) => CallEndReason::Missed,
            _ => CallEndReason::HungUp,
        };
        // A failed call may still be up for the other side.
        self.finish(reason, failed, now)
    }

    /// The person mutes or unmutes.
    pub(super) fn set_muted(&mut self, id: CallId, muted: bool) -> Vec<Step<H>> {
        let Some(call) = self.current_mut(id) else {
            return Vec::new();
        };
        call.muted = muted;
        if call.live.is_some() {
            vec![Step::Mute { id, muted }]
        } else {
            // Applied once the call is running.
            vec![Step::Emit(Event::CallMuted { call: id, muted })]
        }
    }

    /// The microphone's state after a mute or unmute.
    pub(super) fn muted(&mut self, id: CallId, muted: bool) -> Vec<Step<H>> {
        let Some(call) = self.current_mut(id) else {
            return Vec::new();
        };
        call.muted = muted;
        vec![Step::Emit(Event::CallMuted { call: id, muted })]
    }

    /// The connection is going away. A call ringing here is left to the
    /// other devices; anything further along is cut.
    pub(super) fn shutdown(&mut self, now: i64) -> Vec<Step<H>> {
        let Some(call) = &self.current else {
            return Vec::new();
        };
        let reason = if call.incoming() && call.phase == CallPhase::Ringing {
            CallEndReason::Missed
        } else {
            CallEndReason::Failed
        };
        self.finish(reason, true, now)
    }
}

fn note_of<H>(call: &Call<H>, record: CallRecord) -> CallNote {
    CallNote {
        chat: call.chat.clone(),
        protocol: call
            .protocol
            .clone()
            .unwrap_or_else(|| format!("local-{}", call.id.0)),
        video: call.video,
        outgoing: !call.incoming(),
        record,
        started: call.started,
    }
}

fn end_record<H>(call: &Call<H>, reason: CallEndReason, now: i64) -> Option<CallRecord> {
    Some(match reason {
        CallEndReason::HungUp => {
            if let Some(connected) = call.connected_at {
                CallRecord::Answered {
                    seconds: ((now - connected) / 1000).max(0) as u32,
                }
            } else if call.incoming() {
                CallRecord::Declined
            } else {
                CallRecord::Cancelled
            }
        }
        CallEndReason::Rejected => CallRecord::Declined,
        CallEndReason::Missed => {
            if call.incoming() {
                CallRecord::Missed
            } else {
                CallRecord::Unanswered
            }
        }
        CallEndReason::EndedElsewhere if call.declined_elsewhere => CallRecord::Declined,
        CallEndReason::EndedElsewhere => CallRecord::Elsewhere,
        CallEndReason::Busy => CallRecord::Unanswered,
        CallEndReason::Failed => CallRecord::Failed,
    })
}

fn now() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// A failure's kind, without the identifiers its message may carry.
fn error_label(error: &CallError) -> &'static str {
    match error {
        CallError::Send(_) => "send failed",
        CallError::EmptyCallId => "empty call id",
        CallError::NotAnOffer => "not an offer",
        CallError::MissingAudio => "missing audio",
        CallError::AudioFormatNotOffered(_) => "audio format not offered",
        CallError::EncodedAudioCodecNotNegotiated { .. } => "codec not negotiated",
        CallError::VideoNotOffered => "video not offered",
        CallError::CallEndedDuringSetup => "ended during setup",
        CallError::Decrypt(_) => "call key not decrypted",
        CallError::Setup(_) => "setup failed",
        CallError::Connect(_) => "relay connect failed",
        CallError::Media(_) => "media offer error",
        CallError::NoDevices => "no devices",
        CallError::MissingDeviceIdentity => "missing device identity",
        CallError::ResponseTimeout => "call service timed out",
        _ => "other",
    }
}

fn setup_failure(error: &CallError) -> Failure {
    log::warn!("a call could not be set up: {}", error_label(error));
    Failure {
        // The caller hung up before the answer went through.
        reason: if matches!(error, CallError::CallEndedDuringSetup) {
            CallEndReason::Missed
        } else {
            CallEndReason::Failed
        },
        notice: None,
    }
}

/// Opens the microphone and speaker off the runtime's threads.
async fn open_audio(
    ringback: bool,
    devices: crate::audio::CallDevices,
) -> Result<(CallAudio, CallEndpoints), Failure> {
    let opened = tokio::task::spawn_blocking(move || {
        CallAudio::start(
            WA_SAMPLE_RATE,
            ringback,
            devices.microphone,
            devices.speaker,
        )
    })
    .await
    .unwrap_or_else(|_| Err("The call's audio could not start.".to_owned()));
    opened.map_err(|notice| {
        log::warn!("a call's audio could not open: {notice}");
        Failure {
            reason: CallEndReason::Failed,
            notice: Some(notice),
        }
    })
}

/// Whether a call event means the media cannot carry the call any more.
fn ends_badly(event: &CallEvent) -> bool {
    match event {
        CallEvent::RelayAllocateFailed(_)
        | CallEvent::RelayAllocateTimedOut
        | CallEvent::MediaSetupFailed(_)
        | CallEvent::RelayReconnectTimedOut => true,
        // A local close is a hang-up, from either side.
        CallEvent::Closed(reason) => !matches!(reason, MediaCloseReason::Local),
        _ => false,
    }
}

/// Whether the other side's video stops at this state of theirs.
fn video_stops(state: VideoState) -> bool {
    matches!(
        state,
        VideoState::Disabled | VideoState::Paused | VideoState::Stopped
    )
}

/// Whether the other side has answered our request to switch to video:
/// `Some(true)` once their video is on, `Some(false)` once the library
/// withdrew ours (they declined, or WhatsApp's timer ran out).
fn upgrade_verdict(ours: VideoState, theirs: VideoState) -> Option<bool> {
    if matches!(
        theirs,
        VideoState::Enabled | VideoState::UpgradeAccept | VideoState::Paused
    ) {
        Some(true)
    } else if ours.is_inactive_for_call_mode() {
        Some(false)
    } else {
        None
    }
}

/// Waits for the answer to our request to switch to video, a second past
/// WhatsApp's own timer, after which an unanswered request is over.
async fn upgrade_answer(handle: &CallHandle) -> bool {
    let deadline = tokio::time::Instant::now() + VIDEO_UPGRADE_TIMEOUT + Duration::from_secs(1);
    loop {
        let Some((ours, theirs)) = handle.video_states() else {
            return false;
        };
        if let Some(accepted) = upgrade_verdict(ours, theirs) {
            return accepted;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Asks for keyframes on the decoder's behalf. A camera that did not open is
/// turned off later, from [`watch`], once the relay exists: doing it here
/// removes the video the outgoing relay has not attached yet, and the call
/// then has no picture to receive.
fn finish_video(handle: &CallHandle, video: &Reception) {
    let asking = handle.clone();
    video.on_loss(move || asking.request_peer_keyframe(KeyframeUrgency::Coalesced));
}

/// Follows a running call until it ends, and says whether it failed. This
/// is the call's one reader of its event queue, so the other side's video
/// signalling goes to the worker from here as [`Report::PeerVideo`].
/// `silence_camera` turns our own video off once the relay has allocated,
/// which is the first moment that still leaves their picture in place.
async fn watch(
    id: CallId,
    handle: &CallHandle,
    sender: &mpsc::UnboundedSender<RuntimeEvent>,
    mut silence_camera: bool,
) -> bool {
    let events = handle.events();
    let ended = handle.wait_ended();
    tokio::pin!(ended);
    let mut stats_at = tokio::time::interval(std::time::Duration::from_secs(5));
    // The first tick is ready immediately; the counts are all zero then.
    stats_at.tick().await;
    loop {
        tokio::select! {
            () = &mut ended => {
                log_call_media(handle);
                return false;
            }
            _ = stats_at.tick() => log_call_media(handle),
            event = events.recv() => match event {
                Ok(event) => {
                    if silence_camera && matches!(event, CallEvent::RelayAllocated) {
                        silence_camera = false;
                        if let Err(error) = handle.stop_video().await {
                            log::info!(
                                "call media: our video could not be turned off: {}",
                                error_label(&error)
                            );
                        }
                    }
                    note_media_event(&event);
                    if ends_badly(&event) {
                        log_call_media(handle);
                        return true;
                    }
                    if let CallEvent::PeerVideoStateChanged {
                        state,
                        upgrade_token,
                        ..
                    } = event
                    {
                        log::info!("call {}: their video is {state:?}", id.0);
                        let _ = sender.send(RuntimeEvent::Call(Report::PeerVideo {
                            id,
                            state,
                            token: upgrade_token,
                        }));
                    }
                }
                Err(_) => {
                    ended.await;
                    log_call_media(handle);
                    return false;
                }
            },
        }
    }
}

/// Packets and frames the library counted. A call with no sound and no
/// picture is either nothing arriving (`packets in` stays 0) or arriving
/// and not played (`audio frames` climbs while the speaker reports none).
fn log_call_media(handle: &CallHandle) {
    let stats = handle.media_stats();
    log::info!(
        "call media: {} packets in, {} audio frames ({} opus, {} with no decoder), {} outbound with no encoder, {} audio dropped, {} video frames dropped, {} decrypt failures, {} unexpected, {} unclassified, {} dropped before decode",
        stats.rtp_received,
        stats.audio_produced(),
        stats.foreign_frames_decoded,
        stats.audio_frames_without_decoder,
        stats.outbound_frames_without_encoder,
        stats.audio_sink_dropped,
        stats.video_sink_dropped,
        stats
            .srtp_unprotect_failed
            .saturating_add(stats.sframe_decrypt_failed),
        stats.rtp_payload_type_unexpected,
        stats.relay_packet_unclassified,
        stats.inbound_pipe_dropped,
    );
}

/// Why a connected call can stay quiet, in the log the next call leaves behind.
fn note_media_event(event: &CallEvent) {
    match event {
        CallEvent::AudioSilent {
            silent_for_ms,
            rtp_received,
            frames_produced,
            dominant_reason,
        } => log::info!(
            "call media: silent for {silent_for_ms:?}, {rtp_received} packets, {frames_produced} frames, {dominant_reason:?}"
        ),
        CallEvent::AudioReceptionStalled { silent_for_ms } => {
            log::info!("call media: no audio packets for {silent_for_ms:?}");
        }
        CallEvent::AudioFormatMismatch {
            expected_rate,
            received_rates,
        } => log::info!(
            "call media: the other side's audio rate {received_rates:?} is not {expected_rate}"
        ),
        CallEvent::ForeignAudio(_) => {
            use std::sync::atomic::{AtomicBool, Ordering};
            static REPORTED: AtomicBool = AtomicBool::new(false);
            if !REPORTED.swap(true, Ordering::Relaxed) {
                log::info!("call media: audio arrived in a codec this call cannot play");
            }
        }
        CallEvent::AudioCodecSwitched { from, to, .. } => {
            log::info!("call media: audio codec changed from {from:?} to {to:?}");
        }
        CallEvent::RelayAllocated => log::info!("call media: relay allocated"),
        _ => {}
    }
}

impl Worker {
    /// Every `<call>` stanza the library passes on: offers that ring, and the
    /// other side's answer, decline, or hang-up.
    pub(super) fn incoming_call(&mut self, call: &IncomingCall) {
        let signal = match &call.action {
            CallAction::Offer {
                call_creator,
                caller_pn,
                is_video,
                group_jid,
                ..
            } => {
                let group = group_jid.is_some() || call.group.is_some();
                let caller = match caller_pn {
                    Some(pn) if call_creator.is_lid() => pn,
                    _ => call_creator,
                };
                self.call_offered(call, caller, *is_video, group);
                return;
            }
            CallAction::Accept { .. } => Signal::Accepted,
            CallAction::Reject { reason, .. } => Signal::Rejected {
                device: matches!(
                    reason.as_deref(),
                    Some(REJECT_REASON_BUSY | REJECT_REASON_ENC)
                ),
            },
            CallAction::Terminate { .. } => Signal::Terminated,
            _ => return,
        };
        let steps = self.calls.signal(call.action.call_id(), signal, now());
        self.run_call_steps(steps);
    }

    fn call_offered(&mut self, call: &IncomingCall, caller: &Jid, video: bool, group: bool) {
        // The library reports offers replayed after a reconnect as missed
        // calls instead; one that slips through cannot be answered.
        if call.offline {
            return;
        }
        if group && !self.calls.busy() {
            log::info!("a group call rang; it is left to the account's other devices");
            self.calls.ignore(call.action.call_id());
            return;
        }
        let chat = self.canonical(caller);
        let name = self.chat_name(&chat, call.notify.as_deref());
        let media = if video {
            CallMedia::Video
        } else {
            CallMedia::Voice
        };
        let steps = self
            .calls
            .offer(Box::new(call.clone()), chat, name, media, now());
        self.run_call_steps(steps);
    }

    pub(super) fn missed_call(&mut self, missed: &MissedCall) {
        let chat = self.canonical(&missed.from);
        let steps = self.calls.missed(&missed.call_id, chat, now());
        self.run_call_steps(steps);
    }

    pub(super) fn call_ended_elsewhere(&mut self, ended: &CallEndedElsewhere) {
        let steps = self.calls.elsewhere(
            &ended.call_id,
            matches!(
                ended.outcome,
                whatsapp_rust::types::call::ElsewhereOutcome::Rejected
            ),
            now(),
        );
        self.run_call_steps(steps);
    }

    pub(super) fn start_call(&mut self, chat: ChatId, video: bool) {
        if !Self::jid_of(&chat).is_some_and(|jid| jid.is_pn() || jid.is_lid()) {
            self.emit(Event::Error(
                "Calls to groups and channels are not supported yet.".to_owned(),
            ));
            return;
        }
        let steps = self.calls.place(chat, video, now());
        self.run_call_steps(steps);
    }

    pub(super) fn accept_call(&mut self, id: CallId) {
        let steps = self.calls.accept(id, now());
        self.run_call_steps(steps);
    }

    pub(super) fn hang_up_call(&mut self, id: CallId) {
        let steps = self.calls.hang_up(id, now());
        self.run_call_steps(steps);
    }

    pub(super) fn mute_call(&mut self, id: CallId, muted: bool) {
        let steps = self.calls.set_muted(id, muted);
        self.run_call_steps(steps);
    }

    /// Remembers the chosen devices and points a running call at them.
    pub(super) fn set_call_devices(&mut self, devices: crate::audio::CallDevices) {
        let camera_changed = devices.camera != self.call_devices.camera;
        self.call_devices = devices.clone();
        let Some(id) = self.calls.current_id() else {
            return;
        };
        if let Some(audio) = self.calls.speaker(id) {
            audio.set_devices(&devices.microphone, &devices.speaker);
        }
        let sending = self
            .calls
            .reception(id)
            .is_some_and(|video| video.sending());
        // A camera opened to ask for video is not running video yet.
        if camera_changed && sending && self.calls.has_video(id) {
            self.restart_camera(id);
        }
    }

    /// Closes the camera that is sending and opens the chosen one.
    fn restart_camera(&mut self, id: CallId) {
        let Some(handle) = self.calls.live(id).map(|live| live.handle.clone()) else {
            return;
        };
        let Some(video) = self.calls.reception(id) else {
            return;
        };
        let camera = video.camera();
        let sink = video.sink();
        let name = self.call_devices.camera.clone();
        let sender = self.wa_sender.clone();
        tokio::spawn(async move {
            let closing = std::sync::Arc::clone(&camera);
            tokio::task::spawn_blocking(move || closing.halt())
                .await
                .ok();
            if let Err(error) = handle.stop_video().await {
                log::info!(
                    "call {}: our video could not be turned off: {}",
                    id.0,
                    error_label(&error)
                );
            }
            let camera_for_open = std::sync::Arc::clone(&camera);
            let opened = tokio::task::spawn_blocking(move || camera_for_open.arm(&name))
                .await
                .unwrap_or(false);
            let (sending, notice) = if opened {
                match handle.resume_video(camera.source(), sink).await {
                    Ok(()) => (true, None),
                    Err(error) => {
                        log::warn!(
                            "call {}: the camera could not be turned on: {}",
                            id.0,
                            error_label(&error)
                        );
                        let closing = std::sync::Arc::clone(&camera);
                        tokio::task::spawn_blocking(move || closing.halt())
                            .await
                            .ok();
                        (
                            false,
                            camera
                                .notice()
                                .or(Some("The camera could not be turned on.".to_owned())),
                        )
                    }
                }
            } else {
                (
                    false,
                    Some(
                        camera
                            .notice()
                            .unwrap_or_else(|| "No camera was found.".to_owned()),
                    ),
                )
            };
            let _ = sender.send(RuntimeEvent::Call(Report::Camera {
                id,
                sending,
                notice,
            }));
        });
    }

    /// Turns our camera on or off. In a voice call, turning it on asks the
    /// other side to switch to video, and turning it off again withdraws that.
    pub(super) fn set_call_camera(&mut self, id: CallId, on: bool) {
        let Some(handle) = self.calls.live(id).map(|live| live.handle.clone()) else {
            return;
        };
        if !self.calls.has_video(id) {
            if on {
                self.switch_to_video(id, handle);
            } else {
                self.withdraw_video(id, handle);
            }
            return;
        }
        let Some(video) = self.calls.reception(id) else {
            return;
        };
        if video.sending() == on {
            return;
        }
        let camera = video.camera();
        let sink = video.sink();
        let sender = self.wa_sender.clone();
        let camera_name = self.call_devices.camera.clone();
        tokio::spawn(async move {
            let (sending, notice) = if on {
                let camera_for_open = std::sync::Arc::clone(&camera);
                let opened = tokio::task::spawn_blocking(move || camera_for_open.arm(&camera_name))
                    .await
                    .unwrap_or(false);
                if opened {
                    match handle.resume_video(camera.source(), sink).await {
                        Ok(()) => (true, None),
                        Err(error) => {
                            log::warn!(
                                "call {}: the camera could not be turned on: {}",
                                id.0,
                                error_label(&error)
                            );
                            let closing = std::sync::Arc::clone(&camera);
                            tokio::task::spawn_blocking(move || closing.halt())
                                .await
                                .ok();
                            (
                                false,
                                camera
                                    .notice()
                                    .or(Some("The camera could not be turned on.".to_owned())),
                            )
                        }
                    }
                } else {
                    let notice = camera
                        .notice()
                        .unwrap_or_else(|| "No camera was found.".to_owned());
                    (false, Some(notice))
                }
            } else {
                let closing = std::sync::Arc::clone(&camera);
                tokio::task::spawn_blocking(move || closing.halt())
                    .await
                    .ok();
                if let Err(error) = handle.stop_video().await {
                    log::info!(
                        "call {}: our video could not be turned off: {}",
                        id.0,
                        error_label(&error)
                    );
                }
                (false, None)
            };
            let _ = sender.send(RuntimeEvent::Call(Report::Camera {
                id,
                sending,
                notice,
            }));
        });
    }

    /// The call's video reception, started when a voice call first needs one.
    fn reception_for(&mut self, id: CallId, handle: &CallHandle) -> Option<&mut Reception> {
        if self.calls.reception(id).is_none() {
            let video = match Reception::start(self.waker.clone()) {
                Ok(video) => video,
                Err(error) => {
                    log::warn!("call {}: video cannot be shown: {error}", id.0);
                    return None;
                }
            };
            finish_video(handle, &video);
            self.emit(Event::CallVideo {
                call: id,
                feed: video.feed(),
            });
            return self.calls.attach_reception(id, video);
        }
        self.calls.reception(id)
    }

    /// Opens our camera and asks the other side to switch a voice call to
    /// video. The call switches once they accept; until then they see nothing
    /// of ours and we show our own picture.
    fn switch_to_video(&mut self, id: CallId, handle: CallHandle) {
        let Some(video) = self.reception_for(id, &handle) else {
            self.emit(Event::Error(
                "Video calls cannot be shown on this computer.".to_owned(),
            ));
            return;
        };
        if video.sending() {
            // Already asking.
            return;
        }
        let camera = video.camera();
        let sink = video.sink();
        let name = self.call_devices.camera.clone();
        let sender = self.wa_sender.clone();
        tokio::spawn(async move {
            let opening = std::sync::Arc::clone(&camera);
            let opened = tokio::task::spawn_blocking(move || opening.arm(&name))
                .await
                .unwrap_or(false);
            if !opened {
                let notice = camera
                    .notice()
                    .unwrap_or_else(|| "No camera was found.".to_owned());
                let _ = sender.send(RuntimeEvent::Call(Report::Camera {
                    id,
                    sending: false,
                    notice: Some(notice),
                }));
                return;
            }
            let _ = sender.send(RuntimeEvent::Call(Report::Camera {
                id,
                sending: true,
                notice: None,
            }));
            log::info!("call {}: asking to switch to video", id.0);
            let accepted = match handle.start_video(camera.source(), sink).await {
                Ok(()) => upgrade_answer(&handle).await,
                Err(error) => {
                    log::warn!(
                        "call {}: could not ask to switch to video: {}",
                        id.0,
                        error_label(&error)
                    );
                    false
                }
            };
            let mut notice = None;
            if !accepted {
                // Turning the camera off meanwhile withdrew the request.
                if camera.sending() {
                    notice = Some("The call stayed a voice call.".to_owned());
                }
                let closing = std::sync::Arc::clone(&camera);
                tokio::task::spawn_blocking(move || closing.halt())
                    .await
                    .ok();
                if handle
                    .video_states()
                    .is_some_and(|(ours, _)| !ours.is_inactive_for_call_mode())
                    && let Err(error) = handle.stop_video().await
                {
                    log::info!(
                        "call {}: our video could not be turned off: {}",
                        id.0,
                        error_label(&error)
                    );
                }
            }
            let _ = sender.send(RuntimeEvent::Call(Report::Switched {
                id,
                video: accepted,
                notice,
            }));
        });
    }

    /// Closes the camera opened to ask for video, before the other side
    /// answered. Asking ends with it.
    fn withdraw_video(&mut self, id: CallId, handle: CallHandle) {
        let Some(camera) = self
            .calls
            .reception(id)
            .filter(|video| video.sending())
            .map(|video| video.camera())
        else {
            return;
        };
        let sender = self.wa_sender.clone();
        tokio::spawn(async move {
            tokio::task::spawn_blocking(move || camera.halt())
                .await
                .ok();
            if let Err(error) = handle.stop_video().await {
                log::info!(
                    "call {}: our video could not be turned off: {}",
                    id.0,
                    error_label(&error)
                );
            }
            let _ = sender.send(RuntimeEvent::Call(Report::Camera {
                id,
                sending: false,
                notice: None,
            }));
        });
    }

    /// The other side's video signalling for a running call.
    fn peer_video(&mut self, id: CallId, state: VideoState, token: Option<VideoUpgradeToken>) {
        if video_stops(state)
            && let Some(video) = self.calls.reception(id)
        {
            video.feed().stopped();
        }
        match state {
            // `None` is a request that crossed ours; the library accepted it.
            VideoState::UpgradeRequest | VideoState::UpgradeRequestV2 => {
                if let Some(token) = token
                    && !self.calls.has_video(id)
                {
                    self.accept_video_request(id, token);
                }
            }
            // The library turns the call's video off for these, ours included.
            VideoState::Disabled
            | VideoState::UpgradeCancel
            | VideoState::UpgradeCancelByTimeout
            | VideoState::Error
                if self.calls.has_video(id) =>
            {
                if let Some(camera) = self
                    .calls
                    .reception(id)
                    .filter(|video| video.sending())
                    .map(|video| video.camera())
                {
                    tokio::task::spawn_blocking(move || camera.halt());
                }
                self.emit(Event::CallCamera {
                    call: id,
                    sending: false,
                    preview: None,
                });
                let steps = self.calls.switch_media(id, false, now());
                self.run_call_steps(steps);
            }
            _ => {}
        }
    }

    /// The other side asks to switch to video. Their picture is accepted at
    /// once; ours stays off until the person turns the camera on, which then
    /// resumes our stopped direction.
    fn accept_video_request(&mut self, id: CallId, token: VideoUpgradeToken) {
        let Some(handle) = self.calls.live(id).map(|live| live.handle.clone()) else {
            return;
        };
        // Without a way to show it, the request is left to their timer.
        let Some(video) = self.reception_for(id, &handle) else {
            return;
        };
        let sending = video.sending();
        let source = video.source();
        let sink = video.sink();
        let sender = self.wa_sender.clone();
        tokio::spawn(async move {
            if let Err(error) = handle.accept_video(token, source, sink).await {
                log::info!(
                    "call {}: their video could not be accepted: {}",
                    id.0,
                    error_label(&error)
                );
                return;
            }
            if !sending && let Err(error) = handle.stop_video().await {
                log::info!(
                    "call {}: our video could not be turned off: {}",
                    id.0,
                    error_label(&error)
                );
            }
            let _ = sender.send(RuntimeEvent::Call(Report::Switched {
                id,
                video: true,
                notice: None,
            }));
        });
    }

    pub(super) fn call_report(&mut self, report: Report) {
        let steps = match report {
            Report::Started { id, result } => {
                let result = match result {
                    Ok(live) => {
                        let protocol = live.handle.call_id().to_owned();
                        if let Some(video) = &live.video {
                            let sending = video.sending();
                            let preview = sending.then(|| video.preview());
                            self.emit(Event::CallCamera {
                                call: id,
                                sending,
                                preview,
                            });
                            if !sending && let Some(notice) = video.notice() {
                                self.emit(Event::Error(notice));
                            }
                        }
                        Ok((*live, protocol))
                    }
                    Err(failure) => {
                        if let Some(notice) = failure.notice {
                            self.emit(Event::Error(notice));
                        }
                        Err(failure.reason)
                    }
                };
                self.calls.started(id, result, now())
            }
            Report::Over { id, failed } => self.calls.over(id, failed, now()),
            Report::Muted { id, muted } => self.calls.muted(id, muted),
            Report::Camera {
                id,
                sending,
                notice,
            } => {
                let preview = sending
                    .then(|| self.calls.reception(id).map(|video| video.preview()))
                    .flatten();
                self.emit(Event::CallCamera {
                    call: id,
                    sending,
                    preview,
                });
                if let Some(notice) = notice {
                    self.emit(Event::Error(notice));
                }
                return;
            }
            Report::PeerVideo { id, state, token } => {
                self.peer_video(id, state, token);
                return;
            }
            Report::Switched { id, video, notice } => {
                if !video {
                    self.emit(Event::CallCamera {
                        call: id,
                        sending: false,
                        preview: None,
                    });
                }
                if let Some(notice) = notice {
                    self.emit(Event::Info(notice));
                }
                self.calls.switch_media(id, video, now())
            }
        };
        self.run_call_steps(steps);
    }

    /// Ends the call before the connection stops, waiting briefly for the
    /// other side to be told.
    pub(super) async fn end_call_for_stop(&mut self) {
        for step in self.calls.shutdown(now()) {
            match step {
                Step::Terminate(Live {
                    handle,
                    audio,
                    video,
                }) => {
                    drop((audio, video));
                    if tokio::time::timeout(STOP_TERMINATE, handle.terminate())
                        .await
                        .is_err()
                    {
                        log::info!("a call ended before the other side could be told");
                    }
                }
                step => self.run_call_step(step),
            }
        }
        self.drain_call_logs();
    }

    fn run_call_steps(&mut self, steps: Vec<Step<Live>>) {
        for step in steps {
            self.run_call_step(step);
        }
        self.drain_call_logs();
    }

    fn drain_call_logs(&mut self) {
        for note in self.calls.take_logs() {
            self.file_call(note);
        }
    }

    /// Writes a call line into the chat. A finished line is not replaced by a
    /// later "still ringing" one. Nothing is marked unread: the call itself
    /// already notified.
    fn file_call(&mut self, note: CallNote) {
        let id = format!("call:{}", note.protocol);
        if let Ok(Some(existing)) = self.archive.message(&note.chat, &id)
            && let crate::model::Content::Call { record, .. } = &existing.content
            && record.finished()
            && !note.record.finished()
        {
            return;
        }
        let name = self.chat_name(&note.chat, None);
        if let Err(error) = self.archive.ensure_chat(&note.chat, &name) {
            log::warn!("could not store a call: {error}");
            return;
        }
        let existed = self
            .archive
            .message(&note.chat, &id)
            .ok()
            .flatten()
            .is_some();
        let message = crate::model::Message {
            id: id.clone(),
            chat: note.chat.clone(),
            sender: if note.outgoing {
                self.me()
            } else {
                note.chat.clone()
            },
            sender_name: None,
            from_me: note.outgoing,
            timestamp: note.started,
            history_order: None,
            content: crate::model::Content::Call {
                video: note.video,
                record: note.record,
            },
            status: if note.outgoing {
                crate::model::Delivery::Sent
            } else {
                crate::model::Delivery::None
            },
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        };
        if let Err(error) = self.archive.insert_message(&message, None) {
            log::warn!("could not store a call: {error}");
            return;
        }
        if existed {
            self.emit_message(&note.chat, &id);
        } else if let Ok(Some(stored)) = self.archive.message(&note.chat, &id) {
            self.emit(Event::Messages {
                chat: note.chat.clone(),
                messages: vec![stored],
                older: false,
                complete: false,
            });
        }
        self.emit_chat(&note.chat);
    }

    fn run_call_step(&mut self, step: Step<Live>) {
        match step {
            Step::Emit(event) => self.emit(event),
            Step::Decline(offer) => {
                let Some(client) = self.client.clone() else {
                    return;
                };
                tokio::spawn(async move {
                    if let Err(error) = client.voip().reject(&offer).await {
                        log::info!("a call could not be declined: {}", error_label(&error));
                    }
                });
            }
            Step::Answer { id, offer } => {
                let Some(client) = self.client.clone() else {
                    self.call_not_connected(id);
                    return;
                };
                let video = self.receive_video(id, &offer);
                let sender = self.wa_sender.clone();
                let devices = self.call_devices.clone();
                tokio::spawn(async move {
                    if let Some(video) = &video {
                        let camera = video.camera();
                        let camera_name = devices.camera.clone();
                        tokio::task::spawn_blocking(move || camera.arm(&camera_name))
                            .await
                            .ok();
                    }
                    let result = match open_audio(false, devices).await {
                        Ok((audio, ends)) => {
                            let voip = client.voip();
                            let accept = voip.accept(&offer).audio(ends.source, ends.sink);
                            let accept = match &video {
                                Some(video) => accept.video(video.source(), video.sink()),
                                None => accept,
                            };
                            match accept.start().await {
                                Ok(handle) => {
                                    if let Some(video) = &video {
                                        finish_video(&handle, video);
                                    }
                                    Ok(Live {
                                        handle,
                                        audio,
                                        video,
                                    })
                                }
                                Err(error) => Err(setup_failure(&error)),
                            }
                        }
                        Err(failure) => Err(failure),
                    };
                    let _ = sender.send(RuntimeEvent::Call(Report::Started {
                        id,
                        result: result.map(Box::new),
                    }));
                });
            }
            Step::Place { id, chat, video } => {
                let (Some(client), Some(peer)) = (self.client.clone(), Self::jid_of(&chat)) else {
                    self.call_not_connected(id);
                    return;
                };
                let sender = self.wa_sender.clone();
                let devices = self.call_devices.clone();
                let reception = if video {
                    match Reception::start(self.waker.clone()) {
                        Ok(reception) => {
                            self.emit(Event::CallVideo {
                                call: id,
                                feed: reception.feed(),
                            });
                            Some(reception)
                        }
                        Err(error) => {
                            log::warn!("call {}: video cannot be shown: {error}", id.0);
                            None
                        }
                    }
                } else {
                    None
                };
                tokio::spawn(async move {
                    if let Some(video) = &reception {
                        let camera = video.camera();
                        let camera_name = devices.camera.clone();
                        tokio::task::spawn_blocking(move || camera.arm(&camera_name))
                            .await
                            .ok();
                    }
                    let result = match open_audio(true, devices).await {
                        Ok((audio, ends)) => {
                            let started = match &reception {
                                Some(video) => {
                                    client
                                        .voip()
                                        .call(&peer)
                                        .audio(ends.source, ends.sink)
                                        .video(video.source(), video.sink())
                                        .start()
                                        .await
                                }
                                None => {
                                    client
                                        .voip()
                                        .call(&peer)
                                        .audio(ends.source, ends.sink)
                                        .start()
                                        .await
                                }
                            };
                            match started {
                                Ok(handle) => {
                                    if let Some(video) = &reception {
                                        finish_video(&handle, video);
                                    }
                                    Ok(Live {
                                        handle,
                                        audio,
                                        video: reception,
                                    })
                                }
                                Err(error) => Err(setup_failure(&error)),
                            }
                        }
                        Err(failure) => Err(failure),
                    };
                    let _ = sender.send(RuntimeEvent::Call(Report::Started {
                        id,
                        result: result.map(Box::new),
                    }));
                });
            }
            Step::LineOpen(id) => {
                if let Some(audio) = self.calls.speaker(id) {
                    audio.line_open();
                }
            }
            Step::Watch(id) => {
                let Some(handle) = self.calls.live(id).map(|live| live.handle.clone()) else {
                    return;
                };
                let sender = self.wa_sender.clone();
                // The camera is shared with the task that opened it, so this
                // sees whether that open succeeded.
                let silence_camera = self
                    .calls
                    .reception(id)
                    .is_some_and(|video| !video.sending());
                tokio::spawn(async move {
                    let failed = watch(id, &handle, &sender, silence_camera).await;
                    if failed {
                        log::warn!("call {}: the media connection failed", id.0);
                    }
                    let _ = sender.send(RuntimeEvent::Call(Report::Over { id, failed }));
                });
            }
            Step::Mute { id, muted } => {
                let Some(handle) = self.calls.live(id).map(|live| live.handle.clone()) else {
                    return;
                };
                let sender = self.wa_sender.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle.set_muted(muted).await {
                        log::warn!(
                            "call {}: the other side was not told of the mute: {}",
                            id.0,
                            error_label(&error)
                        );
                    }
                    let muted = handle.is_muted();
                    let _ = sender.send(RuntimeEvent::Call(Report::Muted { id, muted }));
                });
            }
            Step::Terminate(Live {
                handle,
                audio,
                video,
            }) => {
                drop((audio, video));
                tokio::spawn(async move {
                    match handle.terminate().await {
                        CallTermination::PeerNotified | CallTermination::AlreadyEnded => {}
                        _ => log::info!("a call ended without the other side confirming it"),
                    }
                });
            }
            Step::Release(Live {
                handle,
                audio,
                video,
            }) => {
                drop((audio, video));
                tokio::spawn(async move { handle.hangup_local().await });
            }
        }
    }

    /// Starts receiving the other side's video when `offer` is a video
    /// call, and hands the interface its feed. A call whose decode thread cannot
    /// start is answered as a voice call.
    fn receive_video(&mut self, id: CallId, offer: &IncomingCall) -> Option<Reception> {
        if !matches!(offer.action, CallAction::Offer { is_video: true, .. }) {
            return None;
        }
        match Reception::start(self.waker.clone()) {
            Ok(video) => {
                self.emit(Event::CallVideo {
                    call: id,
                    feed: video.feed(),
                });
                Some(video)
            }
            Err(error) => {
                log::warn!("call {}: video cannot be shown: {error}", id.0);
                None
            }
        }
    }

    /// A setup that cannot start without a connection.
    fn call_not_connected(&mut self, id: CallId) {
        self.emit(Event::Error(
            "Connect to WhatsApp to make or answer calls.".to_owned(),
        ));
        let steps = self.calls.started(id, Err(CallEndReason::Failed), now());
        self.run_call_steps(steps);
    }
}

#[cfg(test)]
mod tests {
    use super::super::receipt_tests::{PEER, worker};
    use super::*;

    const LID: &str = "167650256810092";
    const CALLER: &str = "15550000001@s.whatsapp.net";

    /// A stand-in for a running call's handle and audio.
    #[derive(Debug, PartialEq)]
    struct Fake(u32);

    fn offer(protocol: &str, from: &str, video: bool) -> Box<IncomingCall> {
        let from: Jid = from.parse().unwrap();
        Box::new(
            IncomingCall::builder()
                .from(from.clone())
                .stanza_id("stanza".to_owned())
                .timestamp(whatsapp_rust::wacore::time::from_millis_or_now(1_000))
                .offline(false)
                .action(CallAction::Offer {
                    call_id: protocol.to_owned(),
                    call_creator: from,
                    caller_pn: None,
                    caller_country_code: None,
                    device_class: None,
                    joinable: false,
                    is_video: video,
                    audio: Vec::new(),
                    group_jid: None,
                })
                .build(),
        )
    }

    /// The steps, short enough to compare.
    fn summary(steps: &[Step<Fake>]) -> Vec<String> {
        steps
            .iter()
            .map(|step| match step {
                Step::Emit(Event::CallIncoming {
                    call, chat, media, ..
                }) => format!("incoming {} {chat} {media:?}", call.0),
                Step::Emit(Event::CallState {
                    call, phase, since, ..
                }) => format!("state {} {phase:?} {since}", call.0),
                Step::Emit(Event::CallEnded { call, chat, reason }) => {
                    format!("ended {} {chat} {reason:?}", call.0)
                }
                Step::Emit(Event::CallMuted { call, muted }) => {
                    format!("muted {} {muted}", call.0)
                }
                Step::Emit(Event::Error(_)) => "error".to_owned(),
                Step::Emit(_) => "other event".to_owned(),
                Step::Decline(offer) => format!("decline {}", offer.action.call_id()),
                Step::Answer { id, offer } => {
                    format!("answer {} {}", id.0, offer.action.call_id())
                }
                Step::Place { id, chat, .. } => format!("place {} {chat}", id.0),
                Step::Watch(id) => format!("watch {}", id.0),
                Step::LineOpen(id) => format!("open {}", id.0),
                Step::Mute { id, muted } => format!("mute {} {muted}", id.0),
                Step::Terminate(fake) => format!("terminate {}", fake.0),
                Step::Release(fake) => format!("release {}", fake.0),
            })
            .collect()
    }

    fn ringing(calls: &mut Calls<Fake>, protocol: &str) -> CallId {
        let steps = calls.offer(
            offer(protocol, CALLER, false),
            CALLER.to_owned(),
            "Ada".to_owned(),
            CallMedia::Voice,
            10,
        );
        assert_eq!(
            summary(&steps),
            [
                format!("incoming {} {CALLER} Voice", calls.next),
                format!("state {} Ringing 10", calls.next),
            ]
        );
        CallId(calls.next)
    }

    fn connected(calls: &mut Calls<Fake>, protocol: &str) -> CallId {
        let id = ringing(calls, protocol);
        calls.accept(id, 20);
        let steps = calls.started(id, Ok((Fake(7), protocol.to_owned())), 30);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Connected 30", id.0),
                format!("open {}", id.0),
                format!("watch {}", id.0)
            ]
        );
        id
    }

    fn dialled(calls: &mut Calls<Fake>, protocol: &str) -> CallId {
        let steps = calls.place(PEER.to_owned(), false, 10);
        let id = CallId(calls.next);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Connecting 10", id.0),
                format!("place {} {PEER}", id.0),
            ]
        );
        let steps = calls.started(id, Ok((Fake(9), protocol.to_owned())), 20);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Ringing 20", id.0),
                format!("watch {}", id.0)
            ]
        );
        id
    }

    #[test]
    fn an_offer_during_a_ringing_call_is_declined_once() {
        let mut calls = Calls::<Fake>::default();
        let id = ringing(&mut calls, "A");
        let steps = calls.offer(
            offer("B", "15550000002@s.whatsapp.net", false),
            "15550000002@s.whatsapp.net".to_owned(),
            "Bo".to_owned(),
            CallMedia::Voice,
            11,
        );
        assert_eq!(
            summary(&steps),
            ["decline B"],
            "nothing reaches the interface"
        );
        assert_eq!(calls.current.as_ref().map(|call| call.id), Some(id));
        let again = calls.offer(
            offer("B", "15550000002@s.whatsapp.net", false),
            String::new(),
            String::new(),
            CallMedia::Voice,
            12,
        );
        assert!(again.is_empty(), "a repeated offer is not declined twice");
        // The declined call's later hang-up does not touch the ringing one.
        assert!(calls.signal("B", Signal::Terminated, 13).is_empty());
        assert!(calls.missed("B", String::new(), 13).is_empty());
        assert_eq!(calls.current.as_ref().map(|call| call.id), Some(id));
    }

    #[test]
    fn an_offer_during_a_connected_or_dialled_call_is_declined() {
        let mut calls = Calls::<Fake>::default();
        connected(&mut calls, "A");
        let steps = calls.offer(
            offer("B", CALLER, true),
            CALLER.to_owned(),
            String::new(),
            CallMedia::Video,
            40,
        );
        assert_eq!(summary(&steps), ["decline B"]);

        let mut calls = Calls::<Fake>::default();
        calls.place(PEER.to_owned(), false, 10);
        let steps = calls.offer(
            offer("C", CALLER, false),
            CALLER.to_owned(),
            String::new(),
            CallMedia::Voice,
            11,
        );
        assert_eq!(summary(&steps), ["decline C"], "busy while still dialling");
    }

    #[test]
    fn a_repeated_offer_rings_once() {
        let mut calls = Calls::<Fake>::default();
        ringing(&mut calls, "A");
        let again = calls.offer(
            offer("A", CALLER, false),
            CALLER.to_owned(),
            "Ada".to_owned(),
            CallMedia::Voice,
            11,
        );
        assert!(again.is_empty());
    }

    #[test]
    fn an_ended_call_does_not_ring_again() {
        let mut calls = Calls::<Fake>::default();
        let id = ringing(&mut calls, "A");
        let steps = calls.hang_up(id, 20);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Ended 20", id.0),
                format!("ended {} {CALLER} Rejected", id.0),
                "decline A".to_owned(),
            ]
        );
        let again = calls.offer(
            offer("A", CALLER, false),
            CALLER.to_owned(),
            String::new(),
            CallMedia::Voice,
            21,
        );
        assert!(again.is_empty());
        assert!(!calls.busy());
    }

    #[test]
    fn ended_elsewhere_clears_the_ringing_call_once() {
        let mut calls = Calls::<Fake>::default();
        let id = ringing(&mut calls, "A");
        let steps = calls.elsewhere("A", false, 20);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Ended 20", id.0),
                format!("ended {} {CALLER} EndedElsewhere", id.0),
            ],
            "nothing is declined: the phone already settled it"
        );
        assert!(!calls.busy());
        assert!(calls.elsewhere("A", false, 21).is_empty());
        // The caller's terminate that follows the dismissal is the same call.
        assert!(calls.signal("A", Signal::Terminated, 22).is_empty());
        assert!(calls.accept(id, 23).is_empty(), "too late to answer");
    }

    #[test]
    fn ended_elsewhere_while_answering_terminates_the_late_handle() {
        let mut calls = Calls::<Fake>::default();
        let id = ringing(&mut calls, "A");
        assert_eq!(
            summary(&calls.accept(id, 20)),
            [
                format!("state {} Connecting 20", id.0),
                format!("answer {} A", id.0)
            ]
        );
        let steps = calls.elsewhere("A", false, 21);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Ended 21", id.0),
                format!("ended {} {CALLER} EndedElsewhere", id.0),
            ]
        );
        let steps = calls.started(id, Ok((Fake(3), "A".to_owned())), 22);
        assert_eq!(
            summary(&steps),
            ["terminate 3"],
            "the handle and its audio go"
        );
        assert!(calls.live(id).is_none());
        assert!(!calls.busy());
    }

    #[test]
    fn a_missed_ring_ends_once() {
        let mut calls = Calls::<Fake>::default();
        let id = ringing(&mut calls, "A");
        let steps = calls.missed("A", CALLER.to_owned(), 20);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Ended 20", id.0),
                format!("ended {} {CALLER} Missed", id.0),
            ]
        );
        assert!(calls.missed("A", CALLER.to_owned(), 21).is_empty());
        assert!(calls.signal("A", Signal::Terminated, 21).is_empty());
    }

    #[test]
    fn a_caller_hanging_up_before_the_missed_event_ends_it_as_missed() {
        let mut calls = Calls::<Fake>::default();
        let id = ringing(&mut calls, "A");
        let steps = calls.signal("A", Signal::Terminated, 20);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Ended 20", id.0),
                format!("ended {} {CALLER} Missed", id.0),
            ]
        );
        assert!(calls.missed("A", CALLER.to_owned(), 21).is_empty());
    }

    #[test]
    fn a_call_missed_while_offline_is_announced_once() {
        let mut calls = Calls::<Fake>::default();
        let steps = calls.missed("X", CALLER.to_owned(), 5);
        assert_eq!(summary(&steps), [format!("ended 1 {CALLER} Missed")]);
        assert!(calls.missed("X", CALLER.to_owned(), 6).is_empty());
        assert!(!calls.busy(), "it never rang");
    }

    #[test]
    fn a_repeated_accept_answers_once() {
        let mut calls = Calls::<Fake>::default();
        let id = ringing(&mut calls, "A");
        assert_eq!(calls.accept(id, 20).len(), 2);
        assert!(calls.accept(id, 21).is_empty());
        assert!(calls.accept(CallId(99), 21).is_empty(), "an unknown call");
    }

    #[test]
    fn a_failed_answer_ends_the_call() {
        let mut calls = Calls::<Fake>::default();
        let id = ringing(&mut calls, "A");
        calls.accept(id, 20);
        let steps = calls.started(id, Err(CallEndReason::Failed), 21);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Ended 21", id.0),
                format!("ended {} {CALLER} Failed", id.0),
            ]
        );
        assert!(calls.started(id, Err(CallEndReason::Failed), 22).is_empty());
    }

    #[test]
    fn a_connected_call_ends_once_whoever_hangs_up() {
        let mut calls = Calls::<Fake>::default();
        let id = connected(&mut calls, "A");
        let steps = calls.signal("A", Signal::Terminated, 50);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Ended 50", id.0),
                format!("ended {} {CALLER} HungUp", id.0),
                "release 7".to_owned(),
            ]
        );
        assert!(
            calls.over(id, false, 51).is_empty(),
            "the library's end is the same one"
        );
        assert!(calls.hang_up(id, 52).is_empty());

        let mut calls = Calls::<Fake>::default();
        let id = connected(&mut calls, "B");
        assert_eq!(
            summary(&calls.hang_up(id, 50))[1..],
            [
                format!("ended {} {CALLER} HungUp", id.0),
                "terminate 7".to_owned()
            ]
        );
        assert!(calls.over(id, false, 51).is_empty());
    }

    #[test]
    fn a_media_failure_ends_the_call_and_tells_the_other_side() {
        let mut calls = Calls::<Fake>::default();
        let id = connected(&mut calls, "A");
        assert_eq!(
            summary(&calls.over(id, true, 60))[1..],
            [
                format!("ended {} {CALLER} Failed", id.0),
                "terminate 7".to_owned()
            ]
        );
    }

    #[test]
    fn a_placed_call_connects_when_answered() {
        let mut calls = Calls::<Fake>::default();
        let id = dialled(&mut calls, "P");
        assert_eq!(
            summary(&calls.signal("P", Signal::Accepted, 30)),
            [
                format!("state {} Connected 30", id.0),
                format!("open {}", id.0),
            ]
        );
        assert!(calls.signal("P", Signal::Accepted, 31).is_empty());
        assert!(
            calls.signal("Q", Signal::Terminated, 31).is_empty(),
            "another call"
        );
        assert_eq!(
            summary(&calls.hang_up(id, 40))[1..],
            [
                format!("ended {} {PEER} HungUp", id.0),
                "terminate 9".to_owned()
            ]
        );
    }

    #[test]
    fn a_declined_placed_call_ends_rejected() {
        let mut calls = Calls::<Fake>::default();
        let id = dialled(&mut calls, "P");
        assert_eq!(
            summary(&calls.signal("P", Signal::Rejected { device: false }, 30))[1..],
            [
                format!("ended {} {PEER} Rejected", id.0),
                "release 9".to_owned()
            ]
        );
        assert!(calls.signal("P", Signal::Terminated, 31).is_empty());
        assert!(calls.over(id, false, 31).is_empty());
    }

    #[test]
    fn a_busy_callee_rings_on_and_ends_busy() {
        let mut calls = Calls::<Fake>::default();
        let id = dialled(&mut calls, "P");
        assert!(
            calls
                .signal("P", Signal::Rejected { device: true }, 30)
                .is_empty(),
            "the callee's other devices still ring"
        );
        assert!(calls.busy());
        assert_eq!(
            summary(&calls.over(id, false, 40))[1..],
            [
                format!("ended {} {PEER} Busy", id.0),
                "release 9".to_owned()
            ]
        );
    }

    #[test]
    fn an_unanswered_placed_call_ends_missed() {
        let mut calls = Calls::<Fake>::default();
        let id = dialled(&mut calls, "P");
        assert_eq!(
            summary(&calls.over(id, false, 40))[1],
            format!("ended {} {PEER} Missed", id.0)
        );
    }

    #[test]
    fn hanging_up_while_dialling_terminates_the_late_handle() {
        let mut calls = Calls::<Fake>::default();
        calls.place(PEER.to_owned(), false, 10);
        let id = CallId(calls.next);
        assert_eq!(
            summary(&calls.hang_up(id, 11)),
            [
                format!("state {} Ended 11", id.0),
                format!("ended {} {PEER} HungUp", id.0),
            ]
        );
        let steps = calls.started(id, Ok((Fake(4), "P".to_owned())), 12);
        assert_eq!(summary(&steps), ["terminate 4"]);
        assert!(calls.signal("P", Signal::Accepted, 13).is_empty());
    }

    #[test]
    fn a_second_placed_call_is_refused() {
        let mut calls = Calls::<Fake>::default();
        calls.place(PEER.to_owned(), false, 10);
        assert_eq!(
            summary(&calls.place(CALLER.to_owned(), false, 11)),
            ["error"]
        );
        assert_eq!(calls.next, 1);
    }

    #[test]
    fn a_mute_before_the_answer_is_applied_again_once_connected() {
        let mut calls = Calls::<Fake>::default();
        calls.place(PEER.to_owned(), false, 10);
        let id = CallId(calls.next);
        assert_eq!(
            summary(&calls.set_muted(id, true)),
            [format!("muted {} true", id.0)]
        );
        let steps = calls.started(id, Ok((Fake(9), "P".to_owned())), 20);
        assert_eq!(summary(&steps)[2], format!("mute {} true", id.0));
        let steps = calls.signal("P", Signal::Accepted, 30);
        assert_eq!(summary(&steps)[2], format!("mute {} true", id.0));
        // The library reports it could not unmute: the call stays muted.
        assert_eq!(calls.set_muted(id, false).len(), 1);
        assert_eq!(
            summary(&calls.muted(id, true)),
            [format!("muted {} true", id.0)]
        );
        assert!(calls.current.as_ref().unwrap().muted);
        assert!(calls.muted(CallId(42), false).is_empty());
    }

    #[test]
    fn an_incoming_call_is_filed_until_it_is_missed() {
        let mut calls = Calls::<Fake>::default();
        ringing(&mut calls, "A");
        let note = calls.logs.last().expect("the ring is filed");
        assert!(matches!(note.record, CallRecord::Incoming));
        assert!(!note.video);
        assert_eq!(note.protocol, "A");
        calls.finish(CallEndReason::Missed, false, 20_000);
        let note = calls.logs.last().expect("the miss replaces the ring");
        assert!(matches!(note.record, CallRecord::Missed));
        assert_eq!(calls.logs.len(), 1);
    }

    #[test]
    fn stopping_cuts_a_running_call_and_leaves_a_ringing_one() {
        let mut calls = Calls::<Fake>::default();
        let id = connected(&mut calls, "A");
        assert_eq!(
            summary(&calls.shutdown(60))[1..],
            [
                format!("ended {} {CALLER} Failed", id.0),
                "terminate 7".to_owned()
            ]
        );

        let mut calls = Calls::<Fake>::default();
        let id = ringing(&mut calls, "B");
        assert_eq!(
            summary(&calls.shutdown(60))[1..],
            [format!("ended {} {CALLER} Missed", id.0)],
            "the phone can still answer it"
        );
        assert!(calls.shutdown(61).is_empty());
    }

    #[test]
    fn old_call_ids_are_forgotten_in_turn() {
        let mut calls = Calls::<Fake>::default();
        for n in 0..=FINISHED {
            calls.missed(&format!("X{n}"), CALLER.to_owned(), 1);
        }
        assert_eq!(calls.finished.len(), FINISHED);
        assert!(!calls.known("X0"));
        assert!(calls.known(&format!("X{FINISHED}")));
    }

    fn sent(events: &std::sync::mpsc::Receiver<Event>) -> Vec<Event> {
        events.try_iter().collect()
    }

    #[tokio::test]
    async fn an_offer_from_a_privacy_id_rings_under_the_phone_number() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.learn_lid(LID, "4917663430455");
        sent(&events);
        let call = offer("A", &format!("{LID}@lid"), true);
        worker
            .handle_wa_event(Arc::new(wa_events::Event::IncomingCall(call)))
            .await;
        let sent = sent(&events);
        let Some(Event::CallIncoming {
            call, chat, media, ..
        }) = sent.first()
        else {
            panic!("no ring");
        };
        assert_eq!(chat, PEER);
        assert_eq!(*media, CallMedia::Video);
        assert!(matches!(
            sent.get(1),
            Some(Event::CallState { phase: CallPhase::Ringing, call: state, .. }) if state == call
        ));
        assert!(worker.calls.busy());
    }

    #[tokio::test]
    async fn a_second_ring_and_the_library_s_endings_reach_the_worker() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker
            .handle_wa_event(Arc::new(wa_events::Event::IncomingCall(offer(
                "A", CALLER, false,
            ))))
            .await;
        sent(&events);
        // Declined as busy: without a connection nothing goes out, and the
        // interface hears nothing of it.
        worker
            .handle_wa_event(Arc::new(wa_events::Event::IncomingCall(offer(
                "B",
                "15550000002@s.whatsapp.net",
                false,
            ))))
            .await;
        assert!(sent(&events).is_empty());

        let at = whatsapp_rust::wacore::time::from_millis_or_now(2_000);
        let ended = CallEndedElsewhere::new(
            CALLER.parse().unwrap(),
            "A".to_owned(),
            at,
            whatsapp_rust::types::call::ElsewhereOutcome::Accepted,
        );
        worker
            .handle_wa_event(Arc::new(wa_events::Event::CallEndedElsewhere(ended)))
            .await;
        let ended = sent(&events);
        assert!(ended.iter().any(|event| matches!(
            event,
            Event::CallEnded {
                reason: CallEndReason::EndedElsewhere,
                ..
            }
        )));
        assert!(!worker.calls.busy());

        let missed = MissedCall::new(
            CALLER.parse().unwrap(),
            "Z".to_owned(),
            at,
            whatsapp_rust::types::call::MissedReason::Offline,
        );
        worker
            .handle_wa_event(Arc::new(wa_events::Event::MissedCall(missed.clone())))
            .await;
        worker
            .handle_wa_event(Arc::new(wa_events::Event::MissedCall(missed)))
            .await;
        let sent = sent(&events);
        let missed = sent
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    Event::CallEnded {
                        chat,
                        reason: CallEndReason::Missed,
                        ..
                    } if chat == CALLER
                )
            })
            .count();
        assert_eq!(missed, 1, "a repeated missed call is announced once");
    }

    #[tokio::test]
    async fn a_call_without_a_connection_fails_at_once() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.start_call(PEER.to_owned(), false);
        let placed = sent(&events);
        assert!(matches!(
            placed.first(),
            Some(Event::CallState {
                phase: CallPhase::Connecting,
                ..
            })
        ));
        assert!(placed.iter().any(|event| matches!(
            event,
            Event::CallEnded {
                reason: CallEndReason::Failed,
                ..
            }
        )));
        assert!(!worker.calls.busy());
        worker.start_call("120363000000000042@g.us".to_owned(), false);
        assert!(matches!(sent(&events)[..], [Event::Error(_)]));
    }

    #[tokio::test]
    async fn only_a_video_offer_receives_video() {
        let (mut worker, events, _inbox, _wa) = worker();
        assert!(
            worker
                .receive_video(CallId(1), &offer("A", CALLER, false))
                .is_none()
        );
        assert!(sent(&events).is_empty(), "a voice call has no feed");
        let video = worker
            .receive_video(CallId(2), &offer("B", CALLER, true))
            .expect("a video call receives video");
        match &sent(&events)[..] {
            [Event::CallVideo { call, feed }] => {
                assert_eq!(*call, CallId(2));
                assert_eq!(*feed, video.feed(), "the interface gets the decoder's feed");
            }
            other => panic!("expected the call's feed, got {other:?}"),
        }
    }

    #[test]
    fn the_other_sides_video_stops_when_they_turn_it_off() {
        assert!(video_stops(VideoState::Stopped));
        assert!(video_stops(VideoState::Paused));
        assert!(video_stops(VideoState::Disabled));
        assert!(!video_stops(VideoState::Enabled));
        assert!(!video_stops(VideoState::UpgradeRequestV2));
    }

    /// The media and start time a step announces, when it is a call state.
    fn media_of(steps: &[Step<Fake>]) -> Vec<(CallMedia, i64)> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Emit(Event::CallState { media, since, .. }) => Some((*media, *since)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_voice_call_switches_to_video_and_back_without_restarting_its_timer() {
        let mut calls = Calls::<Fake>::default();
        let id = dialled(&mut calls, "P");
        calls.signal("P", Signal::Accepted, 30);
        assert!(!calls.has_video(id));
        assert_eq!(
            media_of(&calls.switch_media(id, true, 50)),
            [(CallMedia::Video, 30)]
        );
        assert!(calls.has_video(id));
        assert!(calls.switch_media(id, true, 51).is_empty(), "already video");
        assert_eq!(
            media_of(&calls.switch_media(id, false, 60)),
            [(CallMedia::Voice, 30)]
        );
        // The chat line keeps what the call was placed as.
        calls.hang_up(id, 70);
        let notes = calls.take_logs();
        assert!(notes.iter().all(|note| !note.video));
    }

    #[test]
    fn a_call_that_is_not_running_does_not_switch() {
        let mut calls = Calls::<Fake>::default();
        let id = ringing(&mut calls, "R");
        assert!(calls.switch_media(id, true, 5).is_empty());
        assert!(calls.switch_media(CallId(99), true, 5).is_empty());
        assert!(!calls.has_video(id));
    }

    #[test]
    fn a_request_to_switch_to_video_is_settled_by_either_direction() {
        use VideoState::*;
        // Still asking: ours is requested, theirs is not on yet.
        assert_eq!(upgrade_verdict(UpgradeRequestV2, Disabled), None);
        assert_eq!(upgrade_verdict(UpgradeRequestV2, UpgradeAccept), Some(true));
        assert_eq!(upgrade_verdict(Enabled, Enabled), Some(true));
        // The library withdrew ours: declined, or WhatsApp's timer ran out.
        assert_eq!(
            upgrade_verdict(UpgradeCancelByTimeout, Disabled),
            Some(false)
        );
        assert_eq!(upgrade_verdict(Disabled, UpgradeReject), Some(false));
    }
}
