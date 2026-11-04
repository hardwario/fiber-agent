//! The northbound outbox of the P2P central (`p2p_tower_gateway.md` §8.6, `#470` C3).
//!
//! A ring event is acked to the gateway once its effects are stored, and the gateway then
//! forgets it; a resend would be refused anyway, since the Node's frame counter is already in
//! `sessions.db`. So the northbound messages an event produces are part of what is stored: they
//! go into this outbox, durably, before the event counts as done, and a drainer publishes them
//! when the broker can take them.
//!
//! - **Durable.** One file per event under `p2p/outbox/` beside `sessions.db`, written to a
//!   temp file, synced and renamed into place, then the directory is synced: the
//!   [`crate::store::Store::write`] recipe. Startup reloads every file, in the order of the
//!   sequence number each one holds.
//! - **Idempotent.** A file is named by its event's key — the Node's DevEUI, what the event is
//!   and its frame counter ([`uplink_key`], [`undelivered_key`]) — not by a counter of appends.
//!   The same event appended again while its entry is still pending (a crash between the append
//!   and the `sessions.db` save, and the gateway's resend processed afresh) is the entry
//!   already there: nothing is published twice. One whose entry was already published goes out
//!   again; consumers dedup by DevEUI and frame counter (`p2p_tower_gateway.md` §8.9).
//! - **Ordered.** One queue, drained head first, one message in flight. A Node's messages go out
//!   in the order its events were processed, and the messages of one event in the order they
//!   were appended — the decoded uplink, then history, config, `device_info`.
//! - **Deleted only once the broker has it.** The drainer publishes with
//!   [`NorthboundPublisher::publish_confirmed`], which resolves on the QoS 1 `PUBACK`. A message
//!   is taken off its file only then, so a broker that is down or reconnecting costs time, not
//!   messages, and a message that got its `PUBACK` is never published again. One window stays:
//!   a crash between the `PUBACK` and the file update publishes that message again on the next
//!   start (at-least-once).
//! - **Bounded.** At most [`CAPACITY`] messages. An event whose messages do not fit is refused
//!   whole and counted ([`Outbox::overflow`]); the central then does not ack it, and the gateway
//!   holds it and resends (backpressure). Nothing is dropped.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::northbound::Area;
use crate::p2p::publisher::NorthboundPublisher;
use crate::store::Store;

/// The most messages the outbox holds: about four minutes of a busy network's uplinks
/// (≤ 4 messages each) with the broker gone, and a few MB of disk at most.
pub const CAPACITY: usize = 1000;

/// The most messages one ring event stores: the decoded uplink, history, config and
/// `device_info` (an `EVT_TWR_TX` stores at most one).
pub const EVENT_MAX: usize = 4;

/// The outbox directory, relative to the store root: beside `p2p/sessions.db`.
pub const DIR: &str = "p2p/outbox";

/// The first wait after a failed publish; it doubles up to [`BACKOFF_MAX`].
pub const BACKOFF_MIN: Duration = Duration::from_millis(500);
/// The longest wait between two attempts to publish the head.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

macro_rules! diag {
    ($($arg:tt)*) => { eprintln!("[p2p {}] {}", now_ms(), format_args!($($arg)*)) };
}

/// One northbound message: a document for one Node's area.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub dev_eui: String,
    /// [`Area::as_str`].
    pub area: String,
    /// The serialized document, published as is.
    pub payload: String,
}

impl Message {
    pub fn new(dev_eui: &str, area: Area, payload: &serde_json::Value) -> Self {
        Message {
            dev_eui: dev_eui.to_string(),
            area: area.as_str().to_string(),
            payload: payload.to_string(),
        }
    }
}

/// The key of an uplink event's entry: its Node and frame counter. The documents it carries —
/// the decoded uplink and whatever history page, config or `device_info` that one frame
/// completed — follow from the frame, so the counter names them all.
pub fn uplink_key(dev_eui: &str, fcnt: u32) -> String {
    format!("{dev_eui}-uplink-{fcnt:010}")
}

