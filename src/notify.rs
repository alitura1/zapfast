//! Desktop notifications when the app is hidden, unfocused, or on another chat.
//!
//! Delivery uses the platform notification service. Each notification runs on
//! its own thread because delivery and click handling can block. A click hands
//! back the chat and the message it announced.

use crate::settings::NotificationSound;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
mod badge;
#[cfg(target_os = "windows")]
mod badge_windows;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
pub use badge::Badge;
#[cfg(target_os = "windows")]
pub use badge_windows::{Badge, Taskbar};

/// A no-op taskbar badge on platforms without a taskbar badge implementation.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
#[derive(Default)]
pub struct Badge;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
impl Badge {
    /// Does nothing; no supported desktop here reads a taskbar badge.
    pub fn set(&mut self, _count: u32) {}
}

/// What a clicked notification opens: the chat, and the message it announced.
///
/// The message id travels with the click, so the reader lands on what was announced instead of on
/// the end of the chat. An incoming call has no message to land on, so its target carries `None`
/// and the click only has to bring the window up, where the call is already on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationTarget {
    pub chat: String,
    pub message: Option<String>,
}

#[cfg(any(target_os = "macos", test))]
const MACOS_APPLICATION_ID: &str = "me.paolino.fastsapp";

#[cfg(target_os = "macos")]
fn macos_application_ready() -> bool {
    static READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *READY.get_or_init(|| {
        // The library's implicit default looks up an app named "use_default"
        // through AppleScript, which opens macOS's application chooser.
        match notify_rust::set_application(MACOS_APPLICATION_ID) {
            Ok(()) => true,
            Err(error) => {
                log::debug!("could not initialize notification application: {error}");
                false
            }
        }
    })
}

/// Notifications that may still wait for a click. Each one waiting holds a
/// thread, a runtime, and a D-Bus connection, about five file descriptors,
/// and the desktop may never say it closed: KDE Plasma files notifications
/// that arrive during Do Not Disturb (on by default while sharing the screen)
/// straight into its history without a signal. Without a limit they piled up
/// until the process ran out of descriptors and aborted.
const WAITING_LIMIT: usize = 32;

/// How a notification stops waiting for a click.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    /// The chat was read: take the notification off the desktop.
    Close,
    /// Newer notifications took its place: leave it on the desktop, but stop
    /// waiting, so a click on it no longer opens the chat.
    Release,
}

/// What a notification still to be shown was told meanwhile: `None` when its
/// chat was read (don't show it), else whether it may wait for a click. A
/// notification released in a burst before it appeared is still shown.
fn before_showing(cancelled: &mut tokio::sync::oneshot::Receiver<Stop>) -> Option<bool> {
    match cancelled.try_recv() {
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => Some(true),
        Ok(Stop::Release) => Some(false),
        Ok(Stop::Close) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => None,
    }
}

/// Cancellation is registered before delivery starts, so reading a chat while
/// its notification is still being delivered cannot leave a stale notification.
#[derive(Default)]
pub struct Notifications {
    pending: std::collections::HashMap<String, Vec<Waiting>>,
    /// Order of registration, so the oldest waiting notification is released first.
    registered: u64,
}

/// One notification waiting on screen.
struct Waiting {
    /// Registration order, so the oldest is released first.
    order: u64,
    /// Whether this is an incoming-call notification, which is taken back by its own rule rather
    /// than by the chat's: answering a call must not close an unrelated message notification that
    /// happens to be waiting for the same chat.
    call: bool,
    cancel: tokio::sync::oneshot::Sender<Stop>,
}

impl Notifications {
    fn register(&mut self, chat: &str, call: bool) -> tokio::sync::oneshot::Receiver<Stop> {
        self.pending.retain(|_, entries| {
            entries.retain(|entry| !entry.cancel.is_closed());
            !entries.is_empty()
        });
        while self.pending.values().map(Vec::len).sum::<usize>() >= WAITING_LIMIT {
            self.release_oldest();
        }
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        self.registered += 1;
        self.pending
            .entry(chat.to_owned())
            .or_default()
            .push(Waiting {
                order: self.registered,
                call,
                cancel,
            });
        cancelled
    }

    fn release_oldest(&mut self) {
        let oldest = self
            .pending
            .iter()
            .flat_map(|(chat, entries)| entries.iter().map(move |entry| (entry.order, chat)))
            .min()
            .map(|(order, chat)| (order, chat.clone()));
        let Some((order, chat)) = oldest else {
            return;
        };
        if let Some(entries) = self.pending.get_mut(&chat) {
            if let Some(index) = entries.iter().position(|entry| entry.order == order) {
                let entry = entries.remove(index);
                let _ = entry.cancel.send(Stop::Release);
            }
            if entries.is_empty() {
                self.pending.remove(&chat);
            }
        }
    }

