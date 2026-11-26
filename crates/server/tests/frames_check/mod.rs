//! Checks what a client receives on a subscription that switches between
//! the stream and screen frames: every record exactly once and in order
//! while on the stream, records up to exactly the sequence a switch to
//! frames names and none after it, frames numbered without a gap, and a
//! checkpoint at exactly the sequence a switch back names.

#![allow(dead_code)]

use std::collections::VecDeque;

use client::Pushed;
use prost::Message as _;

#[derive(Debug)]
enum State {
    /// Records after `applied`; until the checkpoint is `restored`, they
    /// are held (they may overtake it).
    Stream {
        applied: u64,
        restored: bool,
        held: Vec<u64>,
    },
    Frames {
        number: u64,
        sequence: u64,
    },
}

#[derive(Debug)]
pub struct Checker {
    sub: u64,
    replies: VecDeque<api::FeedSwitch>,
    state: State,
    pub records: usize,
    pub frames: usize,
    pub full_frames: usize,
    pub checkpoints: usize,
    /// The screen as the frames built it.
    pub view: replica::screen::ScreenView,
}

impl Checker {
    pub fn new(s: &api::Subscription) -> Checker {
        let mut c = Checker {
            sub: s.id,
            replies: VecDeque::new(),
            state: State::Stream {
                applied: s.sequence,
                restored: false,
                held: Vec::new(),
            },
            records: 0,
            frames: 0,
            full_frames: 0,
            checkpoints: 0,
            view: replica::screen::ScreenView::new(),
        };
        if s.mode == api::SubscribeMode::Frames as i32 {
            // As if switched to frames from a stream at that sequence.
            c.state = State::Stream {
                applied: s.sequence,
                restored: true,
                held: Vec::new(),
            };
            c.switched(api::FeedSwitch {
                subscription: s.id,
                mode: s.mode,
                sequence: s.sequence,
                frame: s.frame,
            });
        } else {
            assert_eq!(s.mode, api::SubscribeMode::Checkpoint as i32, "{s:?}");
        }
        c
    }

    /// A switch's reply, in the order the switches were made.
    pub fn switched(&mut self, r: api::FeedSwitch) {
        assert_eq!(r.subscription, self.sub);
        self.replies.push_back(r);
    }

    /// Every switch answered has been seen through.
    pub fn settled(&self) -> bool {
        self.replies.is_empty()
            && !matches!(
                self.state,
                State::Stream {
                    restored: false,
                    ..
                }
            )
    }

    pub fn on_frames(&self) -> bool {
        matches!(self.state, State::Frames { .. })
    }

    fn next_reply(&mut self, mode: api::SubscribeMode) -> api::FeedSwitch {
        let r = self
            .replies
            .pop_front()
            .unwrap_or_else(|| panic!("data of another feed with no switch: {:?}", self.state));
        assert_eq!(r.mode, mode as i32, "{r:?}, state {:?}", self.state);
        r
    }

    pub fn push(&mut self, p: Pushed) {
        match p {
            Pushed::Event { topic, payload, .. } if topic == "terminal.records" => {
                let b = api::RecordBatch::decode(payload.as_slice()).expect("batch");
                assert_eq!(b.subscription, self.sub);
                for r in b.records {
                    self.record(r.sequence);
                }
            }
            Pushed::Bulk { topic, data, .. } if topic == "terminal.checkpoint" => {
                let d = api::CheckpointData::decode(data.as_slice()).expect("checkpoint");
                assert_eq!(d.subscription, self.sub);
                self.checkpoint(d.sequence);
            }
            Pushed::Bulk { topic, data, .. } if topic == "terminal.frame" => {
                let f = api::ScreenFrame::decode(data.as_slice()).expect("frame");
                assert_eq!(f.subscription, self.sub);
                self.frame(&f);
            }
            Pushed::Event { topic, payload, .. } if topic == "terminal.end" => {
                panic!(
                    "the subscription ended: {:?}",
                    api::SubscriptionEnd::decode(payload.as_slice())
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    fn record(&mut self, seq: u64) {
        self.records += 1;
        match &mut self.state {
            State::Stream {
                restored: false,
                held,
                ..
            } => held.push(seq),
            State::Stream { applied, .. } => {
                assert_eq!(seq, *applied + 1, "a gap or a duplicate in the records");
                *applied = seq;
            }
            State::Frames { sequence, .. } => {
                // Records overtook the switch's checkpoint.
                let last = *sequence;
                let r = self.next_reply(api::SubscribeMode::Checkpoint);
                assert!(r.sequence >= last, "{r:?} is before frame at {last}");
                self.state = State::Stream {
                    applied: r.sequence,
                    restored: false,
                    held: vec![seq],
                };
            }
        }
    }

    fn checkpoint(&mut self, seq: u64) {
        self.checkpoints += 1;
        match &mut self.state {
            State::Stream {
                applied,
                restored: restored @ false,
                held,
            } => {
                assert_eq!(seq, *applied, "a checkpoint at another sequence");
                *restored = true;
                for h in std::mem::take(held) {
                    assert_eq!(h, *applied + 1, "records after a checkpoint skip or repeat");
                    *applied = h;
                }
            }
            State::Stream { .. } => panic!("a checkpoint with no switch to the stream"),
            State::Frames { sequence, .. } => {
                let last = *sequence;
                let r = self.next_reply(api::SubscribeMode::Checkpoint);
                assert_eq!(seq, r.sequence);
                assert!(seq >= last, "checkpoint {seq} is before frame at {last}");
                self.state = State::Stream {
                    applied: seq,
                    restored: true,
                    held: Vec::new(),
                };
            }
        }
    }

    fn frame(&mut self, f: &api::ScreenFrame) {
        self.frames += 1;
        if f.full {
            self.full_frames += 1;
        }
        match &mut self.state {
            State::Stream {
                applied,
                restored,
                held,
            } => {
                let (applied, restored, held) = (*applied, *restored, held.is_empty());
                let r = self.next_reply(api::SubscribeMode::Frames);
                assert!(f.full, "the first frame after a switch is full");
                assert_eq!((f.number, f.sequence), (r.frame, r.sequence), "{r:?}");
                assert!(restored && held, "frames before the stream's checkpoint");
                assert_eq!(
                    applied, r.sequence,
                    "records must reach exactly the switch to frames, and stop there"
                );
            }
            State::Frames { number, sequence } => {
                assert_eq!(f.number, *number + 1, "a gap in the frame numbers");
                assert!(f.sequence >= *sequence, "a frame went back in sequence");
            }
        }
        self.view.apply(f).expect("frame applies");
        self.state = State::Frames {
            number: f.number,
            sequence: f.sequence,
        };
    }
}
