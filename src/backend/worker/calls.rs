//! The worker's side of a 1:1 call: commands, signaling routing, and media events.
//!
//! One call at a time. [`Worker::call`] holds the only one, and every entry point here refuses to
//! start or accept a second while it exists — a double click, a second chat's call button, and a
//! second inbound offer all land on the same refusal.
//!
//! The state the UI renders is never computed here: it comes from the backend. `<accept>` moves an
//! outgoing call out of dialing, the media plane's `RelayAllocated` is what makes a call Active and
//! starts the timer, and a `<reject>`/`<terminate>`/relay loss ends it with the reason the peer
//! gave. Nothing is inferred from the button that was pressed.

use super::*;
use crate::calls::{self, Call, CallOutcome, CallUpdate, VideoTick};
use whatsapp_rust::types::call::IncomingCall;
use whatsapp_rust::voip::{CallEvent, KeyframeUrgency};

/// The call this worker owns: its state, and the channels the tasks around it feed.
///
/// Dropped the moment the call reaches a terminal phase, which is what releases the media tasks,
/// the microphone and speaker streams and the camera thread.
pub(super) struct CallRuntime {
    pub(super) call: Call,
    /// Media-plane events, and the signal that the media task finished.
    pub(super) events: async_channel::Receiver<CallRuntimeEvent>,
    /// Video frames for the UI.
    pub(super) frames: Option<async_channel::Receiver<VideoTick>>,
    /// Whether the media-plane watcher was started for this call's handle.
    watching: bool,
    /// The snapshot the UI was last handed, so the periodic check publishes only real changes.
    last: CallUpdate,
}

/// What a call's background tasks report back to the worker loop.
pub(super) enum CallRuntimeEvent {
    /// One event off the call's media plane.
    Media(CallEvent),
    /// The media task is gone: relay disconnect, send failure, or hangup.
    Ended,
}

impl CallRuntime {
    /// A runtime whose media plane is not watched yet: a call that is still ringing has no handle
    /// to watch, and gets one the moment it is placed or answered.
    pub(super) fn new(call: Call, frames: Option<async_channel::Receiver<VideoTick>>) -> Self {
        let (_tx, events) = async_channel::bounded(64);
        let last = call.update();
        Self {
            call,
            events,
            frames,
            watching: false,
            last,
        }
    }

    /// Starts forwarding this call's media events and its media-finished signal, once.
    pub(super) fn watch(&mut self) {
        if self.watching {
            return;
        }
        let Some(handle) = self.call.handle() else {
            return;
        };
        self.watching = true;
        let (tx, events) = async_channel::bounded(64);
        self.events = events;
        tokio::spawn(async move {
            let media = handle.events();
            loop {
                tokio::select! {
                    event = media.recv() => match event {
                        Ok(event) => {
                            if tx.send(CallRuntimeEvent::Media(event)).await.is_err() {
                                return;
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(CallRuntimeEvent::Ended).await;
                            return;
                        }
                    },
                    _ = handle.wait_ended() => {
                        let _ = tx.send(CallRuntimeEvent::Ended).await;
                        return;
                    }
                }
            }
        });
    }
}

impl Worker {
    // -----------------------------------------------------------------------
    // Command entry points
    // -----------------------------------------------------------------------

    /// Hands the call screen the microphones, speakers and cameras it can offer, and keeps them:
    /// they are how a device that goes away is named by the description the user saw.
    ///
    /// The list returned is the one already published, so no handler ever waits on the machine:
    /// a fresh scan is started off the loop and lands later through [`Self::call_devices_arrived`].
    pub(super) fn emit_call_devices(&mut self) -> calls::DeviceList {
        self.begin_device_discovery();
        self.call_devices.clone()
    }

