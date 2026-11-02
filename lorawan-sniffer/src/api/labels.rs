//! The label writer: the one place where device labels change.
//!
//! Every change to the label table, whether it comes from `PUT`/`DELETE` or
//! from the retention sweep, is a command sent to a single task.  The task
//! takes one command at a time, saves it, publishes the matching event on the
//! label channel and only then answers the requester.  Two properties follow:
//!
//! - **Publish order equals commit order.**  If handlers saved and published
//!   on their own, two requests could commit in one order and publish in the
//!   other, and a WebUI that applies events as they come would end up showing
//!   a label the database no longer holds.
//! - **A cancelled request cannot lose its event.**  A browser that closes the
//!   page, or a proxy that times out, drops the handler's future at its next
//!   `.await`.  If the handler did the saving, that could land between the
//!   commit and the publish, leaving every other open WebUI unaware of a
//!   change that was saved.  Here the handler only queues a command and waits
//!   for the answer; once queued, the change is saved and published whether or
//!   not anyone is still waiting.
//!
//! A command that was never taken from the queue (the sniffer is stopping)
//! is dropped with its reply channel, and the requester sees
//! [`WriterError::Stopped`].

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::make_label_json;
use crate::core::error::SnifferError;
use crate::core::labels::MAX_LABELS;
use crate::core::seen::SeenDevices;
use crate::core::storage::{LabelWrite, OrphanPolicy, Pruned, Storage};

/// Commands that can wait for the writer at once.
///
/// Label changes are made by people, one click at a time, plus one sweep
/// command every few minutes, so the queue is almost always empty.  64 leaves
/// room for a burst of requests, such as a script labelling devices in a loop,
/// without letting a flood grow memory: when the queue is full, `PUT` handlers
/// wait to enqueue, which is the back-pressure a slow disk should apply.
const QUEUE_CAPACITY: usize = 64;

/// Why a command could not be carried out.
#[derive(Debug, Error)]
pub enum WriterError {
    /// The database refused the operation.
    #[error(transparent)]
    Storage(#[from] SnifferError),

    /// The task running the database operation panicked.
    #[error("the storage task failed: {0}")]
    Task(#[from] tokio::task::JoinError),

    /// The writer is not running: the sniffer is shutting down, and the
    /// command was never taken from the queue (or the queue is closed).
    #[error("the label writer has stopped")]
    Stopped,
}

type Reply<T> = oneshot::Sender<Result<T, WriterError>>;

enum Command {
    Set {
        device: String,
        label: String,
        reply: Reply<LabelWrite>,
    },
    Delete {
        device: String,
        reply: Reply<bool>,
    },
    Prune {
        cutoff: DateTime<Utc>,
        keep_orphans: bool,
        reply: Reply<Pruned>,
    },
}

/// Handle to the label writer task.  Cloning gives another handle to the same
/// task.
#[derive(Clone)]
pub struct LabelWriter {
    tx: mpsc::Sender<Command>,
    /// The most labels the table may hold; reported in the error shown when
    /// it is reached.
    max_labels: usize,
}

impl LabelWriter {
    /// Start the writer task.
    ///
    /// `live_window` is how recently an identifier must have been seen in
    /// live traffic to count as known (the retention period).  Every label
    /// change is published on `labels_tx` once saved.  The task finishes the
    /// command it is working on when `cancel` fires, then stops.
    pub fn spawn(
        storage: Storage,
        seen: SeenDevices,
        live_window: Duration,
        labels_tx: broadcast::Sender<Arc<String>>,
        cancel: CancellationToken,
    ) -> Self {
        Self::start(
            Worker {
                storage,
                seen,
                live_window,
                labels_tx,
                max_labels: MAX_LABELS,
                #[cfg(test)]
                gate: None,
            },
            cancel,
        )
    }

    /// Like [`LabelWriter::spawn`], with a lower label limit and an optional
    /// gate that holds each command before it is saved.
    #[cfg(test)]
    pub(crate) fn spawn_for_test(
        storage: Storage,
        seen: SeenDevices,
        live_window: Duration,
        labels_tx: broadcast::Sender<Arc<String>>,
        max_labels: usize,
        gate: Option<Arc<Gate>>,
        cancel: CancellationToken,
    ) -> Self {
        Self::start(
            Worker {
                storage,
                seen,
                live_window,
                labels_tx,
                max_labels,
                gate,
            },
            cancel,
        )
    }

    fn start(worker: Worker, cancel: CancellationToken) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let max_labels = worker.max_labels;
        tokio::spawn(worker.run(rx, cancel));
        Self { tx, max_labels }
    }

    /// The most labels that can exist at once.
    pub fn max_labels(&self) -> usize {
        self.max_labels
    }

    /// Set or replace the label of `device`.
    ///
    /// `device` and `label` must already be normalised (see
    /// [`crate::core::labels`]).  A device that is neither stored nor seen
    /// live, or a new label past the limit, is reported in the returned
    /// [`LabelWrite`] and writes nothing.
    pub async fn set(&self, device: String, label: String) -> Result<LabelWrite, WriterError> {
        self.request(|reply| Command::Set {
            device,
            label,
            reply,
        })
        .await
    }

    /// Remove the label of `device`.  Returns `false`, and publishes nothing,
    /// when there was none.
    pub async fn delete(&self, device: String) -> Result<bool, WriterError> {
        self.request(|reply| Command::Delete { device, reply })
            .await
    }

    /// Delete messages older than `cutoff`, and the labels of devices that
    /// have no stored message and, unless `keep_orphans` is set, were not
    /// seen live within the live window.  One removal event is published per
    /// label deleted.
    pub async fn prune(
        &self,
        cutoff: DateTime<Utc>,
        keep_orphans: bool,
    ) -> Result<Pruned, WriterError> {
        self.request(|reply| Command::Prune {
            cutoff,
            keep_orphans,
            reply,
        })
        .await
    }

    /// Queue a command and wait for its answer.
    ///
    /// Dropping this future after the command is queued abandons only the
    /// wait; the writer still carries the command out.
    async fn request<T>(
        &self,
        command: impl FnOnce(Reply<T>) -> Command,
    ) -> Result<T, WriterError> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(command(reply))
            .await
            .map_err(|_| WriterError::Stopped)?;
        answer.await.map_err(|_| WriterError::Stopped)?
    }
}

