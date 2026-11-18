//! The worker's output path to stdout. Control messages and video pictures
//! share one writer thread, but not one queue:
//!
//! - Control messages go through a bounded queue and always go out first.
//! - A picture goes into a slot for its tile. A new picture replaces the one
//!   that still waits, so at most one picture per tile is held, and a picture
//!   never waits in front of a control message. The slot write never blocks.
//!
//! A small wake item in the control queue tells the writer that a picture
//! waits. If the queue is full, the writer is busy and looks at the slots
//! anyway before it sleeps.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError};

use persephone_call_protocol::{FromWorker, VIDEO_TILES, write_message, write_video_frame};
use tokio::sync::mpsc::{self, WeakSender, error::TryRecvError};

use crate::video::rgba_len;

/// Worker messages waiting for the stdout writer.
pub const OUTPUT_CAPACITY: usize = 64;

/// What the writer thread reads from the control queue.
pub enum Item {
    Control(Queued),
    /// A picture waits in a slot.
    Wake,
}

/// A queued control message and the tile assignment it belongs to.
pub struct Queued {
    message: FromWorker,
    /// For a `VideoStarted`: its tile's assignment when the start was queued
    /// (`Slots::assignment`). Zero for every other message.
    assignment: u64,
}

/// The control queue is closed: the writer has gone.
#[derive(Debug)]
pub struct Closed;

/// One picture, ready for the wire.
struct Picture {
    rotation: u8,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

struct Slots {
    pictures: [Option<Picture>; VIDEO_TILES as usize],
    /// Tiles whose pictures the writer may take. A remote tile opens only
    /// when the writer has written its `VideoStarted`, and closes when it is
    /// cleared or its `VideoStopped` is written. So a picture of a new
    /// stream never goes out before the messages that announce it. The own
    /// tile (0) has no stream messages and is always open.
    open: [bool; VIDEO_TILES as usize],
    /// Each remote tile's current stream assignment. A clear ends it, so a
    /// `VideoStarted` queued before the clear is stale: written after it, the
    /// start does not open the tile.
    assignment: [u64; VIDEO_TILES as usize],
    /// The tile to look at first next time, so no tile starves the others.
    next: usize,
}

impl Default for Slots {
    fn default() -> Self {
        let mut open = [false; VIDEO_TILES as usize];
        open[usize::from(OWN_TILE)] = true;
        Self {
            pictures: Default::default(),
            open,
            assignment: [0; VIDEO_TILES as usize],
            next: 0,
        }
    }
}

/// The own camera's tile.
const OWN_TILE: u8 = 0;

/// The newest waiting picture of each tile.
#[derive(Default)]
pub struct VideoSlots {
    slots: Mutex<Slots>,
}

impl VideoSlots {
    fn lock(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores a picture and replaces the one that waits. Returns false and
    /// stores nothing when the tile, rotation or size is outside the limits.
    fn put(&self, tile: u8, rotation: u8, width: u32, height: u32, rgba: Vec<u8>) -> bool {
        let valid = tile < VIDEO_TILES
            && rotation <= persephone_call_protocol::MAX_VIDEO_ROTATION
            && rgba_len(width, height) == Some(rgba.len());
        if valid {
            self.lock().pictures[usize::from(tile)] = Some(Picture {
                rotation,
                width,
                height,
                rgba,
            });
        }
        valid
    }

    /// Drops the picture that waits for a tile. A remote tile stays closed
    /// until the writer writes a `VideoStarted` queued after this clear.
    pub fn clear(&self, tile: u8) {
        let mut slots = self.lock();
        let index = usize::from(tile);
        if index < slots.pictures.len() {
            slots.pictures[index] = None;
            if tile != OWN_TILE {
                slots.open[index] = false;
                slots.assignment[index] = slots.assignment[index].wrapping_add(1);
            }
        }
    }

    /// Binds a control message for the queue: a `VideoStarted` takes its
    /// tile's current assignment.
    fn queued(&self, message: FromWorker) -> Item {
        let assignment = match &message {
            FromWorker::VideoStarted { tile, .. } => self
                .lock()
                .assignment
                .get(usize::from(*tile))
                .copied()
                .unwrap_or(0),
            _ => 0,
        };
        Item::Control(Queued {
            message,
            assignment,
        })
    }

    /// The writer wrote `queued`: a remote tile's `VideoStarted` opens it
    /// only while the assignment it was queued with is current; its
    /// `VideoStopped` closes it.
    fn written(&self, queued: &Queued) {
        let (tile, open) = match &queued.message {
            FromWorker::VideoStarted { tile, .. } => (*tile, true),
            FromWorker::VideoStopped { tile } => (*tile, false),
            _ => return,
        };
        let index = usize::from(tile);
        let mut slots = self.lock();
        if tile == OWN_TILE || index >= slots.open.len() {
            return;
        }
        if !open || slots.assignment[index] == queued.assignment {
            slots.open[index] = open;
        }
    }

    /// Takes the next waiting picture, starting after the tile served last.
    fn take(&self) -> Option<(u8, Picture)> {
        let mut slots = self.lock();
        let count = slots.pictures.len();
        for offset in 0..count {
            let tile = (slots.next + offset) % count;
            if !slots.open[tile] {
                continue;
            }
            if let Some(picture) = slots.pictures[tile].take() {
                slots.next = (tile + 1) % count;
                return Some((tile as u8, picture));
            }
        }
        None
    }
}

/// Sends control messages. Cloning it shares the queue.
#[derive(Clone)]
pub struct Output {
    queue: mpsc::Sender<Item>,
    slots: Arc<VideoSlots>,
}

impl Output {
    /// Makes the output and the receiver for the writer thread.
    pub fn new() -> (Self, Receiver) {
        let (queue, receiver) = mpsc::channel(OUTPUT_CAPACITY);
        let slots = Arc::new(VideoSlots::default());
        (
            Self {
                queue,
                slots: slots.clone(),
            },
            Receiver { receiver, slots },
        )
    }

