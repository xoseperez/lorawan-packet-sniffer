//! What the storage writer's queue carries.
//!
//! The proxy queues every captured record for the writer, and a history
//! request that must not miss any of them queues a marker behind them.  The
//! writer takes the items strictly in order and finishes each one before it
//! takes the next, so when it answers a marker, everything queued before the
//! marker has been processed.
//!
//! "Processed" is not "saved": a record whose insert failed has been logged
//! and counted as a failure, and is not in the database.  A marker's answer
//! therefore means that the database holds every record from before the
//! marker that it will ever hold; the failure counter says how many were lost.

use tokio::sync::{oneshot, OwnedSemaphorePermit};

use crate::core::metadata::MessageRecord;

/// An item in the storage writer's queue.
// The queue's slots are as big as the biggest variant either way; boxing the
// record would only add an allocation for every record on the forwarding path.
#[allow(clippy::large_enum_variant)]
pub enum WriterItem {
    /// A record to save.
    Record(MessageRecord),
    /// A marker: the writer answers it once every item queued before it has
    /// been processed.
    Settle {
        /// Answered by the writer; the requester may have stopped waiting, in
        /// which case the answer goes nowhere.
        done: oneshot::Sender<()>,
        /// The single permit of the semaphore that admits markers.  It lives
        /// exactly as long as the marker does, whoever is still waiting, so
        /// no more than one marker is ever in the queue: a requester that
        /// gave up and asked again cannot pile a second one behind a stalled
        /// writer.
        permit: OwnedSemaphorePermit,
    },
}

#[cfg(test)]
pub(crate) use tests_support::{record_channel, RecordReceiver};

/// Queue ends for tests that only care about the records.
#[cfg(test)]
mod tests_support {
    use tokio::sync::mpsc::{self, error::TryRecvError};

    use super::WriterItem;
    use crate::core::metadata::MessageRecord;

    /// The receiving end of a writer queue that hands out the records and
    /// fails on a marker, which no test of the record paths queues.
    pub(crate) struct RecordReceiver(mpsc::Receiver<WriterItem>);

    /// A writer queue of `capacity` items.
    pub(crate) fn record_channel(capacity: usize) -> (mpsc::Sender<WriterItem>, RecordReceiver) {
        let (tx, rx) = mpsc::channel(capacity);
        (tx, RecordReceiver(rx))
    }

    fn record_of(item: WriterItem) -> MessageRecord {
        match item {
            WriterItem::Record(record) => record,
            WriterItem::Settle { .. } => panic!("a marker in a queue that only holds records"),
        }
    }

    impl RecordReceiver {
        pub(crate) async fn recv(&mut self) -> Option<MessageRecord> {
            self.0.recv().await.map(record_of)
        }

        pub(crate) fn try_recv(&mut self) -> Result<MessageRecord, TryRecvError> {
            self.0.try_recv().map(record_of)
        }
    }
}