/// Everything the writer task owns.
struct Worker {
    storage: Storage,
    seen: SeenDevices,
    live_window: Duration,
    labels_tx: broadcast::Sender<Arc<String>>,
    max_labels: usize,
    /// Lets a test hold the writer between taking a command and saving it.
    #[cfg(test)]
    gate: Option<Arc<Gate>>,
}

impl Worker {
    async fn run(self, mut queue: mpsc::Receiver<Command>, cancel: CancellationToken) {
        loop {
            let command = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                command = queue.recv() => match command {
                    Some(command) => command,
                    None => break,
                },
            };
            #[cfg(test)]
            self.hold().await;
            self.handle(command).await;
        }
        // Dropping the queue drops whatever is still in it, together with the
        // reply channels, so those requesters stop waiting.
        debug!("Label writer stopped");
    }

    async fn handle(&self, command: Command) {
        match command {
            Command::Set {
                device,
                label,
                reply,
            } => {
                let seen_live = self.seen.seen_within(&device, self.live_window);
                let storage = self.storage.clone();
                let max_labels = self.max_labels;
                let (d, l) = (device.clone(), label.clone());
                let result =
                    blocking(move || storage.set_label(&d, &l, seen_live, max_labels)).await;
                match &result {
                    Ok(LabelWrite::Set) => {
                        // The identifier only: label text is operator data and
                        // stays out of the log.
                        info!("label set for {}", device);
                        self.publish(&device, Some(&label));
                    }
                    Ok(_) => {}
                    // The requester may be gone, so this is the only place
                    // that is sure to report the failure.
                    Err(e) => error!("Label write failed for {}: {}", device, e),
                }
                // The requester may have gone away; the change is saved either way.
                let _ = reply.send(result);
            }
            Command::Delete { device, reply } => {
                let storage = self.storage.clone();
                let d = device.clone();
                let result = blocking(move || storage.delete_label(&d)).await;
                match &result {
                    Ok(true) => {
                        info!("label removed for {}", device);
                        self.publish(&device, None);
                    }
                    Ok(false) => {}
                    Err(e) => error!("Label removal failed for {}: {}", device, e),
                }
                let _ = reply.send(result);
            }
            Command::Prune {
                cutoff,
                keep_orphans,
                reply,
            } => {
                let storage = self.storage.clone();
                let seen = self.seen.clone();
                let window = self.live_window;
                let result = blocking(move || {
                    if keep_orphans {
                        storage.delete_older_than(cutoff, OrphanPolicy::Keep)
                    } else {
                        // Runs under the storage lock: a lookup in the live
                        // list only, never a call back into storage.
                        let keep = |device: &str| seen.seen_within(device, window);
                        storage.delete_older_than(cutoff, OrphanPolicy::KeepIf(&keep))
                    }
                })
                .await;
                // Its one requester, the retention task, always waits for the
                // answer and reports a failure itself.
                if let Ok(pruned) = &result {
                    for device in &pruned.labels {
                        info!(
                            "label removed for {}: device no longer stored or seen",
                            device
                        );
                        self.publish(device, None);
                    }
                }
                let _ = reply.send(result);
            }
        }
    }

    /// Tell every `/ws/labels` client about a saved change.
    fn publish(&self, device: &str, label: Option<&str>) {
        // Fails only when no client is connected, which is not an error.
        let _ = self.labels_tx.send(make_label_json(device, label));
    }

    #[cfg(test)]
    async fn hold(&self) {
        if let Some(gate) = &self.gate {
            gate.entered.notify_one();
            gate.release
                .acquire()
                .await
                .expect("the gate's semaphore is never closed")
                .forget();
        }
    }
}