/// The key of the `undelivered` command document for the downlink `seq` given up on.
pub fn undelivered_key(dev_eui: &str, seq: u32) -> String {
    format!("{dev_eui}-undelivered-{seq:010}")
}

/// Whether `key` is usable as a file name as it is.
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// One event's messages, as its file holds them: those not yet acked by the broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// The order of appends, across restarts: the queue is sorted by it on load.
    id: u64,
    /// The event's key, which is also the file name.
    key: String,
    messages: VecDeque<Message>,
}

#[derive(Debug, Default)]
struct Inner {
    entries: VecDeque<Entry>,
    /// Messages across `entries`.
    pending: usize,
    next_id: u64,
}

/// The outbox. Shared (`Arc`) by the central, which appends, and the drainer, which publishes;
/// one per state directory and process.
#[derive(Debug)]
pub struct Outbox {
    dir: PathBuf,
    capacity: usize,
    inner: Mutex<Inner>,
    notify: tokio::sync::Notify,
    drainer: AtomicBool,
    overflow: AtomicU64,
    backoff: (Duration, Duration),
}

/// An event's entry in its temp file ([`Outbox::stage`]), not yet in the queue.
#[derive(Debug)]
pub struct StagedEntry {
    entry: Option<Entry>,
    file: Option<std::fs::File>,
    tmp: PathBuf,
    path: PathBuf,
}

impl StagedEntry {
    /// Flush the entry to the disk. Idempotent; [`Outbox::commit`] does it if not done yet.
    pub fn sync(&self) -> Result<()> {
        match &self.file {
            Some(f) => f.sync_all().map_err(outbox_err),
            None => Ok(()),
        }
    }
}

impl Drop for StagedEntry {
    fn drop(&mut self) {
        // Not committed: its event is not acked, and the gateway's resend stages it again.
        if !self.tmp.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

fn outbox_err(m: impl std::fmt::Display) -> Error {
    Error::new(
        Kind::StorageError,
        Transport::P2p,
        Component::SessionDb,
        format!("northbound outbox: {m}"),
    )
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir).and_then(|d| d.sync_all())
}

impl Outbox {
    /// The outbox of `store`, with what an earlier run left in it.
    pub fn open(store: &Store) -> Self {
        Self::open_dir(store.root().join(DIR), CAPACITY)
    }

