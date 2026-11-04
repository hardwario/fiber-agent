//! Taking a draft's frames back out of a device's downlink queue (P-6,
//! `plan/portal/nodes_phase1.cs.md`, dodatek 2026-09-25).
//!
//! ChirpStack v4 has no per-item delete: the queue is read (`GetQueue`), flushed
//! (`FlushQueue`) and the items that are not being withdrawn are enqueued again in their
//! order. A command frame (fPort 85) is recognised by its `seq`, field 1 of the `Command`
//! message the frame is ([`crate::decode::command_encode::Downlink::encode`]).

use crate::decode::command_encode::COMMAND_FPORT;
use crate::decode::pb::{read_tag, read_varint};
use crate::error::Result;
use crate::lorawan::client::ChirpStack;
use crate::lorawan::eui::Eui;
use crate::lorawan::model::QueuedDownlink;

/// The `seq` of a command frame as it was enqueued, or `None` when it carries none.
pub fn downlink_seq(data: &[u8]) -> Option<u32> {
    let tag = read_tag(data, 0);
    (tag.field == 1 && tag.wire == 0).then(|| read_varint(data, tag.next).value)
}

/// Removes the device's queued command frames whose `seq` is in `seqs`, keeping every other
/// item in order. Returns how many were removed; the queue is left untouched when none match.
///
/// Not atomic: an item ChirpStack sends between the read and the flush is gone either way,
/// and one enqueued in that gap is flushed with the rest. Both windows are one RPC wide.
pub async fn drop_queued_seqs(
    client: &dyn ChirpStack,
    dev_eui: &Eui,
    seqs: &[u32],
) -> Result<usize> {
    let queue = client.get_queue(dev_eui).await?;
    let withdrawn = |q: &QueuedDownlink| {
        q.f_port == COMMAND_FPORT && downlink_seq(&q.data).is_some_and(|s| seqs.contains(&s))
    };
    rewrite(client, dev_eui, queue, withdrawn).await
}

/// Coalescing's withdrawal on LoRaWAN (plan.md §5 `node-send`, amended 2026-09-28): the waiting
/// command frames `pick` names by `(seq, bytes)` leave the queue as [`drop_queued_seqs`] takes
/// them, from the one queue read `pick` saw. Returns the seqs that went.
///
/// A frame ChirpStack already has on the air (`is_pending`) is not withdrawn, and while there
/// is one nothing is: without a per-item delete the rest of the queue is flushed and enqueued
/// again, and the pending item with it would be a new item that can go out a second time,
/// whatever command it is. The new frame then queues behind, as it did before coalescing.
pub async fn withdraw_waiting(
    client: &dyn ChirpStack,
    dev_eui: &Eui,
    pick: impl Fn(u32, &[u8]) -> bool,
) -> Result<Vec<u32>> {
    let queue = client.get_queue(dev_eui).await?;
    if queue.iter().any(|q| q.is_pending) {
        return Ok(Vec::new());
    }
    let picked = |q: &QueuedDownlink| {
        q.f_port == COMMAND_FPORT && downlink_seq(&q.data).is_some_and(|s| pick(s, &q.data))
    };
    let seqs: Vec<u32> = queue
        .iter()
        .filter(|q| picked(q))
        .filter_map(|q| downlink_seq(&q.data))
        .collect();
    rewrite(client, dev_eui, queue, picked).await?;
    Ok(seqs)
}

/// Flush the device's queue and enqueue again, in order, every item of `queue` that is not
/// `withdrawn`. Returns how many were withdrawn; the queue is left untouched when none are.
async fn rewrite(
    client: &dyn ChirpStack,
    dev_eui: &Eui,
    queue: Vec<QueuedDownlink>,
    withdrawn: impl Fn(&QueuedDownlink) -> bool,
) -> Result<usize> {
    let removed = queue.iter().filter(|q| withdrawn(q)).count();
    if removed == 0 {
        return Ok(0);
    }
    client.flush_queue(dev_eui).await?;
    for item in queue.into_iter().filter(|q| !withdrawn(q)) {
        client
            .enqueue_downlink(dev_eui, item.f_port, item.data, item.confirmed)
            .await?;
    }
    Ok(removed)
}

