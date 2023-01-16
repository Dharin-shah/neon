//! Lots  of cleanup still to do, but this is a shared memory pipe between owner (pageserver) and
//! worker (postgres --walredo).

#![allow(dead_code)]
use std::alloc::Layout;
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::os::unix::io::AsRawFd;
use std::os::unix::prelude::FromRawFd;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};

use nix::sys::mman::{MapFlags, ProtFlags};

/// C-api as defined in the `shmempipe.h`
mod c_api;
pub mod shared;

const TO_WORKER_LEN: usize = 32 * 4096;
const FROM_WORKER_LEN: usize = 4 * 4096;

/// Whether or not to put the `request_response` function to sleep while waiting for the response
/// written by `write_all`.
///
/// Not using it should lead to busier waiting, and faster operation, but it only helps with more
/// than 1 thread cases.
const USE_EVENTFD_ON_RESPONSE: bool = true;

/// Input/output over a shared memory "pipe" which attempts to be faster than using standard input
/// and output with inter-process communication.
///
/// repr(C): this struct could be shared between recompilations.
/// Note: if benchmark starts to get bus errors, you most likely failed to recompile the
/// neon_walredo.so after the changes.
#[repr(C)]
pub struct RawSharedMemPipe {
    /// States:
    /// - 0x0000_0000 means initializing
    /// - 0xcafe_babe means ready
    /// - 0xffff_ffff means tearing down
    pub magic: AtomicU32,

    /// Eventfd used in semaphore mode, used to wakeup the request reader (walredoproc.c)
    pub notify_worker: i32,

    /// Eventfd used in semaphore mode, used to wakeup the response reader
    pub notify_owner: i32,

    /// The processes participating in this.
    ///
    /// First is the pageserver process, second is the single threaded walredo process. Values are
    /// practically Atomic<Option<u32>>, where zero means unoccupied/exited.
    pub participants: [AtomicU32; 2],

    /// When non-zero, the worker side OwnedRequester::recv cannot go to sleep.
    pub to_worker_waiters: AtomicU32,

    // rest wouldn't be too difficult to make a generic parameter, but let's hold off still.

    // Note: this is repr(c), so the order matters.
    pub to_worker: ringbuf::SharedRb<u8, [MaybeUninit<u8>; TO_WORKER_LEN]>,

    // TODO: response slots idea to cut down needed memcpys. instead of replying with the full
    // page, the page could be in one of the slots, and only the signal of "ready" would need to be
    // transferred over. the worker side could remap slots around to match postgres buffers.
    pub from_worker: ringbuf::SharedRb<u8, [MaybeUninit<u8>; FROM_WORKER_LEN]>,
}

impl SharedMemPipePtr<Created> {
    /// Wrap this in a new hopefully unique `Arc<OwnedRequester>`.
    pub fn try_acquire_requester(self) -> Option<std::sync::Arc<OwnedRequester>> {
        match self.participants[0].compare_exchange(0, std::process::id(), Relaxed, Relaxed) {
            Ok(_zero) => {}
            Err(_other) => {
                return None;
            }
        }

        Some(std::sync::Arc::new(OwnedRequester {
            producer: std::sync::Mutex::default(),
            consumer: std::sync::Mutex::default(),
            ptr: self,
            next: AtomicU32::new(0),
        }))
    }

    #[cfg(any(test, feature = "demo"))]
    pub unsafe fn as_joined(&self) -> SharedMemPipePtr<Joined> {
        // this is easier to debug with only one debugged process, however it needs to be dropped
        // before the actual ptr it's created from.
        SharedMemPipePtr {
            ptr: self.ptr,
            size: self.size,
            close_semaphores: false,
            tombstone_on_drop: false,
            munmap: false,
            _marker: std::marker::PhantomData,
        }
    }
}

impl SharedMemPipePtr<Joined> {
    pub fn try_acquire_responder(self) -> Option<OwnedResponder> {
        match self.participants[1].compare_exchange(0, std::process::id(), Relaxed, Relaxed) {
            Ok(_zero) => {}
            Err(_other) => return None,
        }

        Some(OwnedResponder {
            ptr: self,
            remaining: None,
        })
    }
}