    /// Queues one control message. Waits when the queue is full.
    pub async fn send(&self, message: FromWorker) -> Result<(), Closed> {
        // The assignment is read now: a clear while the send waits for room
        // makes the start stale.
        let item = self.slots.queued(message);
        self.queue.send(item).await.map_err(|_| Closed)
    }

    /// A handle for picture producers. It does not keep the writer alive.
    pub fn pictures(&self) -> PictureSink {
        PictureSink {
            queue: self.queue.downgrade(),
            slots: self.slots.clone(),
        }
    }

    /// Drops the picture that waits for a tile. Call it after the tile's
    /// producer has stopped and before `VideoStopped`.
    pub fn clear_picture(&self, tile: u8) {
        self.slots.clear(tile);
    }
}

/// Puts pictures into their slots. It never blocks, and it holds only a weak
/// handle on the control queue: a producer that outlives the call cannot keep
/// the writer thread, and so the process, from ending.
#[derive(Clone)]
pub struct PictureSink {
    queue: WeakSender<Item>,
    slots: Arc<VideoSlots>,
}

impl PictureSink {
    /// Offers a picture; it replaces an older one that has not gone out. A
    /// picture outside the video limits is dropped.
    pub fn put(&self, tile: u8, rotation: u8, width: u32, height: u32, rgba: Vec<u8>) {
        if !self.slots.put(tile, rotation, width, height, rgba) {
            return;
        }
        if let Some(queue) = self.queue.upgrade() {
            // A full queue means the writer is busy and will look at the slots.
            let _ = queue.try_send(Item::Wake);
        }
    }
}

/// The writer thread's side.
pub struct Receiver {
    receiver: mpsc::Receiver<Item>,
    slots: Arc<VideoSlots>,
}

/// Writes until every sender is gone and the queue is empty. Control messages
/// go first; one picture goes out between two looks at the queue. Returns
/// false when a write failed.
pub fn write_loop<W: Write>(mut out: W, mut input: Receiver) -> bool {
    loop {
        let mut closed = false;
        loop {
            match input.receiver.try_recv() {
                Ok(Item::Control(queued)) => {
                    if write_message(&mut out, &queued.message).is_err() {
                        return false;
                    }
                    input.slots.written(&queued);
                }
                Ok(Item::Wake) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        if let Some((tile, picture)) = input.slots.take() {
            if write_picture(&mut out, tile, &picture).is_err() {
                return false;
            }
            continue;
        }
        if closed {
            return true;
        }
        match input.receiver.blocking_recv() {
            Some(Item::Control(queued)) => {
                if write_message(&mut out, &queued.message).is_err() {
                    return false;
                }
                input.slots.written(&queued);
            }
            Some(Item::Wake) => {}
            None => return true,
        }
    }
}

fn write_picture<W: Write>(out: &mut W, tile: u8, picture: &Picture) -> io::Result<()> {
    write_video_frame(
        out,
        tile,
        picture.rotation,
        picture.width,
        picture.height,
        &picture.rgba,
    )
}

/// The stdout writer thread's entry: locks stdout for the life of the call.
pub fn write_stdout(input: Receiver) -> bool {
    write_loop(io::stdout().lock(), input)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use persephone_call_protocol::{WorkerItem, read_worker_item};
    use persephone_media_protocol::ResourceBudgets;

    use super::*;

    fn picture(width: u32, height: u32, fill: u8) -> Vec<u8> {
        vec![fill; rgba_len(width, height).unwrap()]
    }

    /// Queues `message` and writes it at once.
    fn wrote(slots: &VideoSlots, message: FromWorker) {
        let Item::Control(queued) = slots.queued(message) else {
            unreachable!()
        };
        slots.written(&queued);
    }

    fn started(tile: u8) -> FromWorker {
        FromWorker::VideoStarted {
            tile,
            identity: "@a:example.org:A".to_owned(),
        }
    }

    /// GIVEN a remote tile WHEN a picture waits before the writer wrote the
    /// tile's `VideoStarted` (the writer saw an empty queue, then the session
    /// queued the start and the reader filled the slot) THEN the slot does
    /// not give it out; once the start is written it does, and a cleared or
    /// stopped tile closes again.
    #[test]
    fn given_a_remote_picture_then_it_waits_for_its_written_start() {
        let slots = VideoSlots::default();
        assert!(slots.put(1, 0, 1, 1, picture(1, 1, 1)));
        assert!(slots.take().is_none(), "the start is not written yet");
        wrote(&slots, started(1));
        assert!(matches!(slots.take(), Some((1, _))));
        // A handover: the old stream is cleared, a new picture arrives.
        slots.clear(1);
        assert!(slots.put(1, 0, 1, 1, picture(1, 1, 2)));
        assert!(slots.take().is_none(), "closed until the next start");
        wrote(&slots, FromWorker::VideoStopped { tile: 1 });
        assert!(slots.take().is_none());
        wrote(&slots, started(1));
        assert!(matches!(slots.take(), Some((1, _))));
        // The own tile has no stream messages and stays open.
        slots.clear(0);
        assert!(slots.put(0, 0, 1, 1, picture(1, 1, 3)));
        assert!(matches!(slots.take(), Some((0, _))));
    }

    /// GIVEN a remote tile's `VideoStarted` queued WHEN the tile is cleared
    /// before the writer writes it THEN the written start does not open the
    /// tile, so the next stream's picture waits for its own start.
    #[test]
    fn given_a_start_queued_before_a_clear_then_it_cannot_open_the_tile() {
        let slots = VideoSlots::default();
        let Item::Control(stale) = slots.queued(started(1)) else {
            unreachable!()
        };
        slots.clear(1);
        slots.written(&stale);
        assert!(slots.put(1, 0, 1, 1, picture(1, 1, 1)));
        assert!(slots.take().is_none(), "the stale start opened the tile");
        wrote(&slots, FromWorker::VideoStopped { tile: 1 });
        assert!(slots.take().is_none());
        wrote(&slots, started(1));
        assert!(matches!(slots.take(), Some((1, _))));
    }

    /// GIVEN a handover queued as stop then start, with the new stream's
    /// picture already in the slot WHEN the writer runs THEN the picture goes
    /// out after both messages.
    #[test]
    fn given_a_handover_when_written_then_the_picture_follows_its_start() {
        let (output, receiver) = Output::new();
        let sink = output.pictures();
        sink.put(1, 0, 1, 1, picture(1, 1, 4));
        output
            .queue
            .try_send(output.slots.queued(FromWorker::VideoStopped { tile: 1 }))
            .unwrap();
        output
            .queue
            .try_send(output.slots.queued(started(1)))
            .unwrap();
        drop(sink);
        drop(output);
        let mut wire = Vec::new();
        assert!(write_loop(&mut wire, receiver));
        let items = read_all(wire);
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0],
            WorkerItem::Message(FromWorker::VideoStopped { tile: 1 })
        );
        assert_eq!(items[1], WorkerItem::Message(started(1)));
        assert!(matches!(items[2], WorkerItem::Video { tile: 1, .. }));
    }

    fn read_all(wire: Vec<u8>) -> Vec<WorkerItem> {
        let budgets = ResourceBudgets::default();
        let total = wire.len() as u64;
        let mut reader = Cursor::new(wire);
        let mut items = Vec::new();
        while reader.position() < total {
            items.push(read_worker_item(&mut reader, &budgets).expect("valid output"));
        }
        items
    }

    /// GIVEN a picture waiting in a slot and control messages queued after it
    /// WHEN the writer runs THEN every control message goes out before the
    /// picture.
    #[test]
    fn given_waiting_picture_when_writer_runs_then_control_goes_first() {
        let (output, receiver) = Output::new();
        let sink = output.pictures();
        sink.put(0, 1, 4, 2, picture(4, 2, 7));
        output
            .queue
            .try_send(output.slots.queued(FromWorker::Left))
            .unwrap();
        output
            .queue
            .try_send(output.slots.queued(FromWorker::VideoStopped { tile: 3 }))
            .unwrap();
        drop(sink);
        drop(output);
        let mut wire = Vec::new();
        assert!(write_loop(&mut wire, receiver));
        let items = read_all(wire);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], WorkerItem::Message(FromWorker::Left));
        assert_eq!(
            items[1],
            WorkerItem::Message(FromWorker::VideoStopped { tile: 3 })
        );
        let WorkerItem::Video {
            tile,
            rotation,
            width,
            height,
            rgba,
        } = &items[2]
        else {
            panic!("not a picture");
        };
        assert_eq!((*tile, *rotation, *width, *height), (0, 1, 4, 2));
        assert_eq!(rgba.as_slice(), picture(4, 2, 7).as_slice());
    }

