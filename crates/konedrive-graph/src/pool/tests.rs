use super::*;

/// Holds `n` slots of `class`, taken without waiting.
fn take(pool: &Arc<TransferPool>, class: Class, n: usize) -> Vec<Slot> {
    take_sized(pool, class, Size::Small, n)
}

fn take_sized(pool: &Arc<TransferPool>, class: Class, size: Size, n: usize) -> Vec<Slot> {
    (0..n).map(|_| pool.try_acquire_sized(class, size).expect("a free slot")).collect()
}

/// A waiter of `class`, polled once so that it stands in line.
fn queue(pool: &Arc<TransferPool>, class: Class) -> Pin<Box<Acquire>> {
    queue_sized(pool, class, Size::Small)
}

fn queue_sized(pool: &Arc<TransferPool>, class: Class, size: Size) -> Pin<Box<Acquire>> {
    let mut acquire = Box::pin(pool.acquire_sized(class, size));
    let waker = futures_util::task::noop_waker();
    assert!(acquire.as_mut().poll(&mut Context::from_waker(&waker)).is_pending(), "{class:?} {size:?} waits");
    acquire
}

fn ready(acquire: &mut Pin<Box<Acquire>>) -> Option<Slot> {
    let waker = futures_util::task::noop_waker();
    match acquire.as_mut().poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(slot) => Some(slot),
        Poll::Pending => None,
    }
}

/// Among 30 000 waiters the pool grants in the order they came, and one in
/// the middle that gives up leaves the line as it was around it.
#[test]
fn thirty_thousand_waiters_are_granted_in_order_and_one_gives_up() {
    let pool = TransferPool::starting_at(1, 1);
    let mut held = take(&pool, Class::Download, 1);
    let mut waiting: Vec<Option<Pin<Box<Acquire>>>> = (0..30_000).map(|_| Some(queue(&pool, Class::Download))).collect();
    drop(waiting[15_000].take());
    for n in (0..30_000).filter(|&n| n != 15_000) {
        drop(held.pop());
        for later in [n + 1, 29_999].into_iter().filter(|&l| l != 15_000 && l > n && l < 30_000) {
            assert!(ready(waiting[later].as_mut().unwrap()).is_none(), "{later} waits behind {n}");
        }
        let slot = ready(waiting[n].as_mut().unwrap()).unwrap_or_else(|| panic!("{n} is next"));
        held.push(slot);
        waiting[n] = None;
    }
    assert!(!pool.waiting());
}

/// One success, with work waiting and every slot busy, is one slot more; a success
/// with nothing waiting is nothing.
#[test]
fn it_grows_on_a_success_only_while_work_waits_and_every_slot_is_busy() {
    let pool = TransferPool::starting_at(4, 64);
    let mut slots = take(&pool, Class::Download, 4);
    let _waiting = queue(&pool, Class::Download);
    let mut done = slots.pop().unwrap();
    done.succeeded();
    drop(done);
    assert_eq!(pool.size(), 5, "busy and work waiting: one more");

    let pool = TransferPool::starting_at(4, 64);
    let mut slots = take(&pool, Class::Download, 4);
    let mut done = slots.pop().unwrap();
    done.succeeded();
    drop(done);
    assert_eq!(pool.size(), 4, "nothing waits: idle gaps change nothing");

    let pool = TransferPool::starting_at(4, 64);
    let mut slots = take(&pool, Class::Download, 2);
    let mut done = slots.pop().unwrap();
    done.succeeded();
    drop(done);
    assert_eq!(pool.size(), 4, "slots free: nothing");

    let pool = TransferPool::starting_at(4, 4);
    let mut slots = take(&pool, Class::Download, 4);
    let _waiting = queue(&pool, Class::Download);
    let mut done = slots.pop().unwrap();
    done.succeeded();
    drop(done);
    assert_eq!(pool.size(), 4, "never above the ceiling");
}