pub struct OwnedRequester {
    producer: std::sync::Mutex<u32>,
    consumer: std::sync::Mutex<Wakeup>,
    /// id of the next thread to receive response. Waiting is managed through parking_lot.
    next: AtomicU32,
    ptr: SharedMemPipePtr<Created>,
}

#[derive(Default)]
struct Wakeup {
    // Move this behind a separate spinlock? or otherwise figure out a way for others proceed while
    // the response reception waits on semaphore.
    waiting: UnparkInOrder,
}

#[derive(Default, Debug)]
struct UnparkInOrder(std::collections::BinaryHeap<HeapEntry>);

impl Drop for UnparkInOrder {
    fn drop(&mut self) {
        println!("{} capacity", self.0.capacity());
    }
}

#[derive(Debug)]
struct HeapEntry(std::cmp::Reverse<u32>, std::thread::Thread);

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 && self.1.id() == other.1.id()
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl From<(u32, std::thread::Thread)> for HeapEntry {
    fn from(value: (u32, std::thread::Thread)) -> Self {
        HeapEntry(std::cmp::Reverse(value.0), value.1)
    }
}

impl UnparkInOrder {
    fn store_current(&mut self, id: u32) {
        self.0.push(HeapEntry::from((id, std::thread::current())));
    }

    fn current_is_front(&self, expected_id: u32) -> bool {
        let ret = match self.0.peek() {
            Some(HeapEntry(id, first)) => {
                let cur = std::thread::current();
                id.0 == expected_id && cur.id() == first.id()
            }
            None => false,
        };
        ret
    }

    fn pop_front(&mut self, expected_id: u32) {
        use std::collections::binary_heap::PeekMut;

        let cur = std::thread::current();
        let next = self.0.peek_mut();
        let next = next.expect("should not be empty because we were just unparked");
        let t = &next.1;
        assert_eq!(cur.id(), t.id());
        assert_eq!(next.0 .0, expected_id);

        PeekMut::<'_, HeapEntry>::pop(next);
    }

    fn unpark_front(&self, turn: u32) {
        match self.0.peek() {
            Some(HeapEntry(id, t)) if id.0 == turn => {
                t.unpark();
            }
            Some(_) | None => {
                // Not an error, the thread we are hoping to wakeup just hasn't yet arrived to the
                // parking lot.
            }
        }
    }

    pub(crate) fn park_while<'a, T, F>(
        mut guard: std::sync::MutexGuard<'a, T>,
        consumer: &'a std::sync::Mutex<T>,
        mut cond: F,
    ) -> std::sync::MutexGuard<'a, T>
    where
        F: FnMut(&mut T) -> bool,
    {
        while cond(&mut *guard) {
            drop(guard);
            std::thread::park();
            guard = consumer.lock().unwrap();
        }
        guard
    }
}

impl OwnedRequester {
    /// Returns the file descriptors that need to be kept open for child process.
    pub fn shared_fds(&self) -> [i32; 2] {
        [
            // FIXME: one should be enough for waiting for the worker, or the worker waiting for
            // new input -- nope, it's not, because there's an affinity to read it yourself when
            // immediatedly reading it after posting.
            self.ptr.notify_worker,
            self.ptr.notify_owner,
        ]
    }

    #[inline(never)]
    pub fn request_response(&self, req: &[u8], resp: &mut [u8]) -> u32 {
        // Overview:
        // - `self.producer` creates an order amongst competing request_response callers (id).
        // - the same token (id) is used to find some order with `self.consumer` to read the
        // response

        let id = self.send_request(req);

        let mut next = self.next.load(Acquire);

        if next != id {
            let mut g = self.consumer.lock().unwrap();

            // recheck in case it's our turn now after locking the mutex
            next = self.next.load(Acquire);
            if next != id {
                g.waiting.store_current(id);

                g = UnparkInOrder::park_while(g, &self.consumer, |_| {
                    next = self.next.load(Acquire);
                    next != id
                });

                assert!(g.waiting.current_is_front(id));
                g.waiting.pop_front(id);
            }
            drop(g);
        }

        assert_eq!(next, id);

        self.recv_response(id, resp);

        let prev = self.next.fetch_add(1, Release);
        assert_eq!(id, prev);

        let g = self.consumer.lock().unwrap();
        g.waiting.unpark_front(prev.wrapping_add(1));
        id
    }