    /// The outbox in `dir`, holding at most `capacity` messages. A directory that cannot be
    /// read opens empty, logged; appending to it then fails, and the central acks nothing.
    pub fn open_dir(dir: impl Into<PathBuf>, capacity: usize) -> Self {
        let dir = dir.into();
        let mut inner = Inner::default();
        match std::fs::read_dir(&dir) {
            Ok(rd) => {
                let mut found: Vec<PathBuf> = Vec::new();
                for de in rd.flatten() {
                    let path = de.path();
                    let name = de.file_name().to_string_lossy().into_owned();
                    if name.ends_with(".tmp") {
                        // A write that never reached its rename: its event was not acked.
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if name.ends_with(".json") {
                        found.push(path);
                    }
                }
                let mut loaded: Vec<Entry> = Vec::new();
                for path in found {
                    let stem = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let entry = std::fs::read_to_string(&path)
                        .map_err(|e| e.to_string())
                        .and_then(|t| serde_json::from_str::<Entry>(&t).map_err(|e| e.to_string()))
                        .and_then(|e| {
                            if e.key == stem {
                                Ok(e)
                            } else {
                                Err(format!("holds key {:?}", e.key))
                            }
                        });
                    match entry {
                        Ok(e) => {
                            inner.next_id = inner.next_id.max(e.id + 1);
                            if !e.messages.is_empty() {
                                loaded.push(e);
                            }
                        }
                        Err(e) => {
                            // Kept for a person to look at, out of the queue's way.
                            diag!(
                                "outbox entry {} unreadable ({e}) — set aside as .bad",
                                path.display()
                            );
                            let _ = std::fs::rename(&path, path.with_extension("bad"));
                        }
                    }
                }
                loaded.sort_by_key(|e| e.id);
                for e in loaded {
                    inner.pending += e.messages.len();
                    inner.entries.push_back(e);
                }
                if inner.pending > 0 {
                    diag!(
                        "outbox: {} message(s) from before the restart to publish",
                        inner.pending
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => diag!("outbox {} cannot be read: {e}", dir.display()),
        }
        Outbox {
            dir,
            capacity,
            inner: Mutex::new(inner),
            notify: tokio::sync::Notify::new(),
            drainer: AtomicBool::new(false),
            overflow: AtomicU64::new(0),
            backoff: (BACKOFF_MIN, BACKOFF_MAX),
        }
    }

    /// Retry faster: the test seam of the drainer's backoff.
    pub fn with_backoff(mut self, min: Duration, max: Duration) -> Self {
        self.backoff = (min, max);
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// Write `entry` durably under its id, replacing what is there.
    fn write_entry(&self, entry: &Entry) -> Result<()> {
        let path = self.path(&entry.key);
        let tmp = path.with_extension("tmp");
        let f = self.write_tmp(&tmp, entry)?;
        let result = f
            .sync_all()
            .map_err(outbox_err)
            .and_then(|()| std::fs::rename(&tmp, &path).map_err(outbox_err));
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        sync_dir(&self.dir).map_err(outbox_err)
    }

    /// `entry` in the temp file `tmp`, written, not flushed.
    fn write_tmp(&self, tmp: &Path, entry: &Entry) -> Result<std::fs::File> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| outbox_err(format!("cannot create {}: {e}", self.dir.display())))?;
        let text = serde_json::to_vec(entry).map_err(outbox_err)?;
        let mut f = std::fs::File::create(tmp).map_err(outbox_err)?;
        if let Err(e) = f.write_all(&text) {
            let _ = std::fs::remove_file(tmp);
            return Err(outbox_err(e));
        }
        Ok(f)
    }

    /// Store one event's messages under its `key`, in the order given, durably. All or nothing:
    /// an event that does not fit is refused whole and counted in [`Self::overflow`]. An event
    /// whose entry is still pending is that entry: stored once, published once, whatever of it
    /// the broker already has not sent again.
    pub fn append(&self, key: &str, messages: Vec<Message>) -> Result<()> {
        match self.stage(key, messages)? {
            Some(staged) => self.commit(staged),
            None => Ok(()),
        }
    }

    /// [`Self::append`] up to the entry's temp file, written but not flushed and not yet in
    /// the queue; `None` when there is nothing to store (no messages, or the key is pending
    /// already). [`StagedEntry::sync`] flushes it and [`Self::commit`] puts it in place, so a
    /// caller storing a second file flushes both back to back — one journal commit for the two
    /// (#130). Dropped uncommitted, the temp file is removed.
    ///
    /// The central is the only writer: what is checked here (room, the key) holds until the
    /// commit, the drainer only takes messages away.
    pub fn stage(&self, key: &str, messages: Vec<Message>) -> Result<Option<StagedEntry>> {
        if messages.is_empty() {
            return Ok(None);
        }
        if !valid_key(key) {
            return Err(outbox_err(format!("bad entry key {key:?}")));
        }
        let mut inner = self.lock();
        if inner.entries.iter().any(|e| e.key == key) {
            diag!("outbox entry {key} is already pending — the event was stored before");
            return Ok(None);
        }
        if inner.pending + messages.len() > self.capacity {
            self.overflow.fetch_add(1, Ordering::Relaxed);
            return Err(outbox_err(format!(
                "full ({} of {} messages pending) — the event waits for the broker",
                inner.pending, self.capacity
            )));
        }
        let entry = Entry {
            id: inner.next_id,
            key: key.to_string(),
            messages: messages.into(),
        };
        // The id is taken now: a stage dropped uncommitted leaves a gap, which is harmless.
        inner.next_id += 1;
        drop(inner);
        let path = self.path(&entry.key);
        let tmp = path.with_extension("tmp");
        let file = self.write_tmp(&tmp, &entry)?;
        Ok(Some(StagedEntry {
            entry: Some(entry),
            file: Some(file),
            tmp,
            path,
        }))
    }

    /// Put a staged entry in place — flushed, renamed, the directory synced — and queue it.
    pub fn commit(&self, mut staged: StagedEntry) -> Result<()> {
        staged.sync()?;
        staged.file = None;
        std::fs::rename(&staged.tmp, &staged.path).map_err(outbox_err)?;
        staged.tmp = PathBuf::new();
        sync_dir(&self.dir).map_err(outbox_err)?;
        let Some(entry) = staged.entry.take() else {
            return Ok(());
        };
        let mut inner = self.lock();
        inner.pending += entry.messages.len();
        inner.entries.push_back(entry);
        drop(inner);
        self.notify.notify_one();
        Ok(())
    }

    /// Whether an event of up to `n` messages fits. One that does not is counted in
    /// [`Self::overflow`]: the central refuses it before touching anything, and does not ack it.
    pub fn has_room(&self, n: usize) -> bool {
        let fits = self.lock().pending + n <= self.capacity;
        if !fits {
            self.overflow.fetch_add(1, Ordering::Relaxed);
        }
        fits
    }

    /// Messages waiting for the broker.
    pub fn pending(&self) -> usize {
        self.lock().pending
    }

    /// The bound on [`Self::pending`].
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Events refused because they did not fit, since the process started.
    pub fn overflow(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }

    /// Whether a drainer task ([`Self::run_drainer`]) publishes this outbox.
    pub fn has_drainer(&self) -> bool {
        self.drainer.load(Ordering::Relaxed)
    }

    /// The oldest message not yet acked by the broker, with its entry's sequence number.
    fn head(&self) -> Option<(u64, Message)> {
        let inner = self.lock();
        let e = inner.entries.front()?;
        e.messages.front().map(|m| (e.id, m.clone()))
    }

    /// The broker acked the head of entry `id`: take it off, on disk first.
    fn delivered(&self, id: u64) {
        let mut inner = self.lock();
        let Some(entry) = inner.entries.front_mut().filter(|e| e.id == id) else {
            return;
        };
        entry.messages.pop_front();
        let result = if entry.messages.is_empty() {
            // No directory sync after the delete (#130): it only made the delete durable, and
            // a delete lost to a power cut republishes messages the broker already has — the
            // at-least-once window §12 already allows for a crash between the PUBACK and the
            // delete; consumers dedup by DevEUI + fcnt. The sync competed with the ring ack's
            // own flushes on the same filesystem. A rewrite below keeps its syncs: a torn or
            // stale entry would lose or repeat messages that were never published.
            std::fs::remove_file(self.path(&entry.key)).map_err(outbox_err)
        } else {
            let entry = entry.clone();
            self.write_entry(&entry)
        };
        if let Err(e) = result {
            // In memory it is gone either way: the drainer must not publish it twice now. A
            // restart reads the file as it is and publishes the message once more.
            diag!(
                "outbox entry {id}: an acked message stays on disk ({}) — a restart publishes it \
                 again",
                e.message()
            );
        }
        inner.pending = inner.pending.saturating_sub(1);
        if inner.entries.front().is_some_and(|e| e.messages.is_empty()) {
            inner.entries.pop_front();
        }
    }

    /// Publish the head once. `Ok(true)` delivered, `Ok(false)` nothing to publish.
    async fn publish_head<P: NorthboundPublisher + ?Sized>(&self, publisher: &P) -> Result<bool> {
        let Some((id, m)) = self.head() else {
            return Ok(false);
        };
        let Some(area) = Area::from_name(&m.area) else {
            diag!(
                "outbox entry {id}: unknown area {:?} — dropped, it has no topic",
                m.area
            );
            self.delivered(id);
            return Ok(true);
        };
        publisher
            .publish_confirmed(&m.dev_eui, area, m.payload.as_bytes())
            .await?;
        self.delivered(id);
        Ok(true)
    }

    /// Publish what is pending, in order, until the outbox is empty or a publish fails. For a
    /// central with no drainer task (tests, a harness); the number delivered.
    pub async fn drain_now<P: NorthboundPublisher + ?Sized>(&self, publisher: &P) -> usize {
        let mut n = 0;
        loop {
            match self.publish_head(publisher).await {
                Ok(true) => n += 1,
                Ok(false) => return n,
                Err(e) => {
                    diag!(
                        "northbound publish failed: {} — {} message(s) kept in the outbox",
                        e.message(),
                        self.pending()
                    );
                    return n;
                }
            }
        }
    }

    /// Publish forever: the head, once the broker acks it the next, waiting while the outbox
    /// is empty and backing off while the broker is away. One per outbox and process, started
    /// by the host beside the broker client and outliving the central's restarts, so a publish
    /// in flight is never abandoned and sent a second time by a successor.
    ///
    /// The outbox counts as drained by this task from the call on, before the future is first
    /// polled, so a central never publishes in place beside it.
    pub fn run_drainer<P: NorthboundPublisher>(
        self: Arc<Self>,
        publisher: P,
    ) -> impl std::future::Future<Output = ()> + Send {
        self.drainer.store(true, Ordering::Relaxed);
        async move {
            let (min, max) = self.backoff;
            let mut wait = min;
            let mut failing = false;
            // The broker connect last accounted for, and the backlog drained since a newer one.
            let mut seen_connect = 0u64;
            let mut drain: Option<(Instant, usize)> = None;
            loop {
                match self.publish_head(&publisher).await {
                    Ok(true) => {
                        if failing {
                            diag!("northbound publishing again, {} pending", self.pending());
                            failing = false;
                        }
                        wait = min;
                        if let Some((n, at)) = publisher.broker_connect() {
                            if n != seen_connect {
                                seen_connect = n;
                                drain = Some((at, 0));
                            }
                        }
                        if let Some((_, count)) = drain.as_mut() {
                            *count += 1;
                        }
                    }
                    Ok(false) => {
                        if let Some((at, count)) = drain.take() {
                            diag!(
                                "outbox: drained {count} {} in {} ms after broker connect",
                                if count == 1 { "message" } else { "messages" },
                                at.elapsed().as_millis()
                            );
                        }
                        self.notify.notified().await;
                        // A connect while the outbox was empty drained nothing.
                        if let Some((n, _)) = publisher.broker_connect() {
                            seen_connect = n;
                        }
                    }
                    Err(e) => {
                        diag!(
                            "outbox: publish {} failed: {} — retry in {} ms, {} message(s) \
                             kept in the outbox",
                            if failing { "retry" } else { "of the head" },
                            e.message(),
                            wait.as_millis(),
                            self.pending()
                        );
                        failing = true;
                        tokio::time::sleep(wait).await;
                        wait = (wait * 2).min(max);
                    }
                }
            }
        }
    }

    /// Wake the drainer: something was appended.
    pub fn kick(&self) {
        self.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::fake::RecordingPublisher;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let p =
            std::env::temp_dir().join(format!("outbox-{tag}-{}-{}", std::process::id(), now_ms()));
        Tmp(p)
    }

    /// A fresh key per call, as distinct events have.
    fn k() -> String {
        static N: AtomicU64 = AtomicU64::new(0);
        uplink_key("a", N.fetch_add(1, Ordering::Relaxed) as u32)
    }

    fn msg(dev: &str, n: u32) -> Message {
        Message::new(dev, Area::Reading, &serde_json::json!({ "n": n }))
    }

    fn payloads(p: &RecordingPublisher) -> Vec<String> {
        p.published()
            .iter()
            .map(|m| String::from_utf8_lossy(&m.payload).into_owned())
            .collect()
    }

    #[tokio::test]
    async fn an_appended_event_is_published_in_order_and_removed() {
        let t = tmp("order");
        let ob = Outbox::open_dir(&t.0, 10);
        ob.append(&k(), vec![msg("a", 1), msg("a", 2)])
            .expect("append");
        ob.append(&k(), vec![msg("b", 3)]).expect("append");
        assert_eq!(ob.pending(), 3);
        let p = RecordingPublisher::new();
        assert_eq!(ob.drain_now(&p).await, 3);
        assert_eq!(payloads(&p), ["{\"n\":1}", "{\"n\":2}", "{\"n\":3}"]);
        assert_eq!(ob.pending(), 0);
        assert_eq!(
            std::fs::read_dir(&t.0).expect("dir").count(),
            0,
            "files gone"
        );
    }

    #[tokio::test]
    async fn a_restart_publishes_what_is_left_and_not_what_was_acked() {
        let t = tmp("restart");
        {
            let ob = Outbox::open_dir(&t.0, 10);
            ob.append(&k(), vec![msg("a", 1), msg("a", 2)])
                .expect("append");
            ob.append(&k(), vec![msg("a", 3)]).expect("append");
            // The first message is acked, then the process stops.
            let id = ob.head().expect("head").0;
            ob.delivered(id);
        }
        // Debris of a write that never reached its rename.
        std::fs::write(t.0.join("a-uplink-0000000099.tmp"), b"{").expect("tmp");
        let ob = Outbox::open_dir(&t.0, 10);
        assert_eq!(ob.pending(), 2);
        let p = RecordingPublisher::new();
        ob.drain_now(&p).await;
        assert_eq!(payloads(&p), ["{\"n\":2}", "{\"n\":3}"]);
        ob.append(&k(), vec![msg("a", 4)]).expect("ids continue");
        ob.drain_now(&p).await;
        assert_eq!(payloads(&p).last().map(String::as_str), Some("{\"n\":4}"));
    }

    #[test]
    fn an_event_that_does_not_fit_is_refused_whole_and_counted() {
        let t = tmp("full");
        let ob = Outbox::open_dir(&t.0, 3);
        ob.append(&k(), vec![msg("a", 1), msg("a", 2)])
            .expect("fits");
        assert!(ob.append(&k(), vec![msg("a", 3), msg("a", 4)]).is_err());
        assert_eq!((ob.pending(), ob.overflow()), (2, 1));
        ob.append(&k(), vec![msg("a", 3)]).expect("one more fits");
    }

    #[test]
    fn an_unreadable_entry_is_set_aside() {
        let t = tmp("bad");
        std::fs::create_dir_all(&t.0).expect("dir");
        std::fs::write(t.0.join("a-uplink-0000000004.json"), b"not json").expect("bad");
        let ob = Outbox::open_dir(&t.0, 3);
        assert_eq!(ob.pending(), 0);
        assert!(t.0.join("a-uplink-0000000004.bad").exists());
        ob.append(&uplink_key("a", 4), vec![msg("a", 1)])
            .expect("append");
        assert!(t.0.join("a-uplink-0000000004.json").exists());
    }

    /// The same event appended twice — stored, then processed afresh after a crash before its
    /// `sessions.db` save — is one entry and one publish, also across a restart.
    #[tokio::test]
    async fn the_same_event_appended_twice_is_published_once() {
        let t = tmp("idem");
        let key = uplink_key("a", 7);
        {
            let ob = Outbox::open_dir(&t.0, 10);
            ob.append(&key, vec![msg("a", 1), msg("a", 2)])
                .expect("append");
            ob.append(&uplink_key("a", 8), vec![msg("a", 3)])
                .expect("append");
            ob.append(&key, vec![msg("a", 1), msg("a", 2)])
                .expect("again");
            assert_eq!(ob.pending(), 3);
        }
        let ob = Outbox::open_dir(&t.0, 10);
        ob.append(&key, vec![msg("a", 1), msg("a", 2)])
            .expect("again after a restart");
        assert_eq!(ob.pending(), 3);
        let p = RecordingPublisher::new();
        ob.drain_now(&p).await;
        assert_eq!(
            payloads(&p),
            ["{\"n\":1}", "{\"n\":2}", "{\"n\":3}"],
            "once each, in append order"
        );
    }

    /// Entries reload in append order, not in the order of their names.
    #[tokio::test]
    async fn a_restart_keeps_the_append_order() {
        let t = tmp("reload-order");
        {
            let ob = Outbox::open_dir(&t.0, 10);
            for (dev, n) in [("c", 1), ("a", 2), ("b", 3)] {
                ob.append(&uplink_key(dev, n), vec![msg(dev, n)])
                    .expect("append");
            }
        }
        let ob = Outbox::open_dir(&t.0, 10);
        let p = RecordingPublisher::new();
        ob.drain_now(&p).await;
        assert_eq!(payloads(&p), ["{\"n\":1}", "{\"n\":2}", "{\"n\":3}"]);
    }

    /// #130: a staged entry is neither on disk under its name nor pending until it is
    /// committed; one dropped instead leaves nothing behind, and a duplicate stages nothing.
    #[tokio::test]
    async fn a_staged_entry_counts_only_once_committed() {
        let t = tmp("stage");
        let ob = Outbox::open_dir(&t.0, 10);
        let key = k();
        let staged = ob
            .stage(&key, vec![msg("a", 1)])
            .expect("stage")
            .expect("new");
        staged.sync().expect("sync");
        assert_eq!(ob.pending(), 0);
        assert!(!t.0.join(format!("{key}.json")).exists());
        ob.commit(staged).expect("commit");
        assert_eq!(ob.pending(), 1);
        assert!(t.0.join(format!("{key}.json")).exists());
        assert!(ob.stage(&key, vec![msg("a", 1)]).expect("dup").is_none());

        let dropped = ob.stage(&k(), vec![msg("a", 2)]).expect("stage");
        drop(dropped);
        let names: Vec<_> = std::fs::read_dir(&t.0)
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names.len(), 1, "no temp file left: {names:?}");
        assert_eq!(ob.pending(), 1);
        let p = RecordingPublisher::new();
        ob.drain_now(&p).await;
        assert_eq!(payloads(&p), ["{\"n\":1}"]);
    }

    /// #130 / §12: the delete of a published entry is not made durable. If a power cut loses
    /// it, the entry comes back once on the next start and is published again — the
    /// at-least-once window consumers dedup by DevEUI + fcnt — and is then gone for good.
    #[tokio::test]
    async fn a_lost_delete_republishes_the_entry_once() {
        let t = tmp("lostdel");
        let key = uplink_key("a", 5);
        let file = t.0.join(format!("{key}.json"));
        let saved;
        {
            let ob = Outbox::open_dir(&t.0, 10);
            ob.append(&key, vec![msg("a", 1)]).expect("append");
            saved = std::fs::read(&file).expect("entry on disk");
            let p = RecordingPublisher::new();
            assert_eq!(ob.drain_now(&p).await, 1);
            assert!(!file.exists(), "deleted once published");
        }
        // The delete did not survive the power cut.
        std::fs::write(&file, &saved).expect("restore");
        let ob = Outbox::open_dir(&t.0, 10);
        let p = RecordingPublisher::new();
        assert_eq!(ob.drain_now(&p).await, 1);
        assert_eq!(payloads(&p), ["{\"n\":1}"]);
        assert!(!file.exists());
        let ob = Outbox::open_dir(&t.0, 10);
        assert_eq!(ob.pending(), 0, "and only once");
    }

    #[test]
    fn a_key_that_is_no_file_name_is_refused() {
        let t = tmp("badkey");
        let ob = Outbox::open_dir(&t.0, 3);
        assert!(ob.append("../x", vec![msg("a", 1)]).is_err());
        assert_eq!(ob.pending(), 0);
    }
}