/// A failure is no reason to grow.
#[test]
fn a_failed_transfer_does_not_grow_the_pool() {
    let pool = TransferPool::starting_at(2, 64);
    let mut slots = take(&pool, Class::Upload, 2);
    let _waiting = queue(&pool, Class::Upload);
    drop(slots.pop());
    assert_eq!(pool.size(), 2);
}

/// `429`: half the pool, nothing handed out for the wait, once per burst; then the level
/// it came at is remembered: fast growth below it, one slot per round at and above it,
/// and fast again once it is forgotten.
#[tokio::test(start_paused = true)]
async fn a_throttle_halves_the_pool_and_its_level_slows_growth_until_forgotten() {
    let pool = TransferPool::starting_at(8, 64);
    pool.throttled(Some(Duration::from_secs(5)));
    assert_eq!(pool.size(), 4);
    assert_eq!(pool.throughput().retry_after, 5, "the wait is published");
    assert!(pool.try_acquire(Class::Open).is_none(), "nothing during Retry-After, not even an open");
    pool.throttled(Some(Duration::from_secs(5)));
    assert_eq!(pool.size(), 4, "the same burst halves once");
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(pool.try_acquire(Class::Download).is_some(), "slots again after the wait");
    assert_eq!(pool.throughput().retry_after, 0);

    // Below the level: one per success.
    let grow_once = |pool: &Arc<TransferPool>| {
        let size = pool.size();
        let mut slots = take(pool, Class::Download, size);
        let waiting = queue(pool, Class::Download);
        let mut done = slots.pop().unwrap();
        done.succeeded();
        drop(done);
        drop(waiting);
        drop(slots);
    };
    for _ in 0..4 {
        grow_once(&pool);
    }
    assert_eq!(pool.size(), 8, "fast up to the level the throttle came at");
    for _ in 0..7 {
        grow_once(&pool);
    }
    assert_eq!(pool.size(), 8, "at the level: not before a whole round");
    grow_once(&pool);
    assert_eq!(pool.size(), 9, "one slot after as many successes as slots");

    tokio::time::sleep(THROTTLE_MEMORY).await;
    grow_once(&pool);
    assert_eq!(pool.size(), 10, "forgotten after five minutes: fast again");
    grow_once(&pool);
    assert_eq!(pool.size(), 11);
}

/// A throttle with the pool at 1 leaves it at 1.
#[tokio::test(start_paused = true)]
async fn the_pool_never_halves_below_one() {
    let pool = TransferPool::starting_at(1, 64);
    pool.throttled(None);
    assert_eq!(pool.size(), 1);
}

/// An open goes first and may use the reserve above the pool; while one is under way,
/// background work takes no new slot.
#[test]
fn an_open_goes_first_uses_the_reserve_and_holds_background_work_back() {
    let pool = TransferPool::starting_at(2, 64);
    let background = take(&pool, Class::Download, 2);
    let mut upload = queue(&pool, Class::Upload);
    let open = take(&pool, Class::Open, RESERVE);
    assert!(pool.try_acquire(Class::Open).is_none(), "the reserve is two slots");

    drop(background);
    assert!(ready(&mut upload).is_none(), "an open is under way: background work waits");
    let more = pool.try_acquire(Class::Open);
    assert!(more.is_some(), "freed slots go to opens");
    drop((open, more));
    assert!(ready(&mut upload).is_some(), "no open left: background work goes");
}

/// An open waiting in line holds background work back too, and gets the next slot.
#[test]
fn a_waiting_open_takes_the_next_free_slot() {
    let pool = TransferPool::starting_at(1, 64);
    let _open = take(&pool, Class::Open, 1 + RESERVE);
    let mut waiting_open = queue(&pool, Class::Open);
    let mut download = queue(&pool, Class::Download);
    drop(_open);
    let open = ready(&mut waiting_open);
    assert!(open.is_some());
    assert!(ready(&mut download).is_none(), "while the open runs");
    drop(open);
    assert!(ready(&mut download).is_some());
}