    fn send_request(&self, req: &[u8]) -> u32 {
        let sem = unsafe { shared::EventfdSemaphore::from_raw_fd(self.ptr.notify_worker) };

        // this will be contended if there's anyone else interested in writing
        let mut g = self.producer.lock().unwrap();

        // this will be decremented by `write_all` on each response
        let mut might_wait = self.ptr.to_worker_waiters.fetch_add(1, Release) == 0;

        let id = *g;
        *g = g.wrapping_add(1);

        // Safety: we are only one creating producers for to_worker
        let mut p = unsafe { ringbuf::Producer::new(&self.ptr.to_worker) };

        let mut spin = SpinWait::default();

        let mut send = |mut req| loop {
            let n = p.push_slice(req);
            req = &req[n..];

            if req.is_empty() {
                break;
            } else if n == 0 {
                if might_wait {
                    sem.post();
                    might_wait = false;
                }
            } else if n != 0 {
                spin.reset();
            }

            spin.spin();
        };

        let len = req.len();

        // framing doesn't require us to manage state, as we always send both
        let frame_len = u64::try_from(len)
            .expect("message cannot be more than 4GB")
            .to_ne_bytes();

        // using the postponed version here between the two pushes most definitely leads to
        // corruption, also it can trigger a debug_assert! within ringbuf
        send(&frame_len);
        send(req);

        drop(g);

        // as part of the first write, make sure that the worker is woken up.
        // FIXME: remove if the first one seems to work better
        if might_wait {
            sem.post();
        }

        id
    }

    fn recv_response<'a>(&self, _id: u32, resp: &mut [u8]) {
        // Safety: we are the only one creating consumers for from_worker because we've awaited our
        // turn
        let mut c = unsafe { ringbuf::Consumer::new(&self.ptr.from_worker) };

        let sem = unsafe { shared::EventfdSemaphore::from_raw_fd(self.ptr.notify_owner) };

        if USE_EVENTFD_ON_RESPONSE {
            sem.wait();
        }

        let mut read = 0;
        let mut div = 0;
        let mut spin = SpinWait::default();

        loop {
            let n = c.pop_slice(&mut resp[read..]);

            read += n;
            div += 1;

            if div == 100_000 {
                // when using the eventfd this printout should never happen.
                println!("owner: 100k attempts and read {read} bytes, last {n}");
            }

            if read == resp.len() {
                break;
            }

            if n != 0 {
                spin.reset();
            }

            spin.spin();
        }
    }
}

/// This type is movable.
#[repr(C)]
pub struct OwnedResponder {
    /// How long currently received message is, and how much is remaining.
    remaining: Option<(u32, u32)>,
    ptr: SharedMemPipePtr<Joined>,
}

impl OwnedResponder {
    pub fn read_next_frame_len(&mut self) -> Result<u32, u32> {
        match self.remaining.as_mut() {
            Some((_, remaining)) => Err(*remaining),
            None => {
                // well, reading to empty does seem wrong
                assert_eq!(self.read(&mut [][..]), 0);
                let (len, remaining) = self.remaining.as_ref().unwrap();
                assert_eq!(len, remaining);
                return Ok(*remaining);
            }
        }
    }

    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        if self.remaining.is_none() {
            // read the new frame length, as u64 where the extra 4 bytes are used as verification
            // against corruption, which happens with the postponed writer in send_request.
            let mut raw = [0u8; 8];
            assert_eq!(self.recv(&mut raw, 7, true), 8);

            assert_eq!(&raw[4..], &[0, 0, 0, 0], "read: {raw:?}");

            let len = u64::from_ne_bytes(raw);
            let len = u32::try_from(len).unwrap();

            // store it as frame size, remaining size
            self.remaining = Some((len, len));
        }

        if buf.is_empty() {
            return 0;
        }

        let (_, mut remaining) = self.remaining.unwrap();

        // recv only up to the next frame length
        let allowed = buf.len();
        let buf = &mut buf[..std::cmp::min(allowed, remaining as usize)];

        let read = self.recv(buf, 0, false);

        remaining = remaining
            .checked_sub(
                u32::try_from(read)
                    .expect("should had read at most remaining, not overflowing u32"),
            )
            .expect("should not have read more than remaining");

        if remaining == 0 {
            self.remaining = None;
        } else {
            let (_, rem) = self.remaining.as_mut().unwrap();
            *rem = remaining;
        }