    /// Starts a device scan off the worker's loop.
    ///
    /// Enumerating opens every audio device to name it and probes each camera node, so it runs on a
    /// blocking thread rather than on the async loop. The result is sent back as
    /// [`Command::CallDevices`], tagged with the current call generation, so a scan that finishes
    /// after the call it belonged to has ended or been replaced is discarded rather than
    /// overwriting the newer list. Nothing here is awaited: a slow or stuck driver cannot hold up
    /// signaling or the commands that share the loop.
    pub(super) fn begin_device_discovery(&mut self) {
        let generation = self.call_generation;
        let commands = self.commands.clone();
        tokio::task::spawn_blocking(move || {
            let devices = calls::devices();
            let _ = commands.send(Command::CallDevices {
                generation,
                devices: Box::new(devices),
            });
        });
    }

    /// Applies one finished device scan and publishes it.
    ///
    /// `generation` is the call state the scan was started for. A scan for a call that has since
    /// ended or been replaced is dropped here, so its result cannot overwrite the list a newer call
    /// already published.
    pub(super) async fn call_devices_arrived(
        &mut self,
        generation: u64,
        devices: calls::DeviceList,
    ) {
        if !discovery_is_current(generation, self.call_generation) {
            log::debug!("[CALL] discarding a device scan for a call that has moved on");
            return;
        }
        // Read before the fresh list replaces it: a device that just went away is still named in
        // here by the description the user saw.
        let known = self.call_devices.clone();
        if devices != known {
            self.publish_call_devices(devices.clone());
        }
        let mut lost = Vec::new();
        if let Some(runtime) = self.call.as_mut() {
            lost.extend(runtime.call.take_stream_fallbacks());
            lost.extend(runtime.call.verify_devices(&devices).await);
            if runtime.call.camera_stalled()
                && let Err(error) = runtime.call.set_camera(false).await
            {
                log::warn!("[CALL] the peer was not told the camera stopped: {error}");
            }
        }
        if !lost.is_empty() {
            // Worded by the call screen, which knows the reader's language; the log says it plainly.
            let lost: Vec<calls::LostDevice> = lost
                .into_iter()
                .map(|(kind, id)| {
                    let name = calls::device_name(&known, kind, &id);
                    log::warn!("[CALL] {kind:?} \"{name}\" is not available any more");
                    calls::LostDevice { kind, name }
                })
                .collect();
            if let Some(runtime) = self.call.as_mut() {
                runtime.call.set_lost_devices(lost);
            }
        }
        // Compared against the snapshot the UI was handed, so a camera that stopped on its own or a
        // mute another device set reaches the screen even though no user pressed anything.
        let changed = self.call.as_mut().and_then(|runtime| {
            let current = runtime.call.update();
            let changed = current != runtime.last;
            runtime.last = current.clone();
            changed.then_some(current)
        });
        if let Some(update) = changed {
            self.emit_call(update);
        }
    }

    /// Stores and publishes a device list, so the pickers and the fallback naming share one
    /// snapshot of the machine.
    pub(super) fn publish_call_devices(&mut self, devices: calls::DeviceList) {
        self.call_devices = devices.clone();
        self.emit(Event::CallDevices(Box::new(devices)));
    }

    /// Re-sends the current call after privacy recovery, since call updates are withheld while the
    /// lock state is unknown.
    ///
    /// Nothing is rediscovered: the last published list is what the picker already had, and the
    /// runtime's own snapshot is what the surface should draw. The chat can finally be judged
    /// private or not, so a call that arrived mid-recovery rings behind the right lock state rather
    /// than being dropped for the whole recovery.
    pub(super) fn replay_call(&mut self) {
        let Some(runtime) = self.call.as_ref() else {
            return;
        };
        let devices = self.call_devices.clone();
        let update = runtime.last.clone();
        self.publish_call_devices(devices);
        self.emit_call(update);
    }