    /// GIVEN several pictures for one tile before the writer runs WHEN it runs
    /// THEN only the newest goes out.
    #[test]
    fn given_busy_writer_when_pictures_pile_up_then_only_the_newest_stays() {
        let (output, receiver) = Output::new();
        let sink = output.pictures();
        for fill in 1..=50 {
            sink.put(0, 0, 2, 2, picture(2, 2, fill));
        }
        drop(sink);
        drop(output);
        let mut wire = Vec::new();
        assert!(write_loop(&mut wire, receiver));
        let items = read_all(wire);
        assert_eq!(items.len(), 1);
        let WorkerItem::Video { rgba, .. } = &items[0] else {
            panic!("not a picture");
        };
        assert_eq!(rgba.as_slice(), picture(2, 2, 50).as_slice());
    }

    /// GIVEN a full control queue WHEN a picture is offered THEN the offer does
    /// not block, and the picture still goes out after the queue drains.
    #[test]
    fn given_full_queue_when_picture_offered_then_it_does_not_block() {
        let (output, receiver) = Output::new();
        for _ in 0..OUTPUT_CAPACITY {
            output
                .queue
                .try_send(output.slots.queued(FromWorker::Left))
                .unwrap();
        }
        let sink = output.pictures();
        sink.put(0, 0, 1, 1, picture(1, 1, 9));
        drop(sink);
        drop(output);
        let mut wire = Vec::new();
        assert!(write_loop(&mut wire, receiver));
        let items = read_all(wire);
        assert_eq!(items.len(), OUTPUT_CAPACITY + 1);
        assert!(matches!(
            items.last(),
            Some(WorkerItem::Video { tile: 0, .. })
        ));
    }