        read
    }

    // TODO: call this read_frame or something other
    pub fn read_exact(&mut self, buf: &mut [u8]) -> usize {
        // TODO: panics should not be leaked to ffi, it is UB right now but might become abort in
        // future. it is easy to take all common pointer handling out and make that wrapper also
        // catch_unwind, then abort.
        let remaining = match self.remaining.as_ref() {
            Some((_, remaining)) => *remaining,
            None => unreachable!("cannot panic here but the frame length should be known"),
        };

        assert!(remaining as usize <= buf.len());

        let read = self.recv(
            &mut buf[..remaining as usize],
            remaining as usize - 1,
            false,
        );

        assert_eq!(read, remaining as usize);
        self.remaining = None;
        read
    }

    fn recv(&mut self, buf: &mut [u8], read_more_than: usize, can_wait: bool) -> usize {
        let mut c = unsafe { ringbuf::Consumer::new(&self.ptr.to_worker) };
        let sem = unsafe { shared::EventfdSemaphore::from_raw_fd(self.ptr.notify_worker) };

        let mut read = 0;
        let mut waited = false;
        let mut div = 0;
        let mut spin = SpinWait::default();

        loop {
            let n = c.pop_slice(&mut buf[read..]);

            read += n;
            div += 1;

            if div == 100_000 {
                println!("worker: after 100k attempts, have read {read} bytes");
            }

            if read > read_more_than {
                // interestingly this sleeps the most, probably while the response is being read.
                return read;
            } else if !waited && can_wait {
                // go to sleep, which is few microseconds costlier
                while self.ptr.to_worker_waiters.load(Acquire) == 0 {
                    sem.wait();
                    waited = true;
                }
            } else if n != 0 {
                spin.reset();
            }
            spin.spin();
        }
    }

    pub fn write_all(&mut self, mut buf: &[u8]) -> usize {
        let mut p = unsafe { ringbuf::Producer::new(&self.ptr.from_worker) };

        let sem = unsafe { shared::EventfdSemaphore::from_raw_fd(self.ptr.notify_owner) };

        let len = buf.len();

        let mut spin = SpinWait::default();

        loop {
            let n = p.push_slice(buf);
            buf = &buf[n..];

            if buf.is_empty() {
                if USE_EVENTFD_ON_RESPONSE {
                    sem.post();
                }

                // allow waiting on recv
                self.ptr.to_worker_waiters.fetch_sub(1, Release);
                return len;
            }

            if n != 0 {
                spin.reset();
            }
            spin.spin();
        }
    }
}

/// Spin or yield.
///
/// Adapted from parking_lot's adaptive spinning.
#[derive(Default)]
struct SpinWait(u32);

impl SpinWait {
    fn spin(&mut self) {
        self.0 += 1;
        // this is parking_lot's adaptive spinning
        if self.0 < 10 {
            for _ in 0..(1 << self.0) {
                std::hint::spin_loop();
            }
        } else {
            self.0 = 10;
            std::thread::yield_now();
        }
    }

    fn reset(&mut self) {
        self.0 = 0;
    }
}

pub fn create(path: &Path) -> std::io::Result<SharedMemPipePtr<Created>> {
    use nix::fcntl::OFlag;
    use nix::sys::eventfd::{eventfd, EfdFlags};
    use nix::sys::mman;
    use nix::sys::stat::Mode;

    assert!(path.is_absolute());
    assert!(path.as_os_str().len() < 255);

    // synchronization between the creator and the joiner/worker
    // FIXME: OwnedFd
    let notify_worker = unsafe { std::fs::File::from_raw_fd(eventfd(0, EfdFlags::EFD_SEMAPHORE)?) };
    let notify_owner = unsafe { std::fs::File::from_raw_fd(eventfd(0, EfdFlags::EFD_SEMAPHORE)?) };

    // O_CLOEXEC, the other process does not need to inherit this, it opens it by name
    let flags = OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_TRUNC | OFlag::O_CLOEXEC;
    let mode = Mode::S_IRUSR | Mode::S_IWUSR;

    // use it as a file for get automatic closing
    // FIXME: should use OwnedFd but unstable
    let handle = unsafe { std::fs::File::from_raw_fd(mman::shm_open(path, flags, mode)?) };

    let size = Layout::new::<RawSharedMemPipe>()
        .align_to(4096)
        .expect("alignment is power of two")
        .size();

    assert!(size > 0);

    handle.set_len(size as u64)?;

    let size = NonZeroUsize::new(size).unwrap();

    let ptr = unsafe {
        // Safety: ffi(?)
        mman::mmap(
            None,
            size,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_SHARED,
            handle.as_raw_fd(),
            0,
        )
    }?;

    let ptr = NonNull::new(ptr).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "mmap returned null pointer")
    })?;

    // use this on stack for panics until init is complete, then Arc it?
    let res = SharedMemPipePtr::post_mmap(ptr.cast::<RawSharedMemPipe>(), size);

    // file is no longer needed -- or is it? should it be saved and cleared? we might be leaking
    // fd's, unless the mmap's hold an "fd" to the shared
    drop(handle);

    initialize_at(res, notify_worker, notify_owner)
}

