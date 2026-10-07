//! Calls: one at a time per account, placed as voice calls for now.
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
//! A second offer while a call rings or runs is declined at once. The
//! library at this revision sends a reject without a reason, so the caller
//! sees a decline rather than "busy".
//!
//! WhatsApp's call ids, the callers' ids and the library's errors never
//! reach the log: calls are logged by the number this worker gave them.

use super::*;
use crate::audio::{CallAudio, CallEndpoints};
use crate::model::{CallEndReason, CallId, CallMedia, CallPhase};
use whatsapp_rust::CallError;
use whatsapp_rust::types::call::{CallAction, CallEndedElsewhere, IncomingCall, MissedCall};
use whatsapp_rust::voip::audio::WA_SAMPLE_RATE;
use whatsapp_rust::voip::{CallEvent, CallHandle, CallTermination};
use whatsapp_rust::wacore::stanza::call::{REJECT_REASON_BUSY, REJECT_REASON_ENC};
use whatsapp_rust::wacore::voip_control::MediaCloseReason;

/// How many ended calls' WhatsApp ids are remembered, so a repeated or late
/// signal for one of them neither rings nor ends anything again.
const FINISHED: usize = 32;

/// How long a hang-up at shutdown may take to reach the other side.
const STOP_TERMINATE: Duration = Duration::from_secs(3);

/// A call the library is running: its handle, and the microphone and
/// speaker feeding it. Dropping `audio` closes both devices.
pub(super) struct Live {
    handle: CallHandle,
    audio: CallAudio,
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
        result: Result<Live, Failure>,
    },
    /// The library ended the call, or its media failed for good.
    Over { id: CallId, failed: bool },
    /// The microphone's state after a mute or unmute.
    Muted { id: CallId, muted: bool },
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
    /// Open the audio and call this chat.
    Place {
        id: CallId,
        chat: ChatId,
    },
    /// Follow the running call until the library ends it.
    Watch(CallId),
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
}