    pub fn clear(&mut self, chat: &str) {
        if let Some(entries) = self.pending.remove(chat) {
            for entry in entries {
                let _ = entry.cancel.send(Stop::Close);
            }
        }
    }

    /// Takes back only this chat's incoming-call notification, leaving its message notifications
    /// alone.
    ///
    /// Answering or giving up a call is what takes its own notification down, and a message that
    /// arrived while it was ringing is a separate thing the reader has not seen: closing the whole
    /// chat's notifications made answering a call quietly swallow that message's notification too.
    pub fn clear_call(&mut self, chat: &str) {
        let Some(entries) = self.pending.get_mut(chat) else {
            return;
        };
        // The call's own senders are taken out and told to close; dropping them instead would close
        // their channels without a `Stop`, which the delivery thread reads as "already gone" rather
        // than "take this one down".
        let mut taken = Vec::new();
        for entry in std::mem::take(entries) {
            if entry.call {
                taken.push(entry.cancel);
            } else {
                entries.push(entry);
            }
        }
        for cancel in taken {
            let _ = cancel.send(Stop::Close);
        }
        if entries.is_empty() {
            self.pending.remove(chat);
        }
    }

    pub fn clear_all(&mut self) {
        self.pending.clear();
    }

    /// Shows a notification; platform delivery runs outside the interface thread.
    #[expect(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        title: String,
        body: String,
        picture: Option<PathBuf>,
        sound: NotificationSound,
        target: NotificationTarget,
        opened: Arc<Mutex<Vec<NotificationTarget>>>,
        wake: impl Fn() + Send + 'static,
    ) {
        // A call notification carries no message, which is what tells it apart from a message
        // notification for the same chat when the call is answered.
        let call = target.message.is_none();
        let cancelled = self.register(&target.chat, call);
        let spawned = std::thread::Builder::new()
            .name("notification".into())
            .spawn(move || {
                let system_sound = sound == NotificationSound::System;
                play_sound(sound);
                deliver(
                    &title,
                    &body,
                    picture.as_deref(),
                    system_sound,
                    target,
                    opened,
                    wake,
                    cancelled,
                )
            });
        if let Err(error) = spawned {
            log::debug!("no thread for a notification: {error}");
        }
    }
}

/// Pidgin's message and alert sounds (GPL-2.0, see `assets/sounds/README.md`).
const RECEIVE: &[u8] = include_bytes!("../assets/sounds/receive.wav");
const ALERT: &[u8] = include_bytes!("../assets/sounds/alert.wav");

/// Plays a notification sound on its own thread, for notifications and
/// their preview in Settings. System sounds and silence play nothing here.
pub fn play_sound(sound: NotificationSound) {
    let source: Box<dyn Fn() -> std::io::Result<Box<dyn ReadSeek>> + Send> = match sound {
        NotificationSound::Receive => Box::new(|| Ok(Box::new(std::io::Cursor::new(RECEIVE)))),
        NotificationSound::Alert => Box::new(|| Ok(Box::new(std::io::Cursor::new(ALERT)))),
        NotificationSound::Custom(path) => Box::new(move || {
            Ok(Box::new(std::io::BufReader::new(std::fs::File::open(
                &path,
            )?)))
        }),
        NotificationSound::System | NotificationSound::None => return,
    };
    let spawned = std::thread::Builder::new()
        .name("notification-sound".into())
        .spawn(move || {
            let played = (|| -> Result<(), String> {
                let reader = source().map_err(|error| error.to_string())?;
                let decoder = rodio::Decoder::new(reader).map_err(|error| error.to_string())?;
                let device = rodio::DeviceSinkBuilder::open_default_sink()
                    .map_err(|error| error.to_string())?;
                let player = rodio::Player::connect_new(device.mixer());
                player.append(decoder);
                player.sleep_until_end();
                Ok(())
            })();
            if let Err(error) = played {
                log::debug!("notification sound not played: {error}");
            }
        });
    if let Err(error) = spawned {
        log::debug!("no thread for a notification sound: {error}");
    }
}

trait ReadSeek: std::io::Read + std::io::Seek + Send + Sync {}
impl<T: std::io::Read + std::io::Seek + Send + Sync> ReadSeek for T {}

/// Builds the notification title and body, including the group sender.
pub fn lines(chat_name: &str, is_group: bool, sender: &str, summary: &str) -> (String, String) {
    let body = if is_group {
        format!("{sender}: {summary}")
    } else {
        summary.to_owned()
    };
    (chat_name.to_owned(), body)
}