/// Initialize the RawSharedMemPipe *in place*.
///
/// In place initialization is trickier than normal rust programs. This would be much simpler if we
/// would have stable allocator trait, and many currently unstable MaybeUninit friendly
/// conversions.
fn initialize_at(
    res: SharedMemPipePtr<MMapped>,
    notify_worker: std::fs::File,
    notify_owner: std::fs::File,
) -> std::io::Result<SharedMemPipePtr<Created>> {
    let inner = res.ptr();
    // Safety: lot of requirements, TODO
    let place = unsafe { inner.cast::<MaybeUninit<RawSharedMemPipe>>().as_mut() };

    trait AsPointerToUninit {
        type Target;
        fn cast_uninit(self) -> Self::Target;
    }

    trait AsPointerToUninitArray {
        type Target;
        fn cast_uninit_array(self) -> Self::Target;
    }

    impl<T> AsPointerToUninit for *mut T {
        type Target = *mut MaybeUninit<T>;

        // this is just a convinience to type less, also, any cast is valid, so this is easy to
        // mistype
        fn cast_uninit(self) -> Self::Target {
            self.cast::<MaybeUninit<T>>()
        }
    }

    impl<T, const N: usize> AsPointerToUninitArray for *mut [T; N] {
        type Target = *mut [MaybeUninit<T>; N];

        fn cast_uninit_array(self) -> Self::Target {
            self.cast::<[MaybeUninit<T>; N]>()
        }
    }

    macro_rules! uninit_field {
        ($field:ident) => {{
            unsafe {
                std::ptr::addr_of_mut!((*place.as_mut_ptr()).$field)
                    .cast_uninit()
                    .as_mut()
                    .expect("valid non-null ptr")
            }
        }};
    }

    {
        let magic = uninit_field!(magic);
        magic.write(AtomicU32::new(0x0000_0000));

        // ceremonial
        unsafe { magic.assume_init_mut() };
    }

    {
        let fd = uninit_field!(notify_worker);
        fd.write(notify_worker.as_raw_fd());
        unsafe { fd.assume_init_mut() };
        // the file is forgotten if the init completes
    }

    {
        let fd = uninit_field!(notify_owner);
        fd.write(notify_owner.as_raw_fd());
        unsafe { fd.assume_init_mut() };
        // the file is forgotten if the init completes
    }

    {
        let participants = unsafe {
            std::ptr::addr_of_mut!((*place.as_mut_ptr()).participants)
                .cast_uninit_array()
                .cast_uninit()
                .as_mut()
                .expect("valid non-null pointer")
        };

        // Safety: array_assume_init is unstable
        let participants = unsafe { participants.assume_init_mut() };

        for slot in participants.iter_mut() {
            // Panic safety: not needed, AtomicU32 don't panic.
            slot.write(AtomicU32::new(0));
            unsafe { slot.assume_init_mut() };
        }
    }

    {
        let to_worker_waiters = uninit_field!(to_worker_waiters);
        to_worker_waiters.write(AtomicU32::default());
        unsafe { to_worker_waiters.assume_init_mut() };
    }

    {
        let to_worker = uninit_field!(to_worker);
        to_worker.write(ringbuf::StaticRb::default());
        unsafe { to_worker.assume_init_mut() };
    }

    {
        let from_worker = uninit_field!(from_worker);
        from_worker.write(ringbuf::StaticRb::default());
        unsafe { from_worker.assume_init_mut() };
    }

    // FIXME: above, we need to do manual drop handling

    // Safety: it is now initialized
    let _ = unsafe { place.assume_init_mut() };
    std::mem::forget(notify_worker);
    std::mem::forget(notify_owner);
    drop(place);

    let res = res.post_init_created();

    res.magic
        .store(0xcafebabe, std::sync::atomic::Ordering::SeqCst);

    Ok(res)
}