/// Run a blocking storage operation off the async runtime.
async fn blocking<T, F>(operation: F) -> Result<T, WriterError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, SnifferError> + Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(result) => result.map_err(WriterError::Storage),
        Err(e) => Err(WriterError::Task(e)),
    }
}

/// Test-only valve between the writer taking a command and saving it.
///
/// The cancellation tests need the writer to be provably mid-command while a
/// request is dropped, which no amount of sleeping can guarantee.
#[cfg(test)]
pub(crate) struct Gate {
    /// Notified each time the writer has taken a command and is waiting here.
    pub(crate) entered: tokio::sync::Notify,
    /// Each permit lets one command through.
    pub(crate) release: tokio::sync::Semaphore,
}

#[cfg(test)]
impl Gate {
    pub(crate) fn closed() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::time::timeout;

    use super::*;
    use crate::core::metadata::{
        CrcStatus, Direction, MType, MessageRecord, Protocol as Proto, Status,
    };

    const WINDOW: Duration = Duration::from_secs(3600);
    /// Failure guard for waits that should be immediate.
    const GUARD: Duration = Duration::from_secs(10);

    const ADDR: &str = "00E8A80B";
    const EUI: &str = "36:31:39:54:36:5D:05:39";

    struct Rig {
        storage: Storage,
        seen: SeenDevices,
        events: broadcast::Receiver<Arc<String>>,
        cancel: CancellationToken,
        writer: LabelWriter,
    }

    fn rig(max_labels: usize, gate: Option<Arc<Gate>>) -> Rig {
        let storage = Storage::open_in_memory().unwrap();
        let seen = SeenDevices::new();
        let (labels_tx, events) = broadcast::channel(64);
        let cancel = CancellationToken::new();
        let writer = LabelWriter::spawn_for_test(
            storage.clone(),
            seen.clone(),
            WINDOW,
            labels_tx,
            max_labels,
            gate,
            cancel.clone(),
        );
        Rig {
            storage,
            seen,
            events,
            cancel,
            writer,
        }
    }

    impl Rig {
        /// The events published so far, as `(device, label)` pairs.
        fn drain(&mut self) -> Vec<(String, Option<String>)> {
            let mut out = Vec::new();
            while let Ok(json) = self.events.try_recv() {
                let frame: Value = serde_json::from_str(&json).unwrap();
                assert_eq!(frame["type"], json!("label"));
                out.push((
                    frame["data"]["device"].as_str().unwrap().to_string(),
                    frame["data"]["label"].as_str().map(str::to_string),
                ));
            }
            out
        }