/// Downloads and uploads take turns while both wait; one alone takes every slot.
#[test]
fn downloads_and_uploads_alternate_while_both_wait() {
    let pool = TransferPool::starting_at(1, 64);
    let held = take(&pool, Class::Download, 1);
    let mut waiting: Vec<(Direction, Option<Pin<Box<Acquire>>>)> = (0..3)
        .map(|_| (Direction::Down, Some(queue(&pool, Class::Download))))
        .chain((0..3).map(|_| (Direction::Up, Some(queue(&pool, Class::Upload)))))
        .collect();
    drop(held);
    let mut order = Vec::new();
    for _ in 0..6 {
        let (direction, slot) = waiting
            .iter_mut()
            .find_map(|(direction, acquire)| {
                let slot = ready(acquire.as_mut()?)?;
                *acquire = None;
                Some((*direction, slot))
            })
            .expect("one slot is handed out");
        order.push(direction);
        drop(slot);
    }
    assert_eq!(
        order,
        [Direction::Up, Direction::Down, Direction::Up, Direction::Down, Direction::Up, Direction::Down],
        "the slot held was a download's: uploads' turn first"
    );

    let pool = TransferPool::starting_at(3, 64);
    let alone = take(&pool, Class::Upload, 3);
    assert_eq!(alone.len(), 3, "only uploads: every slot");
}

/// Metadata goes before background transfers; a pause holds back everything but opens.
#[test]
fn metadata_goes_first_and_a_pause_holds_back_all_but_opens() {
    let pool = TransferPool::starting_at(1, 64);
    let held = take(&pool, Class::Upload, 1);
    let mut download = queue(&pool, Class::Download);
    let mut meta = queue(&pool, Class::Metadata);
    drop(held);
    assert!(ready(&mut download).is_none());
    assert!(ready(&mut meta).is_some());

    let pool = TransferPool::starting_at(4, 64);
    pool.set_paused(true);
    assert!(pool.try_acquire(Class::Upload).is_none());
    assert!(pool.try_acquire(Class::Metadata).is_none());
    assert!(pool.try_acquire(Class::Download).is_none());
    assert!(pool.try_acquire(Class::Open).is_some());
    pool.set_paused(false);
    assert!(pool.try_acquire(Class::Download).is_some());
}

/// At most four large transfers at once, downloads and uploads together, each in a slot
/// of the pool; small ones keep taking the other slots, and a large one waiting for the
/// limit does not hold back the small ones behind it.
#[test]
fn at_most_four_large_transfers_run_while_small_ones_keep_going() {
    let pool = TransferPool::starting_at(16, 64);
    let mut large = take_sized(&pool, Class::Download, Size::Large, 2);
    large.extend(take_sized(&pool, Class::Upload, Size::Large, 2));
    assert_eq!(pool.large_held(), DEFAULT_LARGE);
    assert!(pool.try_acquire_sized(Class::Download, Size::Large).is_none(), "the fifth large one waits");
    assert!(pool.try_acquire_sized(Class::Upload, Size::Large).is_none());

    let mut fifth = queue_sized(&pool, Class::Download, Size::Large);
    let small = take(&pool, Class::Download, 6);
    assert_eq!(small.len(), 6, "small ones behind the waiting large one go");
    let mut small = small;
    small.extend(take(&pool, Class::Upload, 6));
    assert_eq!(pool.throughput().large, 4);
    assert_eq!(pool.throughput().large_limit, 4);

    drop(large.pop());
    let fifth = ready(&mut fifth);
    assert!(fifth.is_some(), "a large one done: the next large one goes");
    assert_eq!(pool.large_held(), DEFAULT_LARGE);
}

/// A file being opened is outside the large-stream limit and its count: an
/// open of a large file goes at once, takes no room in the limit, and is not among the
/// large streams; it still takes a slot of the pool.
#[test]
fn an_open_is_outside_the_large_stream_limit_and_its_count() {
    let pool = TransferPool::starting_at(16, 64);
    let _large = take_sized(&pool, Class::Download, Size::Large, DEFAULT_LARGE);
    let open = pool.try_acquire_sized(Class::Open, Size::Large);
    assert!(open.is_some(), "an open of a large file goes at once");
    assert_eq!(open.as_ref().unwrap().size(), Size::Small, "never marked large");
    assert_eq!(pool.large_held(), DEFAULT_LARGE);
    assert_eq!((pool.throughput().large, pool.throughput().in_use), (DEFAULT_LARGE as u32, DEFAULT_LARGE as u32 + 1));
    drop(_large);
    let _opens = take_sized(&pool, Class::Open, Size::Large, 2);
    assert_eq!((pool.large_held(), pool.throughput().in_use), (0, 3), "opens take slots, no room in the limit");
}

