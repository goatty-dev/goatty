//! A connection's writer thread, with interactive priority (#8).
//!
//! With several panes on one link, a flooding pane's backlog used to sit in
//! front of another pane's echo in the one FIFO queue: up to Σ windows /
//! bandwidth of delay. The writer now keeps one queue per [`Lane`] (each
//! session, replies, file transfers) and shares the link between them by
//! self-clocked fair queueing: each frame is tagged with the virtual time its
//! lane would finish it at, `max(lane's last tag, now) + len`, and the
//! smallest tag goes next. A quiet pane's echo or prompt is tagged a few
//! bytes past now and jumps ahead of a flooding pane's backlog, floods share
//! the link, and a large snapshot waits a bounded time. Each lane keeps its
//! own order, so a session's stream is never reordered.

use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::os::unix::net::UnixStream;

use flume::TryRecvError;

use crate::client::{Lane, Outbound};

/// A frame and its place in line: (virtual finish time, arrival order).
type Tag = (u64, u64);

struct Queue {
    lane: Lane,
    frames: VecDeque<(Tag, Outbound)>,
    /// The virtual finish time of the last frame queued here.
    finish: u64,
}

/// The frames the writer has taken off the channel but not written yet.
#[derive(Default)]
struct Lanes {
    queues: Vec<Queue>,
    /// Virtual time: the tag of the frame written last.
    now: u64,
    /// Arrival order, to break ties.
    seq: u64,
}

impl Lanes {
    fn is_empty(&self) -> bool {
        self.queues.is_empty()
    }

    fn push(&mut self, out: Outbound) {
        self.seq += 1;
        let i = match self.queues.iter().position(|q| q.lane == out.lane) {
            Some(i) => i,
            None => {
                self.queues.push(Queue { lane: out.lane, frames: VecDeque::new(), finish: 0 });
                self.queues.len() - 1
            }
        };
        let q = &mut self.queues[i];
        q.finish = q.finish.max(self.now) + out.frame.len() as u64;
        q.frames.push_back(((q.finish, self.seq), out));
    }

    /// The next frame to write: the queued head with the smallest tag. Also
    /// says whether that emptied its lane while other lanes still wait: then
    /// it jumped ahead of them.
    fn pop(&mut self) -> Option<(Outbound, bool)> {
        let i = (0..self.queues.len()).min_by_key(|&i| self.queues[i].frames.front().map(|f| f.0))?;
        let q = &mut self.queues[i];
        let ((finish, _), out) = q.frames.pop_front()?;
        self.now = self.now.max(finish);
        let drained = q.frames.is_empty();
        if drained {
            self.queues.swap_remove(i);
        }
        Some((out, drained && !self.queues.is_empty()))
    }
}