    /// Publishes one call state, and lets the call go once it reaches a terminal phase.
    ///
    /// The UI keeps rendering the snapshot it was handed; what is released here is the media, so a
    /// finished call holds no child process and no task.
    pub(super) fn emit_call(&mut self, update: CallUpdate) {
        let finished = !update.phase.is_live();
        // Read before the runtime is let go, and before the update is published: the record is the
        // call's own account of itself, and once this returns there is nothing left to ask.
        let record = finished
            .then(|| {
                self.call
                    .as_ref()
                    .filter(|runtime| runtime.call.generation() == update.generation)
                    .and_then(|runtime| runtime.call.record())
            })
            .flatten();
        // Remembered so the periodic check can tell a real change from a snapshot it already sent.
        if let Some(runtime) = self.call.as_mut()
            && runtime.call.generation() == update.generation
        {
            runtime.last = update.clone();
        }
        self.emit(Event::Call(Box::new(update)));
        if finished {
            // The call is over, so any device scan still in flight belongs to a call that is gone:
            // moving the generation on makes it stale, and it is dropped when it lands.
            self.call_generation = self.call_generation.wrapping_add(1);
            self.call = None;
        }
        if let Some(record) = record {
            self.log_call(record);
        }
    }

    /// Writes one finished call to the log and tells the interface it is there.
    ///
    /// A log that cannot be written is a warning, not an error the user needs: the call is over
    /// either way, and the next one is unaffected.
    fn log_call(&mut self, record: crate::model::CallRecord) {
        // Deliberately without the chat: a record's chat id is the peer's phone number, and this log
        // ships. What a call became is what a report needs.
        log::info!(
            "[CALL] history direction={:?} media={:?} status={:?} duration={}s",
            record.direction,
            record.media,
            record.status,
            record.duration
        );
        if let Err(error) = self.archive.save_call(&record) {
            log::warn!("[CALL] the call could not be written to the log: {error}");
            return;
        }
        self.emit(Event::CallLogged(Box::new(record)));
    }

    /// Whether this worker already holds a runtime for a call id, which decides whether a terminal
    /// event resolves a live call or describes one that never rang here.
    pub(super) fn owns_call(&self, call_id: &str) -> bool {
        self.call
            .as_ref()
            .is_some_and(|runtime| runtime.call.call_id() == call_id)
    }

    /// This account's own chat id, so a group roster can leave its own participant out of the grid.
    pub(super) fn own_chat(&self) -> Option<String> {
        self.me_lid.clone().or_else(|| self.me_pn.clone())
    }

    /// Whether a call is already up, which every entry point refuses to double.
    pub(super) fn call_busy(&mut self) -> bool {
        if self.call.is_some() {
            log::warn!("[CALL] refusing a new call: one is already up");
            return true;
        }
        false
    }

    pub(super) async fn start_call(&mut self, chat: ChatId, video: bool) {
        if self.call_busy() {
            return;
        }
        let group = group_chat(&chat);
        if !callable_chat(&chat) && !group {
            log::warn!("[CALL] refusing a call to a chat that is not one to one or a group");
            self.emit(Event::Error(fault(
                self.locale,
                "Calls are one to one or group only",
            )));
            return;
        }
        let self_chat = self.own_chat();
        let Some(client) = self.client.clone() else {
            log::warn!("[CALL] cannot start a call: WhatsApp is not connected");
            self.emit(Event::Error(fault(
                self.locale,
                "ZapFast is not connected to WhatsApp",
            )));
            return;
        };
        // The devices the settings remember, checked against the machine first: a headset that was
        // switched off since the last call falls back to the system default and says so, instead of
        // opening a stream that can never deliver a frame. Discovery runs off the loop and lands
        // later, so the call opens with the list the pickers already show.
        let devices = self.emit_call_devices();
        let wanted = self.call_defaults.clone();
        let resolved =
            calls::resolve_devices(&devices, wanted.microphone, wanted.speaker, wanted.camera);
        let placed = if group {
            Call::place_group(
                &client,
                chat.to_string(),
                video,
                resolved.microphone,
                resolved.speaker,
                resolved.camera,
                self_chat,
            )
            .await
        } else {
            Call::place(
                &client,
                chat.to_string(),
                video,
                resolved.microphone,
                resolved.speaker,
                resolved.camera,
            )
            .await
        };
        match placed {
            Ok((mut call, frames)) => {
                call.set_lost_devices(resolved.lost_devices);
                let mut runtime = CallRuntime::new(call, frames);
                runtime.watch();
                let update = runtime.call.update();
                self.call = Some(runtime);
                self.emit_call(update);
            }
            Err(error) => {
                log::error!("[CALL] could not start the call: {error}");
                self.emit(Event::Error(
                    fault(self.locale, "The call could not be started: {error}")
                        .replace("{error}", &error.to_string()),
                ));
            }
        }
    }