/// `in_use` is every slot held, of all four classes — a metadata one among them — and
/// counts an open's reserve above the pool's size.
#[test]
fn the_slots_in_use_are_every_class_and_the_reserve() {
    let pool = TransferPool::starting_at(4, 64);
    let _held =
        [take(&pool, Class::Metadata, 1), take(&pool, Class::Download, 1), take(&pool, Class::Upload, 1), take(&pool, Class::Open, 1)];
    assert_eq!((pool.throughput().in_use, pool.throughput().size), (4, 4));
    let _reserve = take(&pool, Class::Open, RESERVE);
    assert_eq!(pool.throughput().in_use, 4 + RESERVE as u32, "above the size: the opens' reserve");
}

/// `[transfers] large` sets the limit, kept within 1 and the ceiling.
#[test]
fn the_large_file_limit_follows_the_setting() {
    let pool = TransferPool::starting_at(16, 64);
    pool.set_limits(64, 1);
    let _one = take_sized(&pool, Class::Upload, Size::Large, 1);
    assert!(pool.try_acquire_sized(Class::Upload, Size::Large).is_none());
    pool.set_limits(8, 100);
    assert_eq!(pool.throughput().large_limit, 8, "never above the ceiling");
    assert_eq!(pool.size(), 8, "the pool shrinks to a lower ceiling");
}

/// A slot granted to a future dropped before it saw it goes back.
#[test]
fn a_granted_slot_of_a_dropped_waiter_goes_back() {
    let pool = TransferPool::starting_at(1, 64);
    let held = take(&pool, Class::Download, 1);
    let waiting = queue_sized(&pool, Class::Download, Size::Large);
    drop(held);
    assert_eq!(pool.held(Class::Download), 1, "granted to the waiter");
    assert_eq!(pool.large_held(), 1);
    drop(waiting);
    assert_eq!(pool.held(Class::Download), 0);
    assert_eq!(pool.large_held(), 0);
}

/// The speed is the average of the last three seconds.
#[tokio::test(start_paused = true)]
async fn the_speed_is_the_average_of_three_seconds() {
    let pool = TransferPool::starting_at(4, 64);
    pool.moved(Direction::Down, 3_000_000);
    pool.moved(Direction::Up, 300);
    let shown = pool.throughput();
    assert_eq!((shown.down_speed, shown.up_speed), (1_000_000, 100));
    tokio::time::sleep(SPEED_SPAN + BUCKET).await;
    assert_eq!(pool.throughput().down_speed, 0, "decays to 0");
}

/// The average a queue's time left is worked out from covers the run so far (up to the
/// last 30 s), outlasts the three-second speed, and is 0 once nothing has moved for 10 s;
/// what moved is counted for good.
#[tokio::test(start_paused = true)]
async fn the_average_covers_the_run_and_ends_after_ten_still_seconds() {
    let pool = TransferPool::starting_at(4, 64);
    for _ in 0..4 {
        pool.moved(Direction::Up, 1_000_000);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let shown = pool.throughput();
    assert_eq!((shown.up_average, shown.up_moved, shown.down_average), (1_000_000, 4_000_000, 0));

    tokio::time::sleep(STILL_AFTER - Duration::from_secs(2)).await;
    let shown = pool.throughput();
    assert_eq!(shown.up_speed, 0, "the three-second speed is gone");
    assert!(shown.up_average > 0, "the average is not yet");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let shown = pool.throughput();
    assert_eq!((shown.up_average, shown.up_moved), (0, 4_000_000));
}
