//! A step of an encode on a thread of its own, its caller handing it work
//! and taking back what it made, in order: the same results as on the
//! caller's thread, where one cannot be started.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::thread::JoinHandle;

/// Work that keeps its own state from one input to the next and hands back
/// a result for each, in order.
pub(crate) trait Stage: Send + 'static {
    type In: Send + 'static;
    type Out: Send + 'static;
    fn run(&mut self, input: Self::In) -> Self::Out;
}

/// Inputs a stage's thread may be behind by before its caller waits for it.
pub(crate) const LANE_DEPTH: usize = 4;

/// A [`Stage`] on a thread of its own, working while its caller does; or,
/// where no thread can be started, on the caller's, as each input comes. The
/// same results either way, in the same order.
pub(crate) enum Lane<S: Stage> {
    Thread {
        inbox: Option<mpsc::SyncSender<S::In>>,
        outbox: mpsc::Receiver<S::Out>,
        handle: Option<JoinHandle<Option<S>>>,
    },
    Here {
        stage: S,
        done: VecDeque<S::Out>,
    },
}

impl<S: Stage> Lane<S> {
    pub(crate) fn new(mut stage: S, name: &str, threads: bool) -> Self {
        if threads {
            let (inbox, inputs) = mpsc::sync_channel(LANE_DEPTH);
            let (results, outbox) = mpsc::channel();
            // The stage goes to the thread once it has started: a thread that
            // cannot be leaves it here.
            let (give, take) = mpsc::channel::<S>();
            let spawned = std::thread::Builder::new()
                .name(name.to_string())
                .spawn(move || {
                    let mut stage = take.recv().ok()?;
                    for input in inputs {
                        if results.send(stage.run(input)).is_err() {
                            break;
                        }
                    }
                    Some(stage)
                });
            if let Ok(handle) = spawned {
                match give.send(stage) {
                    Ok(()) => {
                        return Self::Thread {
                            inbox: Some(inbox),
                            outbox,
                            handle: Some(handle),
                        };
                    }
                    Err(mpsc::SendError(back)) => stage = back,
                }
            }
        }
        Self::Here {
            stage,
            done: VecDeque::new(),
        }
    }

    pub(crate) fn send(&mut self, input: S::In) {
        match self {
            Self::Thread { inbox, .. } => {
                // A thread gone has panicked; waiting for its result says so.
                if let Some(inbox) = inbox {
                    let _ = inbox.send(input);
                }
            }
            Self::Here { stage, done } => done.push_back(stage.run(input)),
        }
    }

    /// The oldest result not yet taken, waited for.
    pub(crate) fn recv(&mut self) -> S::Out {
        match self {
            Self::Thread { outbox, handle, .. } => match outbox.recv() {
                Ok(out) => out,
                Err(_) => panicked(handle),
            },
            Self::Here { done, .. } => done.pop_front().expect("a result for every input"),
        }
    }

    /// The oldest result not yet taken, if it is ready.
    pub(crate) fn try_recv(&mut self) -> Option<S::Out> {
        match self {
            Self::Thread { outbox, handle, .. } => match outbox.try_recv() {
                Ok(out) => Some(out),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => panicked(handle),
            },
            Self::Here { done, .. } => done.pop_front(),
        }
    }

    /// The stage's thread ended, its stage back on the caller's.
    pub(crate) fn close(&mut self) {
        if let Self::Thread { inbox, handle, .. } = self {
            drop(inbox.take());
            let stage = match handle.take().map(JoinHandle::join) {
                Some(Ok(Some(stage))) => stage,
                Some(Err(panic)) => std::panic::resume_unwind(panic),
                _ => unreachable!("a stage's thread is joined once, and has its stage"),
            };
            *self = Self::Here {
                stage,
                done: VecDeque::new(),
            };
        }
    }

    /// The stage, once it is on the caller's thread.
    pub(crate) fn stage(&self) -> Option<&S> {
        match self {
            Self::Thread { .. } => None,
            Self::Here { stage, .. } => Some(stage),
        }
    }

    /// The stage itself, its thread ended first.
    pub(crate) fn into_stage(mut self) -> S {
        self.close();
        match self {
            Self::Here { stage, .. } => stage,
            Self::Thread { .. } => unreachable!("a closed lane is the caller's"),
        }
    }
}

/// `work` on a thread of its own in `scope`, `run` doing it there, when
/// `threads` and one can be started: `None`, the thread joined when the scope
/// ends. Otherwise `work` itself, for the caller to do on its own thread.
///
/// What a [`Lane`] is for work that borrows from its caller and drives itself
/// rather than taking inputs one by one. The same hand-over: `work` goes to
/// the thread once it has started, so that one which cannot be leaves it
/// here, where [`std::thread::Scope::spawn`] would panic.
pub(crate) fn beside<'scope, W: Send + 'scope>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    threads: bool,
    work: W,
    run: impl FnOnce(W) + Send + 'scope,
) -> Option<W> {
    if !threads {
        return Some(work);
    }
    let (give, take) = mpsc::channel::<W>();
    // Unnamed, as `Scope::spawn` would have started it.
    let spawned = std::thread::Builder::new().spawn_scoped(scope, move || {
        if let Ok(work) = take.recv() {
            run(work);
        }
    });
    match spawned {
        Ok(_) => give.send(work).err().map(|mpsc::SendError(back)| back),
        Err(_) => Some(work),
    }
}

/// A stage's thread is gone before its work is, which only a panic does:
/// carry it on here.
fn panicked<S>(handle: &mut Option<JoinHandle<S>>) -> ! {
    match handle.take().map(JoinHandle::join) {
        Some(Err(panic)) => std::panic::resume_unwind(panic),
        _ => panic!("a stage's thread ended before its work did"),
    }
}
