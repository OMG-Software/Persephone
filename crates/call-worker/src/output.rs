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
    Control(FromWorker),
    /// A picture waits in a slot.
    Wake,
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

#[derive(Default)]
struct Slots {
    pictures: [Option<Picture>; VIDEO_TILES as usize],
    /// The tile to look at first next time, so no tile starves the others.
    next: usize,
}

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

    /// Drops the picture that waits for a tile.
    pub fn clear(&self, tile: u8) {
        if let Some(slot) = self.lock().pictures.get_mut(usize::from(tile)) {
            *slot = None;
        }
    }

    /// Takes the next waiting picture, starting after the tile served last.
    fn take(&self) -> Option<(u8, Picture)> {
        let mut slots = self.lock();
        let count = slots.pictures.len();
        for offset in 0..count {
            let tile = (slots.next + offset) % count;
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
        self.queue
            .send(Item::Control(message))
            .await
            .map_err(|_| Closed)
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
                Ok(Item::Control(message)) => {
                    if write_message(&mut out, &message).is_err() {
                        return false;
                    }
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
            Some(Item::Control(message)) => {
                if write_message(&mut out, &message).is_err() {
                    return false;
                }
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
        sink.put(2, 1, 4, 2, picture(4, 2, 7));
        output
            .queue
            .try_send(Item::Control(FromWorker::Left))
            .unwrap();
        output
            .queue
            .try_send(Item::Control(FromWorker::VideoStopped { tile: 3 }))
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
        assert_eq!(
            items[2],
            WorkerItem::Video {
                tile: 2,
                rotation: 1,
                width: 4,
                height: 2,
                rgba: picture(4, 2, 7),
            }
        );
    }

    /// GIVEN several pictures for one tile before the writer runs WHEN it runs
    /// THEN only the newest goes out.
    #[test]
    fn given_busy_writer_when_pictures_pile_up_then_only_the_newest_stays() {
        let (output, receiver) = Output::new();
        let sink = output.pictures();
        for fill in 1..=50 {
            sink.put(1, 0, 2, 2, picture(2, 2, fill));
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
        assert_eq!(rgba, &picture(2, 2, 50));
    }

    /// GIVEN a full control queue WHEN a picture is offered THEN the offer does
    /// not block, and the picture still goes out after the queue drains.
    #[test]
    fn given_full_queue_when_picture_offered_then_it_does_not_block() {
        let (output, receiver) = Output::new();
        for _ in 0..OUTPUT_CAPACITY {
            output
                .queue
                .try_send(Item::Control(FromWorker::Left))
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
        sink.put(1, 0, 1, 1, picture(1, 1, 5));
        output.clear_picture(1);
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
            .try_send(Item::Control(FromWorker::Left))
            .unwrap();
        drop(output);
        assert!(!write_loop(Broken, receiver));
    }
}