    /// GIVEN pictures for several tiles WHEN the writer runs THEN the tiles take
    /// turns, starting after the one served last.
    #[test]
    fn given_pictures_on_many_tiles_when_taken_then_tiles_take_turns() {
        let slots = VideoSlots::default();
        for tile in [3, 4] {
            wrote(&slots, started(tile));
        }
        for tile in [0, 3, 4] {
            assert!(slots.put(tile, 0, 1, 1, picture(1, 1, tile)));
        }
        let first: Vec<u8> = std::iter::from_fn(|| slots.take().map(|(tile, _)| tile)).collect();
        assert_eq!(first, vec![0, 3, 4]);
        assert!(slots.put(3, 0, 1, 1, picture(1, 1, 1)));
        assert!(slots.put(0, 0, 1, 1, picture(1, 1, 2)));
        // The last tile served was 4, so 0 comes before 3.
        let second: Vec<u8> = std::iter::from_fn(|| slots.take().map(|(tile, _)| tile)).collect();
        assert_eq!(second, vec![0, 3]);
    }

    /// GIVEN pictures outside the limits WHEN they are offered THEN none is
    /// kept.
    #[test]
    fn given_bad_pictures_when_offered_then_they_are_dropped() {
        let slots = VideoSlots::default();
        assert!(!slots.put(VIDEO_TILES, 0, 1, 1, picture(1, 1, 0)));
        assert!(!slots.put(0, 4, 1, 1, picture(1, 1, 0)));
        assert!(!slots.put(0, 0, 1, 1, vec![0; 3]));
        assert!(!slots.put(0, 0, 641, 1, vec![0; 641 * 4]));
        assert!(!slots.put(0, 0, 0, 1, Vec::new()));
        assert!(slots.take().is_none());
    }