        fn labels(&self) -> BTreeMap<String, String> {
            self.storage.labels().unwrap()
        }
    }

    fn pair(device: &str, label: Option<&str>) -> (String, Option<String>) {
        (device.to_string(), label.map(str::to_string))
    }

    fn record_at(dev_addr: &str, timestamp: DateTime<Utc>) -> MessageRecord {
        MessageRecord {
            id: format!("{dev_addr}-{}", timestamp.timestamp_nanos_opt().unwrap()),
            timestamp,
            direction: Direction::Up,
            status: Status::Captured,
            blocked_by: None,
            source_protocol: Proto::Udp,
            freq_mhz: 868.1,
            datarate: "SF7BW125".to_string(),
            rssi: None,
            snr: None,
            crc_status: Some(CrcStatus::Ok),
            mtype: MType::UnconfirmedDataUp,
            dev_addr: Some(dev_addr.to_string()),
            dev_eui: None,
            fcnt: Some(1),
            fport: Some(1),
            payload_size: 4,
            toa_ms: None,
            gateway_eui: None,
            join_eui: None,
            dev_nonce: None,
            adr: None,
            adr_ack_req: None,
            ack: None,
            class_b: None,
            f_pending: None,
            f_opts_len: None,
            malformed: None,
        }
    }

    #[tokio::test]
    async fn every_effective_change_publishes_one_event_in_order() {
        let mut rig = rig(MAX_LABELS, None);
        rig.seen.note(ADDR);
        rig.seen.note(EUI);

        assert_eq!(
            rig.writer.set(ADDR.into(), "first".into()).await.unwrap(),
            LabelWrite::Set
        );
        // The same text again is still a save, and still published: clients
        // apply events by identifier, so repeating one is harmless.
        assert_eq!(
            rig.writer.set(ADDR.into(), "first".into()).await.unwrap(),
            LabelWrite::Set
        );
        assert_eq!(
            rig.writer.set(ADDR.into(), "second".into()).await.unwrap(),
            LabelWrite::Set
        );
        assert_eq!(
            rig.writer.set(EUI.into(), "eui".into()).await.unwrap(),
            LabelWrite::Set
        );
        assert!(rig.writer.delete(ADDR.into()).await.unwrap());

        // The writer publishes before it answers, so everything is already here.
        assert_eq!(
            rig.drain(),
            vec![
                pair(ADDR, Some("first")),
                pair(ADDR, Some("first")),
                pair(ADDR, Some("second")),
                pair(EUI, Some("eui")),
                pair(ADDR, None),
            ]
        );
        assert_eq!(rig.labels(), BTreeMap::from([(EUI.into(), "eui".into())]));
    }

    #[tokio::test]
    async fn deleting_an_absent_label_publishes_nothing() {
        let mut rig = rig(MAX_LABELS, None);

        assert!(!rig.writer.delete(ADDR.into()).await.unwrap());

        assert!(rig.drain().is_empty());
    }

    #[tokio::test]
    async fn refused_writes_publish_nothing() {
        let mut rig = rig(1, None);
        rig.seen.note(ADDR);
        rig.seen.note(EUI);

        // Neither stored nor seen live.
        assert_eq!(
            rig.writer.set("DEADBEEF".into(), "x".into()).await.unwrap(),
            LabelWrite::UnknownDevice
        );
        assert_eq!(
            rig.writer.set(ADDR.into(), "one".into()).await.unwrap(),
            LabelWrite::Set
        );
        // A second new label at a limit of one.
        assert_eq!(
            rig.writer.set(EUI.into(), "two".into()).await.unwrap(),
            LabelWrite::LimitReached
        );

        assert_eq!(rig.drain(), vec![pair(ADDR, Some("one"))]);
        assert_eq!(rig.writer.max_labels(), 1);
    }

    #[tokio::test]
    async fn label_text_is_carried_as_json_not_pasted_into_it() {
        let mut rig = rig(MAX_LABELS, None);
        rig.seen.note(ADDR);
        let nasty = r#"say "hi" \ <b>é€😀</b>"#;

        rig.writer.set(ADDR.into(), nasty.into()).await.unwrap();

        assert_eq!(rig.drain(), vec![pair(ADDR, Some(nasty))]);
    }

