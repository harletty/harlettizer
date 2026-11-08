//! A unit's substreams coded on threads of their own, each thread its share
//! of them, the unit written once every share is back.
//!
//! A substream's coding depends on nothing but its own samples, its number
//! and — for Opus — its own encoder's state, so the shares can be coded at
//! once: each substream is coded by exactly the calls one thread makes, in
//! the same order, and the writer lays the frames out in the order it always
//! does. The bytes are the same whichever thread coded what.

use crate::stream::{Coder, Substream, carried, code};
use std::io;
use std::sync::mpsc;
use std::thread::JoinHandle;

/// One thread's substreams, and the coder they share: FLAC's holds nothing
/// from one frame to the next, and Opus has an encoder per substream.
struct Share {
    coder: Coder,
    /// Each substream's channels, as indices into its batch's.
    substreams: Vec<Vec<usize>>,
    bits: u32,
}

impl Share {
    fn run(&mut self, batch: &mut Batch) {
        batch.payload.clear();
        batch.ends.clear();
        batch.failed = None;
        for (local, indices) in self.substreams.iter().enumerate() {
            let (slices, count) = carried(&batch.channels, indices);
            if let Err(e) = code(
                &mut self.coder,
                local,
                &slices[..count],
                batch.number,
                self.bits,
                &mut batch.payload,
            ) {
                // As on one thread: the substreams after it are not coded.
                batch.failed = Some((local, e));
                return;
            }
            batch.ends.push(batch.payload.len());
        }
    }
}

/// A unit as one share sees it: its channels, and what they were coded to.
struct Batch {
    /// The unit's number, which a FLAC frame states.
    number: u64,
    /// The share's channels, a frame each.
    channels: Vec<Vec<i32>>,
    /// Its substreams' payloads, back to back.
    payload: Vec<u8>,
    /// Where each one's ends in `payload`.
    ends: Vec<usize>,
    /// The first that could not be coded, by its place in the share, and why.
    failed: Option<(usize, io::Error)>,
}

/// One thread and its share.
struct Hand {
    inbox: Option<mpsc::SyncSender<Batch>>,
    outbox: mpsc::Receiver<Batch>,
    /// Here between units, at the thread during one.
    batch: Option<Batch>,
    /// The writer's channels its batch carries, in the batch's order.
    channels: Vec<usize>,
    thread: Option<JoinHandle<()>>,
}

/// The threads coding a writer's substreams.
pub(crate) struct Crew {
    hands: Vec<Hand>,
    /// Each substream's hand, and its place among that hand's.
    at: Vec<(usize, usize)>,
}

impl Crew {
    /// `coder`'s substreams shared out among `hands` threads, or `coder`
    /// back where a thread cannot be started — boxed, a coder being large
    /// and this said once a writer.
    pub(crate) fn start(
        coder: Coder,
        substreams: &[Substream],
        hands: usize,
        frame: usize,
        bits: u32,
    ) -> Result<Self, Box<Coder>> {
        // Every thread first, each waiting for its share: one that cannot
        // be started leaves every share, and so `coder`, here.
        let mut started = Vec::with_capacity(hands);
        for hand in 0..hands {
            let (give, take) = mpsc::channel::<Share>();
            let (inbox, inputs) = mpsc::sync_channel::<Batch>(1);
            let (results, outbox) = mpsc::channel::<Batch>();
            let spawned = std::thread::Builder::new()
                .name(format!("iamf-{hand}"))
                .spawn(move || {
                    let Ok(mut share) = take.recv() else {
                        return;
                    };
                    for mut batch in inputs {
                        share.run(&mut batch);
                        if results.send(batch).is_err() {
                            break;
                        }
                    }
                });
            match spawned {
                Ok(thread) => started.push((give, inbox, outbox, thread)),
                Err(_) => {
                    for (give, _, _, thread) in started {
                        drop(give);
                        let _ = thread.join();
                    }
                    return Err(Box::new(coder));
                }
            }
        }

        // The longest first, each to the hand with least to do: a pair is
        // twice a mono substream's work.
        let mut order: Vec<usize> = (0..substreams.len()).collect();
        order.sort_by_key(|&s| std::cmp::Reverse(substreams[s].channels.len()));
        let mut load = vec![0usize; hands];
        let mut owner = vec![0usize; substreams.len()];
        for s in order {
            let hand = (0..hands).min_by_key(|&h| load[h]).expect("a hand");
            owner[s] = hand;
            load[hand] += substreams[s].channels.len();
        }
        // Each hand's substreams in the writer's order, which is the order
        // one thread codes them in.
        let mut mine: Vec<Vec<usize>> = vec![Vec::new(); hands];
        let mut at = vec![(0, 0); substreams.len()];
        for (s, &hand) in owner.iter().enumerate() {
            at[s] = (hand, mine[hand].len());
            mine[hand].push(s);
        }

        let mut crew = Self {
            hands: Vec::with_capacity(hands),
            at,
        };
        let shares = started.into_iter().zip(&mine).zip(split(coder, &mine));
        for (((give, inbox, outbox, thread), substreams_of), coder) in shares {
            let mut channels = Vec::new();
            let local: Vec<Vec<usize>> = substreams_of
                .iter()
                .map(|&s| {
                    substreams[s]
                        .channels
                        .iter()
                        .map(|&c| {
                            channels.push(c);
                            channels.len() - 1
                        })
                        .collect()
                })
                .collect();
            let share = Share {
                coder,
                substreams: local,
                bits,
            };
            // The thread is waiting for it: started, it has not ended.
            give.send(share)
                .unwrap_or_else(|_| unreachable!("a started hand waits for its share"));
            crew.hands.push(Hand {
                inbox: Some(inbox),
                outbox,
                batch: Some(Batch {
                    number: 0,
                    channels: vec![vec![0; frame]; channels.len()],
                    payload: Vec::new(),
                    ends: Vec::new(),
                    failed: None,
                }),
                channels,
                thread: Some(thread),
            });
        }
        Ok(crew)
    }