    /// GIVEN a waiting picture WHEN its tile is cleared THEN it is not written.
    #[test]
    fn given_waiting_picture_when_tile_cleared_then_nothing_is_written() {
        let (output, receiver) = Output::new();
        let sink = output.pictures();
        sink.put(0, 0, 1, 1, picture(1, 1, 5));
        output.clear_picture(0);
        output.clear_picture(200);
        drop(sink);
        drop(output);
        let mut wire = Vec::new();
        assert!(write_loop(&mut wire, receiver));
        assert!(wire.is_empty());
    }

    /// GIVEN a sink that outlives the output WHEN it offers a picture THEN
    /// nothing panics and the writer has already ended.
    #[test]
    fn given_sink_after_output_dropped_when_offering_then_it_is_harmless() {
        let (output, receiver) = Output::new();
        let sink = output.pictures();
        drop(output);
        let mut wire = Vec::new();
        assert!(write_loop(&mut wire, receiver));
        sink.put(0, 0, 1, 1, picture(1, 1, 1));
        assert!(wire.is_empty());
    }

    /// GIVEN a writer that fails WHEN it writes THEN the loop reports failure.
    #[test]
    fn given_failing_writer_when_writing_then_loop_reports_failure() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (output, receiver) = Output::new();
        output
            .queue
            .try_send(output.slots.queued(FromWorker::Left))
            .unwrap();
        drop(output);
        assert!(!write_loop(Broken, receiver));
    }
}