/// Type state for the cleanup on drop pointer.
///
/// Without any test specific configuration, will call `munmap` afterwards.
pub struct MMapped;

/// Type state to fully cleanup on drop pointer, created with [`create`].
pub struct Created;

/// Type state to fully cleanup on drop pointer, created with [`open_existing`].
pub struct Joined;

/// Owning pointer to the mmap'd shared memory section.
///
/// This has a phantom type parameter, which differentiates the pointed memory in different states,
/// and doesn't allow for example the `join_initialized_at` to call `try_acquire_responder`.
pub struct SharedMemPipePtr<Stage> {
    ptr: Option<NonNull<RawSharedMemPipe>>,
    size: NonZeroUsize,

    /// In normal operation, the semaphores are eventfd's which get duplicated when launching a new
    /// child process.
    ///
    /// In testing it is useful to leave them be, for example to close them manually.
    close_semaphores: bool,

    /// Only the owner side should tombstone.
    tombstone_on_drop: bool,

    /// Normally owner and worker both unmap, however it's unlikely that worker will ever close
    /// before getting killed.
    munmap: bool,

    _marker: std::marker::PhantomData<Stage>,
}

unsafe impl Send for SharedMemPipePtr<Created> {}
// nothing bad with this send impl, but it just hasn't been needed.
#[cfg(any(test, feature = "demo"))]
unsafe impl Send for SharedMemPipePtr<Joined> {}
unsafe impl Sync for SharedMemPipePtr<Created> {}

impl SharedMemPipePtr<MMapped> {
    fn post_mmap(ptr: NonNull<RawSharedMemPipe>, size: NonZeroUsize) -> Self {
        SharedMemPipePtr {
            ptr: Some(ptr),
            size,
            // the files are on-stack, so the values might not be initialized
            close_semaphores: false,
            tombstone_on_drop: true,
            munmap: true,
            _marker: std::marker::PhantomData,
        }
    }

    #[cfg(test)]
    fn with_munmap_on_drop(mut self, munmap: bool) -> Self {
        self.munmap = munmap;
        self
    }

    fn ptr(&self) -> NonNull<RawSharedMemPipe> {
        self.ptr.as_ref().unwrap().clone()
    }

    fn post_init_created(mut self) -> SharedMemPipePtr<Created> {
        let ptr = self.ptr.take();
        let size = self.size;
        let ret = SharedMemPipePtr {
            ptr,
            size,
            close_semaphores: true,
            tombstone_on_drop: true,
            munmap: self.munmap,
            _marker: std::marker::PhantomData,
        };
        std::mem::forget(self);
        ret
    }

    fn post_init_joined(mut self) -> SharedMemPipePtr<Joined> {
        let ptr = self.ptr.take();
        let size = self.size;
        let ret = SharedMemPipePtr {
            ptr,
            size,
            close_semaphores: true,
            tombstone_on_drop: false,
            munmap: self.munmap,
            _marker: std::marker::PhantomData,
        };
        std::mem::forget(self);
        ret
    }
}

impl<Stage> Drop for SharedMemPipePtr<Stage> {
    fn drop(&mut self) {
        let _res = {
            if let Some(ptr) = self.ptr.take() {
                if self.close_semaphores {
                    let shared = unsafe { ptr.as_ref() };

                    for fd in [shared.notify_worker, shared.notify_owner] {
                        unsafe { std::fs::File::from_raw_fd(fd) };
                    }
                }

                if self.tombstone_on_drop {
                    let shared = unsafe { ptr.as_ref() };

                    // FIXME: make sure only the owner does this
                    shared.magic.store(0xffff_ffff, SeqCst);

                    // TODO: as we no longer have anything which would require drop, perhaps this
                    // could just be left out completly?
                    unsafe { std::ptr::drop_in_place(ptr.as_ptr()) };
                }

                let do_unmap = self.munmap;

                if do_unmap {
                    // both should do this, while the postgres side is very unlikely to do
                    // this, because it's killed before it's time to munmap.
                    unsafe { nix::sys::mman::munmap(ptr.as_ptr().cast(), self.size.get()) }
                } else {
                    Ok(())
                }
            } else {
                // FIXME: unsure how should this happen?
                Ok(())
            }
        };
        #[cfg(debug_assertions)]
        _res.expect("closing SharedMemPipePtr failed");
    }
}