    /// Code unit `number` of `channels`, every share at once, and wait for
    /// them all.
    pub(crate) fn code(&mut self, channels: &[Vec<i32>], number: u64) {
        for hand in &mut self.hands {
            let mut batch = hand
                .batch
                .take()
                .expect("a hand's batch is back between units");
            batch.number = number;
            for (to, &from) in batch.channels.iter_mut().zip(&hand.channels) {
                to.copy_from_slice(&channels[from]);
            }
            let sent = hand.inbox.as_ref().map(|inbox| inbox.send(batch));
            if !matches!(sent, Some(Ok(()))) {
                panicked(&mut hand.thread);
            }
        }
        for hand in &mut self.hands {
            match hand.outbox.recv() {
                Ok(batch) => hand.batch = Some(batch),
                Err(_) => panicked(&mut hand.thread),
            }
        }
    }

    /// What substream `substream` of the unit just coded was coded to, or
    /// why it could not be. Asked in the writer's order, an error is met
    /// where one thread would have met it.
    pub(crate) fn coded(&mut self, substream: usize) -> io::Result<&[u8]> {
        let (hand, i) = self.at[substream];
        let batch = self.hands[hand]
            .batch
            .as_mut()
            .expect("a hand's batch is back between units");
        if batch
            .failed
            .as_ref()
            .is_some_and(|(failed, _)| *failed <= i)
        {
            let (_, e) = batch.failed.take().expect("just seen");
            return Err(e);
        }
        if i >= batch.ends.len() {
            // Asked again past a failure already handed over.
            return Err(io::Error::other(
                "a substream before it in this unit could not be coded",
            ));
        }
        let start = if i == 0 { 0 } else { batch.ends[i - 1] };
        Ok(&batch.payload[start..batch.ends[i]])
    }
}

impl Drop for Crew {
    fn drop(&mut self) {
        for hand in &mut self.hands {
            drop(hand.inbox.take());
        }
        for hand in &mut self.hands {
            if let Some(thread) = hand.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

/// One coder for each hand, holding what its substreams need.
fn split(coder: Coder, mine: &[Vec<usize>]) -> Vec<Coder> {
    match coder {
        Coder::Lpcm => mine.iter().map(|_| Coder::Lpcm).collect(),
        // Nothing carries over from one frame to the next: a copy codes
        // what the original does.
        Coder::Flac(encoder) => mine.iter().map(|_| Coder::Flac(encoder.clone())).collect(),
        #[cfg(feature = "opus")]
        Coder::Opus { encoders, scratch } => {
            let mut encoders: Vec<Option<_>> = encoders.into_iter().map(Some).collect();
            mine.iter()
                .map(|substreams| Coder::Opus {
                    encoders: substreams
                        .iter()
                        .map(|&s| encoders[s].take().expect("each substream in one share"))
                        .collect(),
                    scratch: Vec::with_capacity(scratch.capacity()),
                })
                .collect()
        }
    }
}

/// A hand's thread is gone before its work is, which only a panic does:
/// carry it on here.
fn panicked(thread: &mut Option<JoinHandle<()>>) -> ! {
    match thread.take().map(JoinHandle::join) {
        Some(Err(panic)) => std::panic::resume_unwind(panic),
        _ => panic!("a coding thread ended before its work did"),
    }
}