    #[tokio::test]
    async fn concurrent_sets_publish_in_commit_order() {
        let mut rig = rig(MAX_LABELS, None);
        rig.seen.note(ADDR);

        let tasks: Vec<_> = (0..40)
            .map(|i| {
                let writer = rig.writer.clone();
                tokio::spawn(async move { writer.set(ADDR.into(), format!("v{i}")).await })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), LabelWrite::Set);
        }

        let events = rig.drain();
        assert_eq!(events.len(), 40);
        // The row holds whatever was committed last, and that is the event
        // that was published last.
        let stored = rig.labels().remove(ADDR).unwrap();
        assert_eq!(events.last().unwrap().1.as_deref(), Some(stored.as_str()));
    }

    #[tokio::test]
    async fn prune_publishes_one_removal_per_label_removed() {
        let mut rig = rig(MAX_LABELS, None);
        let old = Utc::now() - chrono::Duration::hours(2);
        // A has a record that stays, B an old record that goes, C was seen
        // live, D is an orphan nobody has seen.
        rig.storage
            .insert(&record_at("0000000A", Utc::now()))
            .unwrap();
        rig.storage.insert(&record_at("0000000B", old)).unwrap();
        for device in ["0000000A", "0000000B", "0000000C", "0000000D"] {
            rig.storage
                .set_label(device, &format!("label {device}"), true, MAX_LABELS)
                .unwrap();
        }
        rig.seen.note("0000000C");

        let cutoff = Utc::now() - chrono::Duration::hours(1);
        let pruned = rig.writer.prune(cutoff, false).await.unwrap();

        assert_eq!(pruned.messages, 1);
        let mut removed = pruned.labels.clone();
        removed.sort();
        assert_eq!(removed, ["0000000B", "0000000D"]);
        let mut events = rig.drain();
        events.sort();
        assert_eq!(events, vec![pair("0000000B", None), pair("0000000D", None)]);
        assert_eq!(
            rig.labels().keys().cloned().collect::<Vec<_>>(),
            ["0000000A", "0000000C"]
        );
    }

    #[tokio::test]
    async fn prune_keeping_orphans_removes_no_label() {
        let mut rig = rig(MAX_LABELS, None);
        rig.storage
            .set_label(ADDR, "orphan", true, MAX_LABELS)
            .unwrap();

        let pruned = rig.writer.prune(Utc::now(), true).await.unwrap();

        assert!(pruned.labels.is_empty());
        assert!(rig.drain().is_empty());
        assert_eq!(rig.labels().len(), 1);
    }

    /// Wait until the writer has taken a command and is held at the gate.
    async fn held(gate: &Gate) {
        timeout(GUARD, gate.entered.notified())
            .await
            .expect("the writer never reached the gate");
    }

    #[tokio::test]
    async fn a_command_queued_before_cancel_is_finished_and_later_ones_are_refused() {
        let gate = Gate::closed();
        let mut rig = rig(MAX_LABELS, Some(gate.clone()));
        rig.seen.note(ADDR);
        rig.seen.note(EUI);

        let first = tokio::spawn({
            let writer = rig.writer.clone();
            async move { writer.set(ADDR.into(), "in flight".into()).await }
        });
        held(&gate).await;
        // Queued behind the held one: it is never taken.
        let second = tokio::spawn({
            let writer = rig.writer.clone();
            async move { writer.set(EUI.into(), "never taken".into()).await }
        });
        // Let the second request reach the queue before the writer stops.
        tokio::task::yield_now().await;
        rig.cancel.cancel();
        gate.release.add_permits(1);

        assert_eq!(first.await.unwrap().unwrap(), LabelWrite::Set);
        assert!(matches!(second.await.unwrap(), Err(WriterError::Stopped)));
        assert_eq!(rig.drain(), vec![pair(ADDR, Some("in flight"))]);
        assert_eq!(rig.labels().len(), 1);
        // Once stopped, new requests fail straight away.
        assert!(matches!(
            rig.writer.delete(ADDR.into()).await,
            Err(WriterError::Stopped)
        ));
    }
}