#[cfg(test)]
impl<Stage> SharedMemPipePtr<Stage> {
    fn with_close_semaphores_on_drop(mut self, close: bool) -> Self {
        self.close_semaphores = close;
        self
    }

    fn with_tombstone_on_drop(mut self, tombstone: bool) -> Self {
        self.tombstone_on_drop = tombstone;
        self
    }
}

impl std::ops::Deref for SharedMemPipePtr<Created> {
    type Target = RawSharedMemPipe;

    fn deref(&self) -> &Self::Target {
        unsafe { self.ptr.as_ref().unwrap().as_ref() }
    }
}

impl std::ops::Deref for SharedMemPipePtr<Joined> {
    type Target = RawSharedMemPipe;

    fn deref(&self) -> &Self::Target {
        unsafe { self.ptr.as_ref().unwrap().as_ref() }
    }
}

pub fn open_existing<P: nix::NixPath + ?Sized>(
    path: &P,
) -> std::io::Result<SharedMemPipePtr<Joined>> {
    use nix::fcntl::OFlag;
    use nix::sys::mman;
    use nix::sys::stat::Mode;

    let flags = OFlag::O_RDWR;
    let mode = Mode::S_IRUSR | Mode::S_IWUSR;

    // use it as a file for get automatic closing
    // FIXME: should use OwnedFd but unstable
    // Safety: ffi?
    let handle = unsafe { std::fs::File::from_raw_fd(mman::shm_open(path, flags, mode)?) };

    let size = Layout::new::<RawSharedMemPipe>()
        .align_to(4096)
        .expect("alignment is power of two")
        .size();

    assert!(size > 0);

    handle.set_len(size as u64)?;

    let size = NonZeroUsize::new(size).unwrap();

    let ptr = unsafe {
        // Safety: ffi(?)
        mman::mmap(
            None,
            size,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_SHARED,
            handle.as_raw_fd(),
            0,
        )
    }?;

    let ptr = NonNull::new(ptr).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "mmap returned null pointer")
    })?;

    let ptr = ptr.cast::<RawSharedMemPipe>();

    // use this on stack for panics until init is complete, then Arc it?
    let res = SharedMemPipePtr::post_mmap(ptr, size);

    join_initialized_at(res)
}

fn join_initialized_at(
    res: SharedMemPipePtr<MMapped>,
) -> std::io::Result<SharedMemPipePtr<Joined>> {
    let inner = res.ptr();
    let place = unsafe { inner.cast::<MaybeUninit<RawSharedMemPipe>>().as_mut() };

    // NOTE: here cannot be any mutex initialization
    {
        let magic = unsafe {
            std::ptr::addr_of_mut!((*place.as_mut_ptr()).magic)
                .cast::<MaybeUninit<AtomicU32>>()
                .as_mut()
                .expect("valid non-null pointer")
        };

        // Safety: creator has already initialized, hopefully
        let magic = unsafe { magic.assume_init_ref() };

        let mut ready = false;

        for _ in 0..1000 {
            // FIXME: acqrel would be better?
            let read = magic.load(SeqCst);

            match read {
                0x0000_0000 => {
                    // we are early, it's being initialized
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                0xcafe_babe => {
                    // it's ready!
                    ready = true;
                    break;
                }
                other => {
                    // it probably is not healthy
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("shared memory area has unknown magic: 0x{other:08x}"),
                    ));
                }
            }
        }

        if !ready {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("shared memory area did not complete initialization before timeout"),
            ));
        }
    }

    // It is now initialized, but it happened on a different process
    unsafe { place.assume_init_mut() };

    Ok(res.post_init_joined())
}

#[cfg(all(test))]
mod tests {
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::Ordering::SeqCst;
    use std::{mem::MaybeUninit, num::NonZeroUsize, ptr::NonNull};

    use rand::Rng;

    use crate::SharedMemPipePtr;