#[cfg(target_os = "linux")]
#[expect(clippy::too_many_arguments)]
fn deliver(
    title: &str,
    body: &str,
    picture: Option<&std::path::Path>,
    system_sound: bool,
    target: NotificationTarget,
    opened: Arc<Mutex<Vec<NotificationTarget>>>,
    wake: impl Fn() + Send + 'static,
    mut cancelled: tokio::sync::oneshot::Receiver<Stop>,
) {
    let Some(may_wait) = before_showing(&mut cancelled) else {
        return;
    };
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("ZapFast")
        .summary(title)
        .body(body)
        .icon("zapfast")
        .action("default", "Open");
    if !system_sound {
        notification.hint(notify_rust::Hint::SuppressSound(true));
    }
    if let Some(picture) = picture {
        notification.image_path(&picture.to_string_lossy());
    }
    match notification.show() {
        Ok(handle) => {
            if !may_wait {
                // Shown, and left to the desktop: the handle does not close it.
                return;
            }
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    handle.close();
                    log::debug!("no notification action runtime: {error}");
                    return;
                }
            };
            runtime.block_on(async {
                tokio::select! {
                    biased;
                    stop = &mut cancelled => {
                        if stop != Ok(Stop::Release) {
                            handle.close_async().await;
                        }
                    }
                    _ = handle.wait_for_action_async(|action| {
                        if matches!(action, notify_rust::NotificationResponse::Default) {
                            opened.lock().unwrap_or_else(|p| p.into_inner()).push(target);
                            wake();
                        }
                    }) => {}
                }
            });
        }
        Err(error) => log::debug!("no notification: {error}"),
    }
}