impl<H> Default for Calls<H> {
    fn default() -> Self {
        Self {
            next: 0,
            current: None,
            finished: VecDeque::new(),
        }
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

    fn state(call: &Call<H>, now: i64) -> Step<H> {
        Step::Emit(Event::CallState {
            call: call.id,
            chat: call.chat.clone(),
            phase: call.phase,
            since: now,
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
        steps
    }

    /// The person calls a chat.
    pub(super) fn place(&mut self, chat: ChatId, now: i64) -> Vec<Step<H>> {
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
        };
        let steps = vec![Self::state(&call, now), Step::Place { id, chat }];
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
        log::info!("call {}: {:?}", id.0, call.phase);
        let mut steps = vec![Self::state(call, now), Step::Watch(id)];
        if call.muted {
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
                log::info!("call {}: Connected", call.id.0);
                let mut steps = vec![Self::state(call, now)];
                if call.muted {
                    steps.push(Step::Mute {
                        id: call.id,
                        muted: true,
                    });
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
        log::info!("call {}: missed while offline", id.0);
        vec![Step::Emit(Event::CallEnded {
            call: id,
            chat,
            reason: CallEndReason::Missed,
        })]
    }

    /// Another of this account's devices answered or declined the call
    /// ringing here.
    pub(super) fn elsewhere(&mut self, protocol: &str, now: i64) -> Vec<Step<H>> {
        match self.by_protocol(protocol) {
            Some(call) if call.incoming() && call.phase != CallPhase::Connected => {
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
async fn open_audio() -> Result<(CallAudio, CallEndpoints), Failure> {
    let opened = tokio::task::spawn_blocking(|| CallAudio::start(WA_SAMPLE_RATE))
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

/// Follows a running call until it ends, and says whether it failed. This
/// is the call's one reader of its event queue.
async fn watch(handle: &CallHandle) -> bool {
    let events = handle.events();
    let ended = handle.wait_ended();
    tokio::pin!(ended);
    loop {
        tokio::select! {
            () = &mut ended => return false,
            event = events.recv() => match event {
                Ok(event) if ends_badly(&event) => return true,
                Ok(_) => {}
                Err(_) => {
                    ended.await;
                    return false;
                }
            },
        }
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
        let steps = self.calls.elsewhere(&ended.call_id, now());
        self.run_call_steps(steps);
    }

    pub(super) fn start_call(&mut self, chat: ChatId) {
        if !Self::jid_of(&chat).is_some_and(|jid| jid.is_pn() || jid.is_lid()) {
            self.emit(Event::Error(
                "Calls to groups and channels are not supported yet.".to_owned(),
            ));
            return;
        }
        let steps = self.calls.place(chat, now());
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

    pub(super) fn call_report(&mut self, report: Report) {
        let steps = match report {
            Report::Started { id, result } => {
                let result = match result {
                    Ok(live) => {
                        let protocol = live.handle.call_id().to_owned();
                        Ok((live, protocol))
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
        };
        self.run_call_steps(steps);
    }

    /// Ends the call before the connection stops, waiting briefly for the
    /// other side to be told.
    pub(super) async fn end_call_for_stop(&mut self) {
        for step in self.calls.shutdown(now()) {
            match step {
                Step::Terminate(Live { handle, audio }) => {
                    drop(audio);
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
    }

    fn run_call_steps(&mut self, steps: Vec<Step<Live>>) {
        for step in steps {
            self.run_call_step(step);
        }
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
                let sender = self.wa_sender.clone();
                tokio::spawn(async move {
                    let result = match open_audio().await {
                        Ok((audio, ends)) => client
                            .voip()
                            .accept(&offer)
                            .audio(ends.source, ends.sink)
                            .start()
                            .await
                            .map(|handle| Live { handle, audio })
                            .map_err(|error| setup_failure(&error)),
                        Err(failure) => Err(failure),
                    };
                    let _ = sender.send(RuntimeEvent::Call(Report::Started { id, result }));
                });
            }
            Step::Place { id, chat } => {
                let (Some(client), Some(peer)) = (self.client.clone(), Self::jid_of(&chat)) else {
                    self.call_not_connected(id);
                    return;
                };
                let sender = self.wa_sender.clone();
                tokio::spawn(async move {
                    let result = match open_audio().await {
                        Ok((audio, ends)) => client
                            .voip()
                            .call(&peer)
                            .audio(ends.source, ends.sink)
                            .start()
                            .await
                            .map(|handle| Live { handle, audio })
                            .map_err(|error| setup_failure(&error)),
                        Err(failure) => Err(failure),
                    };
                    let _ = sender.send(RuntimeEvent::Call(Report::Started { id, result }));
                });
            }
            Step::Watch(id) => {
                let Some(handle) = self.calls.live(id).map(|live| live.handle.clone()) else {
                    return;
                };
                let sender = self.wa_sender.clone();
                tokio::spawn(async move {
                    let failed = watch(&handle).await;
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
            Step::Terminate(Live { handle, audio }) => {
                drop(audio);
                tokio::spawn(async move {
                    match handle.terminate().await {
                        CallTermination::PeerNotified | CallTermination::AlreadyEnded => {}
                        _ => log::info!("a call ended without the other side confirming it"),
                    }
                });
            }
            Step::Release(Live { handle, audio }) => {
                drop(audio);
                tokio::spawn(async move { handle.hangup_local().await });
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
                Step::Place { id, chat } => format!("place {} {chat}", id.0),
                Step::Watch(id) => format!("watch {}", id.0),
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
                format!("watch {}", id.0)
            ]
        );
        id
    }

    fn dialled(calls: &mut Calls<Fake>, protocol: &str) -> CallId {
        let steps = calls.place(PEER.to_owned(), 10);
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
        calls.place(PEER.to_owned(), 10);
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
        let steps = calls.elsewhere("A", 20);
        assert_eq!(
            summary(&steps),
            [
                format!("state {} Ended 20", id.0),
                format!("ended {} {CALLER} EndedElsewhere", id.0),
            ],
            "nothing is declined: the phone already settled it"
        );
        assert!(!calls.busy());
        assert!(calls.elsewhere("A", 21).is_empty());
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
        let steps = calls.elsewhere("A", 21);
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
            [format!("state {} Connected 30", id.0)]
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
        calls.place(PEER.to_owned(), 10);
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
        calls.place(PEER.to_owned(), 10);
        assert_eq!(summary(&calls.place(CALLER.to_owned(), 11)), ["error"]);
        assert_eq!(calls.next, 1);
    }

    #[test]
    fn a_mute_before_the_answer_is_applied_again_once_connected() {
        let mut calls = Calls::<Fake>::default();
        calls.place(PEER.to_owned(), 10);
        let id = CallId(calls.next);
        assert_eq!(
            summary(&calls.set_muted(id, true)),
            [format!("muted {} true", id.0)]
        );
        let steps = calls.started(id, Ok((Fake(9), "P".to_owned())), 20);
        assert_eq!(summary(&steps)[2], format!("mute {} true", id.0));
        let steps = calls.signal("P", Signal::Accepted, 30);
        assert_eq!(summary(&steps)[1], format!("mute {} true", id.0));
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
        assert!(matches!(
            sent(&events).last(),
            Some(Event::CallEnded {
                reason: CallEndReason::EndedElsewhere,
                ..
            })
        ));
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
        assert_eq!(sent.len(), 1, "a repeated missed call is announced once");
        assert!(matches!(
            &sent[0],
            Event::CallEnded { chat, reason: CallEndReason::Missed, .. } if chat == CALLER
        ));
    }

    #[tokio::test]
    async fn a_call_without_a_connection_fails_at_once() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.start_call(PEER.to_owned());
        let placed = sent(&events);
        assert!(matches!(
            placed.first(),
            Some(Event::CallState {
                phase: CallPhase::Connecting,
                ..
            })
        ));
        assert!(matches!(
            placed.last(),
            Some(Event::CallEnded {
                reason: CallEndReason::Failed,
                ..
            })
        ));
        assert!(!worker.calls.busy());
        worker.start_call("120363000000000042@g.us".to_owned());
        assert!(matches!(sent(&events)[..], [Event::Error(_)]));
    }
}