/// Write `rx`'s frames to `stream` until the connection ends or is told to
/// stop. An empty frame is a barrier: everything queued before it is
/// written and flushed, then `flushed` hears of it.
pub(crate) fn run(rx: &flume::Receiver<Outbound>, stream: &UnixStream, flushed: &flume::Sender<()>) {
    let mut w = BufWriter::with_capacity(128 << 10, stream);
    let mut lanes = Lanes::default();
    let mut barrier = false;
    let mut open = true;
    loop {
        // Take everything queued, up to a barrier; block only when idle.
        while open && !barrier {
            let next = if lanes.is_empty() { rx.recv().map_err(|_| TryRecvError::Disconnected) } else { rx.try_recv() };
            let out = match next {
                Ok(out) => out,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    open = false;
                    break;
                }
            };
            if out.stop {
                return;
            }
            if out.frame.is_empty() {
                barrier = true;
            } else {
                lanes.push(out);
            }
        }
        if let Some((out, jumped)) = lanes.pop() {
            if w.write_all(&out.frame).is_err() {
                return;
            }
            // Drop `out` (and any `ByteGuard`) as soon as it is written.
            drop(out);
            // Batch under load and flush the moment the queue is idle. A
            // frame that jumped ahead goes out now, not behind 128 KiB of the
            // backlog it skipped.
            if (jumped || lanes.is_empty() && rx.is_empty()) && w.flush().is_err() {
                return;
            }
        } else if barrier {
            if w.flush().is_err() {
                return;
            }
            let _ = flushed.try_send(());
            barrier = false;
        } else if !open {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use goatty_proto::Frame;

    use super::*;

    fn frame(tag: u8, len: usize) -> Frame {
        Frame::from(vec![tag; len])
    }

    fn order(lanes: &mut Lanes) -> Vec<u8> {
        std::iter::from_fn(|| lanes.pop()).map(|(o, _)| o.frame[0]).collect()
    }

    #[test]
    fn a_quiet_lane_jumps_ahead_and_each_lane_keeps_its_order() {
        let mut lanes = Lanes::default();
        for tag in [1, 2, 3] {
            lanes.push(Outbound::session(frame(tag, 64 << 10), 1));
        }
        lanes.push(Outbound::session(frame(10, 3), 2));
        lanes.push(Outbound::session(frame(11, 3), 2));
        lanes.push(Outbound::from(frame(20, 16)));
        lanes.push(Outbound::file(frame(30, 1 << 20)));
        lanes.push(Outbound::from(frame(21, 16)));
        // Session 2's echo and the replies go first, then the flood, in
        // order; the file transfer doesn't hold up the replies.
        assert_eq!(order(&mut lanes), [10, 11, 20, 21, 1, 2, 3, 30]);
    }

    #[test]
    fn floods_share_the_link() {
        let mut lanes = Lanes::default();
        for tag in [1, 2, 3] {
            lanes.push(Outbound::session(frame(tag, 1000), 1));
        }
        for tag in [11, 12] {
            lanes.push(Outbound::session(frame(tag, 1000), 2));
        }
        assert_eq!(order(&mut lanes), [1, 11, 2, 12, 3]);
    }

    #[test]
    fn a_big_frame_is_not_starved_by_a_flood() {
        let mut lanes = Lanes::default();
        lanes.push(Outbound::session(frame(1, 64 << 10), 1));
        lanes.push(Outbound::session(frame(9, 256 << 10), 2));
        // The flood keeps its backlog topped up as it is written.
        let mut sent = Vec::new();
        for _ in 0..8 {
            lanes.push(Outbound::session(frame(1, 64 << 10), 1));
            sent.push(lanes.pop().unwrap().0.frame[0]);
        }
        assert!(sent.contains(&9), "the snapshot waited behind the whole flood: {sent:?}");
    }

    #[test]
    fn one_lane_is_fifo() {
        let mut lanes = Lanes::default();
        for (tag, len) in [(1, 10), (2, 50_000), (3, 1)] {
            lanes.push(Outbound::session(frame(tag, len), 7));
        }
        assert_eq!(order(&mut lanes), [1, 2, 3]);
    }

    #[test]
    fn the_writer_sends_echo_before_a_queued_flood() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let (tx, rx) = flume::unbounded();
        for _ in 0..16 {
            tx.send(Outbound::session(frame(b'x', 64 << 10), 1)).unwrap();
        }
        tx.send(Outbound::session(frame(b'e', 1), 2)).unwrap();
        tx.send(Outbound::from(Frame::from(&[][..]))).unwrap();
        let (flushed_tx, flushed_rx) = flume::bounded(1);
        let writer = std::thread::spawn(move || run(&rx, &a, &flushed_tx));
        let mut first = [0u8; 1];
        b.read_exact(&mut first).unwrap();
        assert_eq!(first[0], b'e', "the echo waited behind the flood");
        let mut rest = vec![0u8; 16 * (64 << 10)];
        b.read_exact(&mut rest).unwrap();
        assert!(rest.iter().all(|&c| c == b'x'));
        flushed_rx.recv().unwrap();
        drop(tx);
        writer.join().unwrap();
    }
}