    use super::RawSharedMemPipe;

    /// This is a test for miri to detect any UB, or valgrind memcheck.
    // #[cfg(miri)]
    #[test]
    fn initialize_at_on_boxed() {
        // use of seqcst is confusing here, it is not needed for anything
        let ordering = SeqCst;

        let mem = Box::new(MaybeUninit::<RawSharedMemPipe>::uninit());
        let ptr = Box::into_raw(mem);

        let _guard = DropRawBoxOnDrop(ptr);

        let ptr = NonNull::new(ptr).unwrap();
        let size = std::mem::size_of::<RawSharedMemPipe>();
        let size = NonZeroUsize::new(size).unwrap();

        // miri does not yet support the tempfile crate, which always uses a secure mode
        let mut tempfiles = std::iter::from_fn(|| {
            let dir = std::env::temp_dir();

            let mut rng = rand::thread_rng();

            const ATTEMPTS: usize = 50;

            for attempt in 0..ATTEMPTS {
                let last_attempt = attempt == ATTEMPTS - 1;

                let filename = (&mut rng)
                    .sample_iter(&rand::distributions::Alphanumeric)
                    .take(8)
                    .map(|b| b as char)
                    .collect::<String>();
                let path = dir.join(filename);
                match std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                {
                    Ok(f) => {
                        std::fs::remove_file(&path)
                            .expect("should had managed to remove just created tempfile");
                        return Some(Ok(f));
                    }
                    Err(e) if !last_attempt && e.kind() == std::io::ErrorKind::AlreadyExists => {
                        continue
                    }
                    Err(other) => return Some(Err(other)),
                }
            }

            unreachable!()
        });

        let file_a = tempfiles
            .next()
            .expect("must be able to create two tempfiles")
            .unwrap();
        let file_b = tempfiles
            .next()
            .expect("must be able to create two tempfiles")
            .unwrap();

        let expected_fds = (file_a.as_raw_fd(), file_b.as_raw_fd());

        // TODO: maybe add Stage::Target = { MaybeUninit<_>, _ }? it is what the types basically
        // do.
        let ready = {
            let ptr = SharedMemPipePtr::post_mmap(ptr.cast(), size).with_munmap_on_drop(false);

            super::initialize_at(ptr, file_a, file_b).unwrap()
        };

        {
            assert_eq!(0xcafe_babe, ready.magic.load(ordering));
            // field order vs. arg order are not really important, as long as both use them for the
            // same outcome
            assert_eq!(expected_fds.0, ready.notify_worker);
            assert_eq!(expected_fds.1, ready.notify_owner);
        }

        // first allowing for initialization then allowing joining already initialized shouldn't
        // cause any more problems, but we might suffer the wait. TODO: make it configurable.

        let joined = {
            let ptr = SharedMemPipePtr::post_mmap(ptr.cast(), size).with_munmap_on_drop(false);
            super::join_initialized_at(ptr).unwrap()
        };

        {
            assert_eq!(0xcafe_babe, joined.magic.load(ordering));
        }

        drop(joined);

        {
            assert_eq!(0xcafe_babe, ready.magic.load(ordering));
        }

        drop(ready);

        // the memory is still valid, it hasn't been dropped, the _guard will drop it
        {
            let target = ptr.cast::<RawSharedMemPipe>();
            let target = unsafe { target.as_ref() };
            let magic = target.magic.load(ordering);
            assert_eq!(0xffff_ffff, magic, "0x{magic:08x}");
        }
    }

    struct DropRawBoxOnDrop<T>(*mut T);

    impl<T> Drop for DropRawBoxOnDrop<T> {
        fn drop(&mut self) {
            // Safety: we never deallocate (might munmap) in tests
            unsafe { Box::from_raw(self.0) };
        }
    }

    use crate::UnparkInOrder;

    #[test]
    fn unparks_in_order() {
        let mut uio = UnparkInOrder::default();
        uio.store_current(0);
        assert!(uio.current_is_front(0));
        uio.pop_front(0);
        uio.unpark_front(1); // there is no front() right now

        uio.store_current(2);
        uio.store_current(1);
        assert!(uio.current_is_front(1));
        uio.pop_front(1);
        uio.unpark_front(2); // unparking 2 => ThreadId(11)
        uio.store_current(3);
    }
}