    pub(super) async fn answer_call(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        // The list the pickers already show, refreshed off the loop: a stuck driver cannot hold the
        // worker inside the answer while signaling waits behind it.
        let devices = self.emit_call_devices();
        let update = match self.call.as_mut() {
            Some(runtime) => {
                // Whatever the user picked while the phone was ringing is what the media binds to,
                // checked against the machine so one that vanished falls back rather than opening a
                // stream that can never deliver.
                let (microphone, speaker, camera) = runtime.call.selections();
                let resolved = calls::resolve_devices(&devices, microphone, speaker, camera);
                let answered = runtime
                    .call
                    .answer(
                        &client,
                        resolved.microphone,
                        resolved.speaker,
                        resolved.camera,
                    )
                    .await;
                match answered {
                    Ok(frames) => {
                        if frames.is_some() {
                            runtime.frames = frames;
                        }
                        runtime.call.set_lost_devices(resolved.lost_devices);
                        runtime.watch();
                        Some(runtime.call.update())
                    }
                    Err(error) => {
                        log::error!("[CALL] could not answer the call: {error}");
                        self.emit(Event::Error(
                            fault(self.locale, "The call could not be answered: {error}")
                                .replace("{error}", &error.to_string()),
                        ));
                        return;
                    }
                }
            }
            None => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    pub(super) async fn decline_call(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let update = match self.call.as_mut() {
            Some(runtime) => match runtime.call.decline(&client).await {
                Ok(()) => Some(runtime.call.update()),
                Err(error) => {
                    log::error!("[CALL] could not decline the call: {error}");
                    return;
                }
            },
            None => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    pub(super) async fn hangup_call(&mut self) {
        let client = self.client.clone();
        let update = match self.call.as_mut() {
            Some(runtime) => Some(runtime.call.hangup(client.as_ref()).await),
            None => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    pub(super) async fn set_call_muted(&mut self, muted: bool) {
        let update = match self.call.as_mut() {
            Some(runtime) => runtime.call.set_muted(muted).await,
            None => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    pub(super) async fn set_call_camera(&mut self, on: bool) {
        let outcome = match self.call.as_mut() {
            Some(runtime) => {
                if on && !runtime.call.is_video() {
                    // A voice call: ask the peer for video and start draining the frames it brings,
                    // from the camera the settings remember. A camera has no system default, so the
                    // stored `None` — the picker's "Default device" — is read here as the first
                    // camera the machine reports, exactly as a call's startup reads it. Passing it
                    // through unresolved made the upgrade fail on a fresh install even with a
                    // working camera attached. The list is already in hand, so nothing is enumerated
                    // here: the list is cached by `reconcile_call` and `call_devices`.
                    let camera = calls::default_camera(
                        &self.call_devices,
                        self.call_defaults.camera.clone(),
                    );
                    match runtime.call.upgrade_to_video(camera).await {
                        Ok(frames) => {
                            if frames.is_some() {
                                runtime.frames = frames;
                            }
                            Ok(runtime.call.update())
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    // Turning a video call's camera back on. A camera the machine lost cleared the
                    // selection (`verify_devices`), so the picker's "Default device" is read again
                    // here, or the resume would open `None` and fail. A camera the call still holds
                    // is kept as it is.
                    if on
                        && runtime.call.camera().is_none()
                        && let Some(camera) = calls::default_camera(
                            &self.call_devices,
                            self.call_defaults.camera.clone(),
                        )
                    {
                        let _ = runtime.call.set_camera_device(Some(camera)).await;
                    }
                    runtime.call.set_camera(on).await
                }
            }
            None => return,
        };
        match outcome {
            Ok(update) => self.emit_call(update),
            Err(error) => {
                log::error!("[CALL] the camera could not be toggled: {error}");
                self.emit(Event::Error(error.to_string()));
            }
        }
    }

    pub(super) fn set_call_microphone(&mut self, device: Option<String>) {
        // The picker is also the preference: the next call opens where this one was left.
        self.call_defaults.microphone = device.clone();
        let update = self
            .call
            .as_mut()
            .map(|runtime| runtime.call.set_microphone(device));
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    pub(super) fn set_call_speaker(&mut self, device: Option<String>) {
        self.call_defaults.speaker = device.clone();
        let update = self
            .call
            .as_mut()
            .map(|runtime| runtime.call.set_speaker(device));
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    pub(super) async fn set_call_camera_device(&mut self, device: Option<String>) {
        self.call_defaults.camera = device.clone();
        // `None` is the picker's "Default device". A camera has no system default, so it is read
        // as the first camera the machine reports — the same reading a call's startup applies.
        // Passing the bare `None` through stopped the running camera and then had no node to open,
        // so choosing the default left the call with no picture until the next call.
        let device = calls::default_camera(&self.call_devices, device);
        let outcome = match self.call.as_mut() {
            Some(runtime) => Some(runtime.call.set_camera_device(device).await),
            None => None,
        };
        match outcome {
            Some(Ok(update)) => self.emit_call(update),
            Some(Err(error)) => {
                log::error!("[CALL] the camera could not be switched: {error}");
                self.emit(Event::Error(error.to_string()));
            }
            None => {}
        }
    }

    // -----------------------------------------------------------------------
    // Backend events
    // -----------------------------------------------------------------------

    /// One event from the current call's media plane.
    pub(super) fn call_runtime(&mut self, event: CallRuntimeEvent) {
        let update = match self.call.as_mut() {
            Some(runtime) => match event {
                CallRuntimeEvent::Ended => runtime.call.media_ended(),
                CallRuntimeEvent::Media(media) => {
                    if matches!(media, CallEvent::RelayAllocated)
                        && runtime.call.is_video()
                        && let Some(handle) = runtime.call.handle()
                    {
                        // A decoder cannot start on a delta frame, so ask for an IDR at once.
                        handle.request_peer_keyframe(KeyframeUrgency::Immediate);
                    }
                    runtime.call.media(&media)
                }
            },
            None => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    /// One video frame from the current call, straight to the UI.
    pub(super) fn call_frame(&mut self, tick: VideoTick) {
        match tick {
            VideoTick::Local(image) => {
                self.emit(Event::CallVideo {
                    local: Some(image),
                    remote: None,
                    remote_from: None,
                });
            }
            VideoTick::Remote(image) => {
                let first = self
                    .call
                    .as_mut()
                    .is_some_and(|runtime| runtime.call.saw_remote_video());
                // The flag is what the UI gates the picture on, so a frame that arrives after the
                // peer said their video stopped is not drawn: it is a picture of a stream that has
                // ended, and showing it would contradict the state the peer just sent.
                let still_sending = self
                    .call
                    .as_ref()
                    .is_none_or(|runtime| runtime.call.peer_video_visible());
                if still_sending {
                    self.emit(Event::CallVideo {
                        local: None,
                        remote: Some(image),
                        remote_from: None,
                    });
                }
                if first && let Some(runtime) = self.call.as_ref() {
                    let update = runtime.call.update();
                    self.emit(Event::Call(Box::new(update)));
                }
            }
            // A group participant's own picture, named by the sender. The frame is drawn while the
            // call is a group; the interface prunes a tile once the roster drops its participant or
            // that participant's camera state says they stopped sending, so nothing stale lingers.
            VideoTick::RemoteFrom { chat, image } => {
                let draw = self
                    .call
                    .as_mut()
                    .is_some_and(|runtime| runtime.call.participant_video_arrived(&chat));
                if draw {
                    self.emit(Event::CallVideo {
                        local: None,
                        remote: None,
                        remote_from: Some((chat, image)),
                    });
                }
            }
        }
    }

    /// Routes one inbound `<call>` stanza: either it belongs to the call we own, or it is a new
    /// offer that should ring.
    ///
    /// This is where an outgoing call learns it was answered, rejected, or ended, so the phase the
    /// UI shows is the peer's, not the dialer's.
    pub(super) async fn call_signaling(&mut self, incoming: &IncomingCall) {
        let action = &incoming.action;
        let mine = self
            .call
            .as_ref()
            .is_some_and(|runtime| runtime.call.call_id() == action.call_id());
        if mine {
            let update = self
                .call
                .as_mut()
                .and_then(|runtime| runtime.call.signaling(action));
            if let Some(update) = update {
                self.emit_call(update);
            }
            return;
        }
        if !calls::is_offer(action) {
            return;
        }
        if self.call_busy() {
            // Refuse it to the caller rather than dropping it. An offer that is only dropped leaves
            // the other side ringing until its own protocol timeout, believing this device might
            // still answer; a real `<reject>` ends it there and then. The call that is already up
            // is untouched: this is the second offer's own call id being refused, not ours.
            match self.client.as_ref() {
                Some(client) => match client.voip().reject(incoming).await {
                    Ok(()) => log::info!(
                        "[CALL] refused a second offer while one is up call_id={}",
                        action.call_id()
                    ),
                    Err(error) => {
                        log::warn!("[CALL] a second offer could not be refused: {error}")
                    }
                },
                None => log::warn!("[CALL] no client to refuse a second offer with"),
            }
            return;
        }
        let chat = self.canonical(&incoming.from);
        // A group, broadcast or newsletter offer never reaches the 1:1 call surface. The interface
        // hides the buttons for those chats, but the worker is the boundary: a non-direct offer is
        // refused here rather than rung, so no call is created and no history entry is written.
        //
        // Refuse it to the caller instead of only dropping it: a dropped offer leaves the other
        // side ringing until its own protocol timeout, believing this device might still answer,
        // while a real `<reject>` ends it there and then. Only a 1:1 offer rings, so an offer that
        // reaches here is one the caller is told we will not take.
        let video = calls::offer_is_video(action);
        let self_chat = self.own_chat();
        // A group invite rings like a one-to-one offer: the room's own JID becomes the call's chat,
        // and the roster arrives once the user joins. Only a broadcast, a channel or a newsletter
        // —a non-direct offer that is not a group—is refused, because there is no call to join.
        let group = calls::offer_group(action);
        if !callable_chat(&chat) && group.is_none() {
            match self.client.as_ref() {
                Some(client) => match client.voip().reject(incoming).await {
                    Ok(()) => log::warn!(
                        "[CALL] refused an incoming offer from a chat that is not one to one"
                    ),
                    Err(error) => {
                        log::warn!("[CALL] a non-direct offer could not be refused: {error}")
                    }
                },
                None => log::warn!("[CALL] no client to refuse a non-direct offer with"),
            }
            return;
        }
        // The remembered devices are pre-selected on the prompt, so answering picks up right where
        // the last call left off.
        let call = match group {
            Some(group) => Call::ringing_group(
                group,
                Box::new(incoming.clone()),
                video,
                self.call_defaults.clone(),
                self_chat,
            ),
            None => Call::ringing(
                chat,
                Box::new(incoming.clone()),
                video,
                self.call_defaults.clone(),
            ),
        };
        let update = call.update();
        self.call = Some(CallRuntime::new(call, None));
        self.emit_call_devices();
        self.emit_call(update);
    }

    /// A call we were ringing or talking on was resolved on another of the account's devices, or
    /// the caller gave up before anyone answered.
    ///
    /// `outcome` is what the terminal event says became of the call; `None` is the engine's own
    /// reading of a ringing call that nobody here picked up. It only matters when there is a live
    /// runtime to correct — see [`Self::record_resolved_without_runtime`] for the replayed case.
    pub(super) fn call_resolved(&mut self, call_id: &str, outcome: Option<CallOutcome>) {
        let update = match self.call.as_mut() {
            Some(runtime) if runtime.call.call_id() == call_id => {
                runtime.call.resolved_elsewhere(outcome)
            }
            _ => None,
        };
        if let Some(update) = update {
            self.emit_call(update);
        }
    }

    /// Writes a call that was resolved with no live runtime here.
    ///
    /// An offer the server replayed from the offline queue never rang on this device, so there is no
    /// runtime to resolve; without this the call is simply dropped and the supposedly complete
    /// durable log never learns it was missed. The record is built from the event's own `from`,
    /// `call_id`, and timestamp, and only for a one-to-one chat — the same boundary the ringing path
    /// keeps, so a group or channel offer replayed offline writes nothing.
    pub(super) fn record_resolved_without_runtime(
        &mut self,
        from: &Jid,
        call_id: &str,
        at: i64,
        outcome: CallOutcome,
    ) {
        let chat = self.canonical(from);
        if !callable_chat(&chat) {
            log::warn!("[CALL] ignoring a resolved call from a chat that is not one to one");
            return;
        }
        let record = crate::model::CallRecord {
            id: call_id.to_owned(),
            chat,
            started_at: at,
            ended_at: at,
            direction: crate::model::CallDirection::Incoming,
            media: crate::model::CallMedia::Voice,
            status: outcome.status(),
            duration: 0,
            participants: 0,
        };
        self.log_call(record);
    }

    /// The whole call log, newest first.
    ///
    /// Held back until the archive's privacy recovery finishes: a call record names the chat, and
    /// a locked chat's rows must not reach the Calls view while the lock state is still unknown.
    /// `reveal_private_content` re-issues this read once recovery completes, so a request made at
    /// startup is not simply dropped.
    pub(super) fn load_calls(&mut self) {
        if !self.privacy_ready {
            return;
        }
        match self.archive.calls() {
            Ok(calls) => self.emit(Event::CallLog(Box::new(calls))),
            Err(error) => log::warn!("[CALL] the call log could not be read: {error}"),
        }
    }

    /// One chat's calls, newest first, for the entries inside that conversation.
    pub(super) fn load_chat_calls(&mut self, chat: ChatId) {
        // Same boundary as the whole log: a chat's call rows are archive-derived private content.
        if !self.privacy_ready {
            return;
        }
        match self.archive.calls_for_chat(&chat) {
            Ok(calls) => self.emit(Event::ChatCalls {
                chat,
                calls: Box::new(calls),
            }),
            Err(error) => log::warn!("[CALL] a chat's call log could not be read: {error}"),
        }
    }

    /// Releases the call on the way out, so quitting leaves no child process or task behind, and
    /// records how the call ended rather than dropping it from the history.
    pub(super) async fn shutdown_call(&mut self) {
        if let Some(mut runtime) = self.call.take() {
            runtime.call.hangup(self.client.as_ref()).await;
            // Quitting during a ringing or active call still writes its ending: `hangup` set the
            // terminal phase, and the record is the call's own account of itself.
            if let Some(record) = runtime.call.record() {
                self.log_call(record);
            }
        }
    }

    /// Keeps a live call's picture of the machine honest.
    ///
    /// A device the user picked can disappear mid-call — a Bluetooth headset switching off, a camera
    /// unplugged, a sound card taken over by another app — and nothing here ends the call over it.
    /// The stream behind it is rebound to the system default, a camera that stopped is stopped for
    /// the peer as well, and the snapshot carries the reason, so the picker stops claiming a device
    /// that is not there.
    ///
    /// The fresh list is also published when it changed, so a device that came or went shows up in
    /// the pickers without anyone pressing anything.
    pub(super) fn reconcile_call(&mut self) {
        if self
            .call
            .as_ref()
            .is_none_or(|runtime| !runtime.call.phase().is_live())
        {
            return;
        }
        // Once per heartbeat, the engine's own counters for this call. A call that is up but carrying
        // no audio is otherwise indistinguishable in a bug report from one where nobody spoke, and
        // these numbers are what tell the two apart.
        if let Some(runtime) = self.call.as_ref() {
            runtime.call.log_media_stats();
        }
        // A fresh scan, off the loop. Its result lands in [`Self::call_devices_arrived`], where a
        // device that came or went is published and a live call's streams are checked against it.
        self.begin_device_discovery();
    }
}

/// Whether a finished device scan still belongs to the call it was started for.
///
/// The generation only moves when a call ends, so every scan started while one call is up shares
/// its generation and is applied, while a scan that lands after that call ended is dropped rather
/// than overwriting the list a newer call already published.
fn discovery_is_current(scan_generation: u64, call_generation: u64) -> bool {
    scan_generation == call_generation
}

/// Whether a chat can be called at all.
///
/// The interface only draws the phone and camera buttons on a one-to-one chat, but the worker is the
/// boundary rather than the interface: a group, a channel or a broadcast list reaching the 1:1
/// builder would be refused by the protocol at best and misbehave at worst, so the JID is checked
/// here as well.
fn callable_chat(chat: &str) -> bool {
    matches!(
        crate::model::ChatKind::from_id(chat),
        crate::model::ChatKind::Direct
    )
}

/// Whether a chat is a group, which can carry a group call.
fn group_chat(chat: &str) -> bool {
    matches!(
        crate::model::ChatKind::from_id(chat),
        crate::model::ChatKind::Group
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::types::call::CallAction;

    #[test]
    fn only_a_one_to_one_chat_can_be_called() {
        assert!(callable_chat("15551234567@s.whatsapp.net"));
        assert!(callable_chat("123456789012345@lid"));
        assert!(!callable_chat("12345-67890@g.us"));
        assert!(!callable_chat("1234567890@broadcast"));
        assert!(!callable_chat("1234567890@newsletter"));
    }

    /// A device scan is applied to the call it was started for, and only to that one: a scan that
    /// lands after the call ended must not overwrite the newer list.
    #[test]
    fn a_device_scan_only_applies_to_the_call_it_was_started_for() {
        assert!(discovery_is_current(4, 4));
        // The call ended and the generation moved on while the scan was in flight.
        assert!(!discovery_is_current(4, 5));
        // A scan started against an older call can never match a newer one.
        assert!(!discovery_is_current(3, 4));
    }

    #[test]
    fn a_group_chat_is_a_group_and_nothing_else_is() {
        assert!(group_chat("12345-67890@g.us"));
        assert!(!group_chat("15551234567@s.whatsapp.net"));
        assert!(!group_chat("123456789012345@lid"));
        assert!(!group_chat("1234567890@broadcast"));
        assert!(!group_chat("1234567890@newsletter"));
    }

    /// A group offer names the room it belongs to; a one-to-one offer names no room, because its
    /// chat is the caller. That is the whole difference the worker needs to ring one or refuse the
    /// other, so it is pinned here with both offers built in memory.
    #[test]
    fn a_group_offer_says_which_group_and_a_one_to_one_offer_says_none() {
        let caller: Jid = "15551234567@s.whatsapp.net".parse().expect("a caller jid");
        let group: Jid = "120363000000000000@g.us".parse().expect("a group jid");
        let group_offer = CallAction::Offer {
            call_id: "a-call".to_owned(),
            call_creator: caller.clone(),
            caller_pn: None,
            caller_country_code: None,
            device_class: None,
            joinable: false,
            is_video: false,
            audio: Vec::new(),
            group_jid: Some(group),
        };
        assert_eq!(
            calls::offer_group(&group_offer).as_deref(),
            Some("120363000000000000@g.us")
        );
        let direct_offer = CallAction::Offer {
            call_id: "a-call".to_owned(),
            call_creator: caller,
            caller_pn: None,
            caller_country_code: None,
            device_class: None,
            joinable: false,
            is_video: false,
            audio: Vec::new(),
            group_jid: None,
        };
        assert_eq!(calls::offer_group(&direct_offer), None);
    }
}