/// A Discard's withdrawal on LoRaWAN (PF-3b): the frames go as with [`drop_queued_seqs`], and
/// the answer also says whether a named frame was no longer queued. ChirpStack drops an
/// unconfirmed downlink from the queue once it is transmitted, so a missing frame is one the
/// Node may have received — the LoRaWAN counterpart of P2P's delivered (`Sent`) head. Returns
/// `(removed, maybe_delivered)`.
pub async fn withdraw_queued_seqs(
    client: &dyn ChirpStack,
    dev_eui: &Eui,
    seqs: &[u32],
) -> Result<(usize, bool)> {
    let queue = client.get_queue(dev_eui).await?;
    let queued: Vec<u32> = queue
        .iter()
        .filter(|q| q.f_port == COMMAND_FPORT && !q.is_pending)
        .filter_map(|q| downlink_seq(&q.data))
        .collect();
    let maybe_delivered = seqs.iter().any(|s| !queued.contains(s))
        || queue.iter().any(|q| {
            q.f_port == COMMAND_FPORT
                && q.is_pending
                && downlink_seq(&q.data).is_some_and(|s| seqs.contains(&s))
        });
    let removed = drop_queued_seqs(client, dev_eui, seqs).await?;
    Ok((removed, maybe_delivered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::command_encode::Downlink;
    use crate::lorawan::fake::FakeChirpStack;
    use crate::Transport;

    fn eui() -> Eui {
        Eui::parse("5876070000000413").expect("eui")
    }

    #[test]
    fn a_command_frame_names_its_seq() {
        let frame = Downlink::ReqHistory {
            from_unix: None,
            to_unix: None,
        }
        .encode(166, Transport::Lorawan)
        .expect("encodes");
        assert_eq!(downlink_seq(&frame), Some(166));
        assert_eq!(downlink_seq(&[0x12, 0x00]), None, "no seq field first");
    }

    #[tokio::test]
    async fn only_the_named_frames_leave_the_queue_and_the_rest_keep_their_order() {
        let fake = FakeChirpStack::new();
        let frame = |seq| {
            Downlink::ReqHistory {
                from_unix: None,
                to_unix: None,
            }
            .encode(seq, Transport::Lorawan)
            .expect("encodes")
        };
        for (port, data) in [
            (COMMAND_FPORT, frame(10)),
            (COMMAND_FPORT, frame(11)),
            (2, vec![0x08, 11]), // not a command frame, even though it reads like seq 11
            (COMMAND_FPORT, frame(12)),
        ] {
            fake.enqueue_downlink(&eui(), port, data, false)
                .await
                .expect("enqueue");
        }

        let removed = drop_queued_seqs(&fake, &eui(), &[11]).await.expect("drop");
        assert_eq!(removed, 1);
        let left: Vec<(u8, Option<u32>)> = fake
            .get_queue(&eui())
            .await
            .expect("queue")
            .iter()
            .map(|q| (q.f_port, downlink_seq(&q.data)))
            .collect();
        assert_eq!(left, vec![(85, Some(10)), (2, Some(11)), (85, Some(12))]);

        let before = fake.downlinks().len();
        assert_eq!(
            drop_queued_seqs(&fake, &eui(), &[99]).await.expect("drop"),
            0
        );
        assert_eq!(
            fake.downlinks().len(),
            before,
            "nothing matched, nothing re-enqueued"
        );
    }

    /// PF-3b: a Discard's withdrawal removes the queued frame and says the Node may have one
    /// that already left the queue (transmitted).
    #[tokio::test]
    async fn a_discard_withdraws_what_is_queued_and_names_what_already_left() {
        let fake = FakeChirpStack::new();
        let frame = |seq| {
            Downlink::ReqHistory {
                from_unix: None,
                to_unix: None,
            }
            .encode(seq, Transport::Lorawan)
            .expect("encodes")
        };
        fake.enqueue_downlink(&eui(), COMMAND_FPORT, frame(20), false)
            .await
            .expect("enqueue");
        assert_eq!(
            withdraw_queued_seqs(&fake, &eui(), &[20])
                .await
                .expect("withdraw"),
            (1, false),
            "still queued: the Node has nothing of it"
        );
        fake.enqueue_downlink(&eui(), COMMAND_FPORT, frame(22), false)
            .await
            .expect("enqueue");
        assert_eq!(
            withdraw_queued_seqs(&fake, &eui(), &[21, 22])
                .await
                .expect("withdraw"),
            (1, true),
            "21 already went out"
        );
        assert!(fake.get_queue(&eui()).await.expect("queue").is_empty());
    }
}