#[cfg(target_os = "windows")]
#[expect(clippy::too_many_arguments)]
fn deliver(
    title: &str,
    body: &str,
    picture: Option<&std::path::Path>,
    system_sound: bool,
    target: NotificationTarget,
    opened: Arc<Mutex<Vec<NotificationTarget>>>,
    wake: impl Fn() + Send + 'static,
    mut cancelled: tokio::sync::oneshot::Receiver<Stop>,
) {
    if before_showing(&mut cancelled).is_some()
        && let Err(error) = windows::show(title, body, picture, system_sound, target, opened, wake)
    {
        log::debug!("no Windows notification: {error}");
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
#[expect(clippy::too_many_arguments)]
fn deliver(
    title: &str,
    body: &str,
    picture: Option<&std::path::Path>,
    system_sound: bool,
    _target: NotificationTarget,
    _opened: Arc<Mutex<Vec<NotificationTarget>>>,
    _wake: impl Fn() + Send + 'static,
    mut cancelled: tokio::sync::oneshot::Receiver<Stop>,
) {
    // Never fall back to application discovery, including for unbundled builds.
    #[cfg(target_os = "macos")]
    if !macos_application_ready() {
        return;
    }
    if before_showing(&mut cancelled).is_none() {
        return;
    }
    let mut notification = notify_rust::Notification::new();
    notification.appname("ZapFast").summary(title).body(body);
    #[cfg(target_os = "macos")]
    if system_sound {
        // The notification system's default sound; custom sounds are played
        // by ZapFast, and None stays silent.
        notification.sound_name("NSUserNotificationDefaultSoundName");
    }
    #[cfg(not(target_os = "macos"))]
    let _ = system_sound;
    // Windows uses the image; macOS always uses the app icon.
    if let Some(picture) = picture {
        notification.image_path(&picture.to_string_lossy());
    }
    if let Err(error) = notification.show() {
        log::debug!("no notification: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_notification_identity_matches_the_packaged_application() {
        let plist = include_str!("../packaging/macos/Info.plist");
        assert!(plist.contains(&format!(
            "<key>CFBundleIdentifier</key><string>{MACOS_APPLICATION_ID}</string>"
        )));
    }

    /// Answering a call takes back its own notification and nothing else.
    ///
    /// A message notification registered while the call was ringing belongs to the same chat, so
    /// `clear(chat)` — which removes every entry for the chat — swallowed it too. The reader never
    /// saw that message, and the notification that would have shown it is gone.
    #[test]
    fn answering_a_call_leaves_its_message_notifications_alone() {
        let mut notifications = Notifications::default();
        // A message notification, then the call's own, in the same chat.
        let mut message = notifications.register("a", false);
        let mut call = notifications.register("a", true);
        let mut other_call = notifications.register("b", true);
        notifications.clear_call("a");
        assert_eq!(call.try_recv(), Ok(Stop::Close), "the call is taken back");
        assert_eq!(
            message.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty),
            "the message notification the reader has not seen stays"
        );
        assert_eq!(
            other_call.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty),
            "another chat's call is untouched"
        );
        // And with the call gone, the chat's ordinary clear still works on the message.
        notifications.clear("a");
        assert_eq!(message.try_recv(), Ok(Stop::Close));
    }

    #[test]
    fn reading_cancels_delivered_and_pending_notifications_for_only_that_chat() {
        let mut notifications = Notifications::default();
        let mut first = notifications.register("a", false);
        let mut second = notifications.register("a", false);
        let mut other = notifications.register("b", false);
        notifications.clear("a");
        assert_eq!(first.try_recv(), Ok(Stop::Close));
        assert_eq!(second.try_recv(), Ok(Stop::Close));
        assert_eq!(
            other.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        let mut next = notifications.register("a", false);
        assert_eq!(
            next.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        notifications.clear_all();
        assert!(other.try_recv().is_err());
        assert_eq!(
            next.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        );
    }

    #[test]
    fn expired_notifications_do_not_accumulate() {
        let mut notifications = Notifications::default();
        drop(notifications.register("a", false));
        let _next = notifications.register("b", false);
        assert!(!notifications.pending.contains_key("a"));
    }

    #[test]
    fn the_oldest_waiting_notification_is_released_without_closing_it() {
        use tokio::sync::oneshot::error::TryRecvError;
        let mut notifications = Notifications::default();
        let mut waiting: Vec<_> = (0..WAITING_LIMIT)
            .map(|index| notifications.register(&format!("chat {}", index % 3), false))
            .collect();
        let mut newest = notifications.register("chat 0", false);
        assert_eq!(waiting[0].try_recv(), Ok(Stop::Release));
        for later in &mut waiting[1..] {
            assert_eq!(later.try_recv(), Err(TryRecvError::Empty));
        }
        assert_eq!(newest.try_recv(), Err(TryRecvError::Empty));
        let count: usize = notifications.pending.values().map(Vec::len).sum();
        assert_eq!(count, WAITING_LIMIT);

        // Reading a chat still closes what is left of it.
        notifications.clear("chat 0");
        assert_eq!(newest.try_recv(), Ok(Stop::Close));
        assert_eq!(waiting[3].try_recv(), Ok(Stop::Close));
        assert_eq!(waiting[1].try_recv(), Err(TryRecvError::Empty));
    }

    /// A burst past the limit releases notifications that have not appeared
    /// yet: they are still shown, only not waited on. A read chat's are not.
    #[test]
    fn a_notification_released_before_it_appears_is_still_shown() {
        let mut notifications = Notifications::default();
        let mut first = notifications.register("a", false);
        let mut read = notifications.register("b", false);
        let _waiting: Vec<_> = (0..WAITING_LIMIT - 1)
            .map(|index| notifications.register(&format!("chat {index}"), false))
            .collect();
        notifications.clear("b");
        assert_eq!(before_showing(&mut first), Some(false));
        assert_eq!(before_showing(&mut read), None);
        let mut fresh = notifications.register("c", false);
        assert_eq!(before_showing(&mut fresh), Some(true));
    }

    #[test]
    fn finished_notifications_do_not_count_against_the_limit() {
        let mut notifications = Notifications::default();
        for index in 0..WAITING_LIMIT * 2 {
            drop(notifications.register(&format!("chat {index}"), false));
        }
        let mut waiting: Vec<_> = (0..WAITING_LIMIT)
            .map(|index| notifications.register(&format!("chat {index}"), false))
            .collect();
        for receiver in &mut waiting {
            assert_eq!(
                receiver.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            );
        }
    }

    /// Shows a test notification with an optional cached picture:
    /// `cargo test --all-features shows_one -- --ignored --nocapture`.
    #[test]
    #[ignore = "shows a real notification"]
    fn shows_one_on_this_desktop() {
        let picture = std::fs::read_dir(crate::paths::AppDirs::discover().avatar_cache_dir())
            .ok()
            .and_then(|entries| entries.flatten().map(|entry| entry.path()).next());
        let mut notifications = Notifications::default();
        notifications.show(
            "Ada Lovelace".into(),
            "A test from ZapFast, with a picture".into(),
            picture,
            NotificationSound::System,
            NotificationTarget {
                chat: "test".into(),
                message: Some("test-message".into()),
            },
            Default::default(),
            || {},
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    #[test]
    fn a_group_names_the_sender_and_a_chat_does_not() {
        assert_eq!(
            lines("Rust Berlin", true, "Mira", "Save me a seat"),
            ("Rust Berlin".to_owned(), "Mira: Save me a seat".to_owned())
        );
        assert_eq!(
            lines("Ada Lovelace", false, "Ada Lovelace", "Photo"),
            ("Ada Lovelace".to_owned(), "Photo".to_owned())
        );
    }
}
