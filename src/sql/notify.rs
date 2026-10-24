//! LISTEN / NOTIFY: sessions subscribe to channels and receive the messages
//! sent on them, as in PostgreSQL. A notification is delivered when the
//! transaction that sent it commits, and never if it rolls back.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// The longest payload NOTIFY accepts, in bytes, as in PostgreSQL.
pub const MAX_PAYLOAD: usize = 8000;

/// Notifications a session holds before it starts dropping the oldest, so a
/// listener that never reads can't grow without bound.
const MAX_QUEUED: usize = 65_536;

/// A message sent with NOTIFY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub channel: String,
    pub payload: String,
    /// The [id](super::Session::id) of the session that sent it.
    pub sender: u64,
}

/// Routes committed notifications to the sessions listening for them.
#[derive(Default)]
pub(crate) struct Notifier {
    inboxes: Mutex<Vec<Weak<Inbox>>>,
}

impl Notifier {
    pub fn register(&self, inbox: &Arc<Inbox>) {
        self.inboxes.lock().unwrap().push(Arc::downgrade(inbox));
    }

    /// Hands notifications to every session listening on their channels,
    /// forgetting sessions that have gone away.
    pub fn deliver(&self, notifications: &[Notification]) {
        if notifications.is_empty() {
            return;
        }
        let mut inboxes = self.inboxes.lock().unwrap();
        inboxes.retain(|inbox| match inbox.upgrade() {
            Some(inbox) => {
                inbox.offer(notifications);
                true
            }
            None => false,
        });
    }
}

/// One session's subscriptions and received notifications.
#[derive(Default)]
pub(crate) struct Inbox {
    state: Mutex<InboxState>,
    ready: Condvar,
}

#[derive(Default)]
struct InboxState {
    channels: HashSet<String>,
    queue: VecDeque<Notification>,
}

impl Inbox {
    pub fn listen(&self, channel: &str) {
        self.state.lock().unwrap().channels.insert(channel.to_string());
    }

    /// Stops listening on `channel`, or on every channel with `None`.
    pub fn unlisten(&self, channel: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        match channel {
            Some(channel) => {
                state.channels.remove(channel);
            }
            None => state.channels.clear(),
        }
    }

    pub fn channels(&self) -> Vec<String> {
        let mut channels: Vec<String> = self.state.lock().unwrap().channels.iter().cloned().collect();
        channels.sort();
        channels
    }

    fn offer(&self, notifications: &[Notification]) {
        let mut state = self.state.lock().unwrap();
        let mut received = false;
        for n in notifications {
            if state.channels.contains(&n.channel) {
                if state.queue.len() == MAX_QUEUED {
                    state.queue.pop_front();
                }
                state.queue.push_back(n.clone());
                received = true;
            }
        }
        drop(state);
        if received {
            self.ready.notify_all();
        }
    }

    pub fn drain(&self) -> Vec<Notification> {
        self.state.lock().unwrap().queue.drain(..).collect()
    }

    /// The oldest notification, waiting up to `timeout` (forever with `None`).
    pub fn wait(&self, timeout: Option<Duration>) -> Option<Notification> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(n) = state.queue.pop_front() {
                return Some(n);
            }
            state = match deadline {
                None => self.ready.wait(state).unwrap(),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    self.ready.wait_timeout(state, deadline - now).unwrap().0
                }
            };
        }
    }
}

/// Receives a session's notifications, from any thread: run queries on the
/// session while another thread waits here.
#[derive(Clone)]
pub struct NotificationReceiver {
    inbox: Arc<Inbox>,
}

impl NotificationReceiver {
    pub(crate) fn new(inbox: Arc<Inbox>) -> Self {
        NotificationReceiver { inbox }
    }

    /// The oldest waiting notification, without waiting.
    pub fn try_recv(&self) -> Option<Notification> {
        self.inbox.wait(Some(Duration::ZERO))
    }

    /// Waits up to `timeout` for a notification.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Notification> {
        self.inbox.wait(Some(timeout))
    }

    /// Waits for the next notification, however long it takes.
    pub fn recv(&self) -> Notification {
        self.inbox.wait(None).expect("waiting without a timeout always returns a notification")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(channel: &str, payload: &str) -> Notification {
        Notification { channel: channel.into(), payload: payload.into(), sender: 1 }
    }

    #[test]
    fn delivers_only_to_listeners_of_the_channel() {
        let notifier = Notifier::default();
        let (a, b) = (Arc::new(Inbox::default()), Arc::new(Inbox::default()));
        notifier.register(&a);
        notifier.register(&b);
        a.listen("orders");
        b.listen("users");
        notifier.deliver(&[note("orders", "1"), note("users", "2"), note("other", "3")]);
        assert_eq!(a.drain(), vec![note("orders", "1")]);
        assert_eq!(b.drain(), vec![note("users", "2")]);
        a.unlisten(None);
        notifier.deliver(&[note("orders", "4")]);
        assert!(a.drain().is_empty());
    }

    #[test]
    fn forgets_dropped_inboxes_and_wakes_waiters() {
        let notifier = Arc::new(Notifier::default());
        let gone = Arc::new(Inbox::default());
        notifier.register(&gone);
        drop(gone);
        let inbox = Arc::new(Inbox::default());
        inbox.listen("ch");
        notifier.register(&inbox);
        let receiver = NotificationReceiver::new(inbox.clone());
        assert_eq!(receiver.try_recv(), None);
        let sender = {
            let notifier = notifier.clone();
            std::thread::spawn(move || notifier.deliver(&[note("ch", "wake")]))
        };
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)), Some(note("ch", "wake")));
        sender.join().unwrap();
        assert_eq!(notifier.inboxes.lock().unwrap().len(), 1, "the dropped inbox is forgotten");
    }
}
