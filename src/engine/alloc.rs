//! Getting test memory from the operating system and giving it back.
//!
//! Memory comes in [`Chunk`]s of at most [`CHUNK_BYTES`], so that running out
//! part way still leaves the chunks already obtained to test. On Windows each
//! chunk is locked into physical RAM so it cannot be paged out while under
//! test; see [`Arena::allocate`].

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Size of one allocation in normal runs. Small enough that allocating and
/// locking one chunk takes well under the time a stop request may wait, large
/// enough that tens of GiB need only a few hundred chunks.
pub(crate) const CHUNK_BYTES: usize = 128 << 20;

/// Allocation sizes are rounded down to whole pages.
pub(crate) const PAGE_BYTES: usize = 4096;

/// How much to test on non-Windows systems (development only) when no limit
/// is given.
#[cfg(not(windows))]
const DEV_DEFAULT_BYTES: u64 = 256 << 20;

#[cfg(any(windows, test))]
const GIB: u64 = 1 << 30;

/// What the operating system reports about memory, in bytes.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MemoryStatus {
    /// Physical memory installed.
    pub total_phys: u64,
    /// Physical memory free right now.
    pub avail_phys: u64,
    /// The system commit limit: physical memory plus page files. Every
    /// committed byte, ours or another program's, counts against it.
    pub commit_limit: u64,
    /// Commit still available before the limit is reached.
    pub avail_commit: u64,
}

/// How many bytes to aim for.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Targets {
    /// Bytes to try to lock into RAM.
    pub locked: u64,
    /// Bytes to use instead if nothing at all can be locked.
    pub unlocked: u64,
}

/// Headroom left free out of a resource of size `total`: 1 GiB or 10% of it,
/// whichever is larger.
#[cfg(any(windows, test))]
fn headroom(total: u64) -> u64 {
    (total / 10).max(GIB)
}

/// How much to try to test, given the system's memory `status`.
///
/// The locked target is the smaller of available physical memory minus its
/// headroom and available commit minus its headroom (each headroom being
/// 1 GiB or 10% of the matching total, whichever is larger), then capped at
/// `max_bytes`. The physical headroom keeps the rest of the system responsive
/// and stops it paging heavily. The commit headroom keeps other programs able
/// to allocate memory: on machines with a small or disabled page file,
/// committing everything that is physically free would make every other
/// allocation on the system fail.
///
/// The unlocked target is the locked one further capped at half of the
/// available physical memory, because unlocked memory competes with the rest
/// of the system for RAM.
#[cfg(any(windows, test))]
pub(crate) fn targets(status: &MemoryStatus, max_bytes: Option<u64>) -> Targets {
    let physical = status
        .avail_phys
        .saturating_sub(headroom(status.total_phys));
    let commit = status
        .avail_commit
        .saturating_sub(headroom(status.commit_limit));
    let locked = physical.min(commit);
    let locked = max_bytes.map_or(locked, |cap| locked.min(cap));
    Targets {
        locked,
        unlocked: locked.min(status.avail_phys / 2),
    }
}

/// Size of the next chunk to allocate when `remaining` bytes are still
/// wanted: at most `chunk_bytes`, rounded down to whole pages, 0 when less
/// than a page is wanted.
fn next_chunk_len(remaining: u64, chunk_bytes: usize) -> usize {
    // The minimum is at most `chunk_bytes`, so it fits in a usize.
    let len = usize::try_from(remaining.min(chunk_bytes as u64)).unwrap_or(chunk_bytes);
    len - len % PAGE_BYTES
}

/// Total physical memory installed, as far as the operating system reports
/// it, or `None` where that is not known (always `None` off Windows).
pub fn total_physical_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        win::memory_status().map(|s| s.total_phys)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// One block of committed, read/write memory, freed when dropped.
#[derive(Debug)]
pub(crate) struct Chunk {
    ptr: NonNull<u64>,
    bytes: usize,
    locked: bool,
}

impl Chunk {
    /// Allocates `bytes` (a non-zero multiple of [`PAGE_BYTES`]) of memory,
    /// page aligned. Returns `None` when the system refuses.
    fn alloc(bytes: usize) -> Option<Chunk> {
        if bytes == 0 || !bytes.is_multiple_of(PAGE_BYTES) {
            return None;
        }
        #[cfg(windows)]
        let ptr = win::alloc(bytes);
        #[cfg(not(windows))]
        let ptr = {
            let layout = std::alloc::Layout::from_size_align(bytes, PAGE_BYTES).ok()?;
            // SAFETY: `layout` has a non-zero size (checked above).
            unsafe { std::alloc::alloc_zeroed(layout) }
        };
        let ptr = NonNull::new(ptr.cast::<u64>())?;
        #[cfg(test)]
        LIVE_BYTES.with(|b| b.set(b.get() + bytes as u64));
        Some(Chunk {
            ptr,
            bytes,
            locked: false,
        })
    }

    /// First word of the chunk.
    pub(crate) fn ptr(&self) -> *mut u64 {
        self.ptr.as_ptr()
    }

    /// Size of the chunk in bytes (a multiple of [`PAGE_BYTES`]).
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: `ptr`/`bytes` describe a region returned by `win::alloc`,
        // owned only by this chunk and released exactly once, here. Nothing
        // uses it afterwards: blocks pointing into it are dropped before the
        // arena that owns the chunks.
        unsafe {
            win::free(self.ptr.as_ptr().cast(), self.bytes, self.locked)
        };
        #[cfg(not(windows))]
        {
            // `alloc` built this layout successfully, so it is valid.
            if let Ok(layout) = std::alloc::Layout::from_size_align(self.bytes, PAGE_BYTES) {
                // SAFETY: the pointer came from `alloc_zeroed` with this same
                // layout and is freed exactly once, here.
                unsafe { std::alloc::dealloc(self.ptr.as_ptr().cast(), layout) }
            }
        }
        #[cfg(test)]
        LIVE_BYTES.with(|b| b.set(b.get() - self.bytes as u64));
    }
}

/// Something that can pin a chunk into physical RAM.
trait Pinner {
    /// Pins `chunk`, returning `false` if that is not possible.
    fn pin(&mut self, chunk: &Chunk) -> bool;
}

/// Settings and shared counters for one allocation.
#[derive(Debug)]
pub(crate) struct AllocContext<'a> {
    /// Largest chunk to allocate ([`CHUNK_BYTES`] in normal runs).
    pub chunk_bytes: usize,
    /// Allocation ends early, keeping what it has, when this is set.
    pub stop: &'a AtomicBool,
    /// Bytes currently being aimed for, for progress reporting.
    pub target: &'a AtomicU64,
    /// Bytes obtained (and kept) so far, for progress reporting. Only grows.
    pub obtained: &'a AtomicU64,
    /// Test only: the system refuses every allocation after this many, as
    /// when the commit limit is reached part way.
    #[cfg(test)]
    pub refuse_after: Option<usize>,
}

/// Allocates chunks up to a target, stopping early if `ctx.stop` is set.
///
/// With a `pinner`, chunks are allocated and pinned one at a time up to
/// `locked_target` bytes; the first chunk that cannot be allocated or pinned
/// is freed and growth stops there, keeping the chunks already pinned. If not
/// a single chunk could be pinned, it falls back to unpinned chunks up to
/// `unlocked_target` bytes, which is also what happens without a pinner.
/// Unpinned growth likewise stops at the first allocation the system refuses.
fn grow(
    ctx: &AllocContext<'_>,
    locked_target: u64,
    pinner: Option<&mut dyn Pinner>,
    unlocked_target: u64,
) -> Vec<Chunk> {
    #[cfg(test)]
    let granted = std::cell::Cell::new(0usize);
    let alloc = |len: usize| {
        #[cfg(test)]
        {
            if ctx.refuse_after.is_some_and(|n| granted.get() >= n) {
                return None;
            }
            granted.set(granted.get() + 1);
        }
        Chunk::alloc(len)
    };
    let stopping = || ctx.stop.load(Ordering::Relaxed);

    let mut chunks = Vec::new();
    if let Some(pinner) = pinner {
        ctx.target.store(locked_target, Ordering::Relaxed);
        let mut total = 0u64;
        while !stopping() {
            let len = next_chunk_len(locked_target - total, ctx.chunk_bytes);
            if len == 0 {
                break;
            }
            let Some(mut chunk) = alloc(len) else {
                break;
            };
            if !pinner.pin(&chunk) {
                // `chunk` is dropped (freed) here.
                break;
            }
            chunk.locked = true;
            total += len as u64;
            chunks.push(chunk);
            ctx.obtained.fetch_add(len as u64, Ordering::Relaxed);
        }
        if !chunks.is_empty() || stopping() {
            return chunks;
        }
    }
    // Nothing has been obtained yet, so switching the target cannot make the
    // reported fraction go backwards.
    ctx.target.store(unlocked_target, Ordering::Relaxed);
    let mut total = 0u64;
    while !stopping() {
        let len = next_chunk_len(unlocked_target - total, ctx.chunk_bytes);
        if len == 0 {
            break;
        }
        let Some(chunk) = alloc(len) else {
            break;
        };
        total += len as u64;
        chunks.push(chunk);
        ctx.obtained.fetch_add(len as u64, Ordering::Relaxed);
    }
    chunks
}

/// All the memory of one test run. Dropping it frees every chunk and then
/// puts back any working-set change made to lock them.
#[derive(Debug)]
pub(crate) struct Arena {
    // Field order matters: fields drop in declaration order, so the chunks
    // (and their locks) are released before the working set is shrunk back.
    chunks: Vec<Chunk>,
    #[cfg(windows)]
    _working_set: Option<win::WorkingSet>,
}

impl Arena {
    /// Allocates as much test memory as is safe, at most `max_bytes`, in
    /// chunks of at most `ctx.chunk_bytes`. The result may be empty when
    /// nothing could be allocated or when `ctx.stop` was set during
    /// allocation.
    ///
    /// On Windows: reads physical and commit figures, aims for [`targets`],
    /// and locks each chunk into RAM, raising the process's minimum working
    /// set a chunk at a time (locking is limited by it). If no chunk at all
    /// can be locked, it uses unlocked memory instead, up to the unlocked
    /// target.
    ///
    /// Elsewhere (development only): ordinary heap memory, 256 MiB unless
    /// `max_bytes` says otherwise, never locked.
    pub(crate) fn allocate(max_bytes: Option<u64>, ctx: &AllocContext<'_>) -> Arena {
        #[cfg(windows)]
        {
            let Some(status) = win::memory_status() else {
                return Arena {
                    chunks: Vec::new(),
                    _working_set: None,
                };
            };
            let targets = targets(&status, max_bytes);
            let mut working_set = win::WorkingSet::acquire();
            let chunks = grow(
                ctx,
                targets.locked,
                working_set.as_mut().map(|w| w as &mut dyn Pinner),
                targets.unlocked,
            );
            if !chunks.iter().any(|c| c.locked) {
                // Nothing is locked, so the working set (already put back by
                // the failed pin) need not be held.
                working_set = None;
            }
            Arena {
                chunks,
                _working_set: working_set,
            }
        }
        #[cfg(not(windows))]
        {
            let target = max_bytes.unwrap_or(DEV_DEFAULT_BYTES);
            Arena {
                chunks: grow(ctx, 0, None, target),
            }
        }
    }

    /// The allocated chunks.
    pub(crate) fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// Bytes allocated in total.
    pub(crate) fn total_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.bytes as u64).sum()
    }

    /// Bytes locked into physical RAM.
    pub(crate) fn locked_bytes(&self) -> u64 {
        self.chunks
            .iter()
            .filter(|c| c.locked)
            .map(|c| c.bytes as u64)
            .sum()
    }
}

#[cfg(test)]
thread_local! {
    /// Bytes currently held in chunks allocated by this thread, so tests can
    /// prove that everything was freed.
    static LIVE_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Bytes still allocated in chunks created on the calling thread.
#[cfg(test)]
pub(crate) fn live_bytes_on_this_thread() -> u64 {
    LIVE_BYTES.with(|b| b.get())
}

/// Serialises tests that allocate through [`Arena::allocate`] (directly or by
/// running the engine): the working set is a per-process setting, so only one
/// arena at a time can lock memory.
#[cfg(test)]
pub(crate) static TEST_ALLOC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn test_alloc_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(windows)]
mod win {
    //! The Win32 calls behind [`Arena::allocate`](super::Arena::allocate).

    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_INC_WORKING_SET_NAME,
        SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
    };
    use windows_sys::Win32::System::Memory::{
        GetProcessWorkingSetSizeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
        QUOTA_LIMITS_HARDWS_MAX_DISABLE, QUOTA_LIMITS_HARDWS_MIN_DISABLE,
        SetProcessWorkingSetSizeEx, VirtualAlloc, VirtualFree, VirtualLock, VirtualUnlock,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::{Chunk, MemoryStatus, Pinner};

    /// Extra minimum working set kept on top of the locked bytes, for the
    /// "small overhead" Windows subtracts from the minimum working set when
    /// deciding how much a process may lock.
    const WORKING_SET_SLACK: usize = 16 << 20;

    /// Set while a [`WorkingSet`] exists. The working set is one setting for
    /// the whole process, so two arenas changing it at once would undo each
    /// other's changes.
    static WORKING_SET_IN_USE: AtomicBool = AtomicBool::new(false);

    /// Physical memory and commit figures for the whole system.
    pub(super) fn memory_status() -> Option<MemoryStatus> {
        let mut status = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: `status` is a writable MEMORYSTATUSEX with `dwLength` set,
        // as the function requires.
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        (ok != 0).then_some(MemoryStatus {
            total_phys: status.ullTotalPhys,
            avail_phys: status.ullAvailPhys,
            // Despite the names, these two are the system commit limit and
            // the commit still available, not page-file sizes.
            commit_limit: status.ullTotalPageFile,
            avail_commit: status.ullAvailPageFile,
        })
    }

    /// Reserves and commits `bytes` of fresh read/write memory. Null on
    /// failure.
    pub(super) fn alloc(bytes: usize) -> *mut c_void {
        // SAFETY: a null address asks the system to choose where; no existing
        // memory is touched.
        unsafe { VirtualAlloc(ptr::null(), bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }
    }

    /// Unlocks (if `locked`) and releases a region returned by [`alloc`].
    ///
    /// # Safety
    ///
    /// `ptr` must be the start of a region from [`alloc`] of `bytes` bytes
    /// that is not used afterwards and not freed again.
    pub(super) unsafe fn free(ptr: *mut c_void, bytes: usize, locked: bool) {
        if locked {
            // SAFETY: the caller guarantees the region is ours and live.
            // Failure is harmless: releasing the region below drops the lock.
            unsafe { VirtualUnlock(ptr, bytes) };
        }
        // SAFETY: the caller guarantees `ptr` is the base of a live region
        // from VirtualAlloc; MEM_RELEASE requires size 0 to free all of it.
        unsafe { VirtualFree(ptr, 0, MEM_RELEASE) };
    }

    /// Raises the process's minimum working set as chunks are locked, and
    /// puts the original limits back when dropped.
    #[derive(Debug)]
    pub(crate) struct WorkingSet {
        original_min: usize,
        original_max: usize,
        original_flags: u32,
        /// Bytes locked so far through this working set.
        pinned: usize,
    }

    impl WorkingSet {
        /// Records the current working-set limits so they can be restored.
        /// Returns `None` when they cannot be read or another arena is
        /// already using the working set; memory is then not locked.
        pub(super) fn acquire() -> Option<WorkingSet> {
            if WORKING_SET_IN_USE
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                return None;
            }
            enable_increase_working_set_privilege();
            let (mut min, mut max, mut flags) = (0usize, 0usize, 0u32);
            // SAFETY: the pseudo handle from GetCurrentProcess is always
            // valid for the current process, and the three out pointers point
            // at live locals of the right types.
            let ok = unsafe {
                GetProcessWorkingSetSizeEx(GetCurrentProcess(), &mut min, &mut max, &mut flags)
            };
            if ok == 0 {
                WORKING_SET_IN_USE.store(false, Ordering::Release);
                return None;
            }
            Some(WorkingSet {
                original_min: min,
                original_max: max,
                original_flags: flags,
                pinned: 0,
            })
        }

        /// Sets the working-set limits needed to hold `pinned` locked bytes,
        /// or the original limits when `pinned` is 0. Returns whether Windows
        /// accepted them.
        fn apply(&self, pinned: usize) -> bool {
            let (min, max, flags) = if pinned == 0 {
                (self.original_min, self.original_max, self.original_flags)
            } else {
                let min = self
                    .original_min
                    .saturating_add(pinned)
                    .saturating_add(WORKING_SET_SLACK);
                let max = self.original_max.max(min.saturating_add(WORKING_SET_SLACK));
                // Soft limits: the minimum is what allows locking; the
                // maximum must not stop the rest of the program growing.
                let flags = QUOTA_LIMITS_HARDWS_MIN_DISABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE;
                (min, max, flags)
            };
            // SAFETY: plain values and the always-valid current-process
            // pseudo handle.
            unsafe { SetProcessWorkingSetSizeEx(GetCurrentProcess(), min, max, flags) != 0 }
        }
    }

    impl Pinner for WorkingSet {
        fn pin(&mut self, chunk: &Chunk) -> bool {
            let wanted = self.pinned.saturating_add(chunk.bytes());
            if !self.apply(wanted) {
                return false;
            }
            // SAFETY: the chunk is a live committed read/write region of
            // exactly `bytes()` bytes owned by us.
            if unsafe { VirtualLock(chunk.ptr().cast(), chunk.bytes()) } == 0 {
                // Give back the extra minimum that was raised for this chunk.
                self.apply(self.pinned);
                return false;
            }
            self.pinned = wanted;
            true
        }
    }

    impl Drop for WorkingSet {
        fn drop(&mut self) {
            // Nothing useful can be done if this fails; the limits are
            // per-process and go away with the process anyway.
            self.apply(0);
            WORKING_SET_IN_USE.store(false, Ordering::Release);
        }
    }

    /// Enables SeIncreaseWorkingSetPrivilege in the process token. Standard
    /// users hold this privilege but it starts disabled, and raising the
    /// working set above its current size needs it. Best effort: on failure
    /// raising the working set fails later and memory is used unlocked.
    fn enable_increase_working_set_privilege() {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: valid pseudo handle and a live out pointer for the token.
        let opened =
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token) };
        if opened == 0 {
            return;
        }
        let mut luid = LUID::default();
        // SAFETY: null system name means the local system; the name is a
        // static NUL-terminated wide string; `luid` is a live out pointer.
        let found =
            unsafe { LookupPrivilegeValueW(ptr::null(), SE_INC_WORKING_SET_NAME, &mut luid) };
        if found != 0 {
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            // SAFETY: `token` was opened with TOKEN_ADJUST_PRIVILEGES;
            // `privileges` is a valid one-entry list; no previous state is
            // requested, so the buffer length is 0 and both out pointers null.
            unsafe {
                AdjustTokenPrivileges(token, 0, &privileges, 0, ptr::null_mut(), ptr::null_mut())
            };
        }
        // SAFETY: `token` is a handle we opened above and close exactly once.
        unsafe { CloseHandle(token) };
    }

    #[cfg(test)]
    pub(super) fn current_working_set() -> (usize, usize) {
        let (mut min, mut max, mut flags) = (0usize, 0usize, 0u32);
        // SAFETY: as in `WorkingSet::acquire`.
        let ok = unsafe {
            GetProcessWorkingSetSizeEx(GetCurrentProcess(), &mut min, &mut max, &mut flags)
        };
        assert_ne!(ok, 0, "GetProcessWorkingSetSizeEx failed");
        (min, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    /// Small chunks so the tests allocate little memory.
    const SMALL: usize = 64 * 1024;

    /// The counters an `AllocContext` points at.
    struct Counters {
        stop: AtomicBool,
        target: AtomicU64,
        obtained: AtomicU64,
    }

    impl Counters {
        fn new() -> Counters {
            Counters {
                stop: AtomicBool::new(false),
                target: AtomicU64::new(0),
                obtained: AtomicU64::new(0),
            }
        }

        fn ctx(&self, chunk_bytes: usize, refuse_after: Option<usize>) -> AllocContext<'_> {
            AllocContext {
                chunk_bytes,
                stop: &self.stop,
                target: &self.target,
                obtained: &self.obtained,
                refuse_after,
            }
        }
    }

    fn status(
        total_phys: u64,
        avail_phys: u64,
        commit_limit: u64,
        avail_commit: u64,
    ) -> MemoryStatus {
        MemoryStatus {
            total_phys,
            avail_phys,
            commit_limit,
            avail_commit,
        }
    }

    #[test]
    fn physical_headroom_is_the_larger_of_1_gib_and_10_percent() {
        // Commit is plentiful here, so physical memory decides.
        let plenty = 4096 * GIB;
        let locked = |total, avail, cap| targets(&status(total, avail, plenty, plenty), cap).locked;
        // 64 GiB machine: 10% (6.4 GiB) beats 1 GiB.
        assert_eq!(locked(64 * GIB, 60 * GIB, None), 60 * GIB - 64 * GIB / 10);
        // 8 GiB machine: 1 GiB beats 10% (0.8 GiB).
        assert_eq!(locked(8 * GIB, 6 * GIB, None), 5 * GIB);
        // Less available than headroom: nothing.
        assert_eq!(locked(8 * GIB, GIB / 2, None), 0);
        // The cap applies only when smaller.
        assert_eq!(locked(8 * GIB, 6 * GIB, Some(64 * MIB)), 64 * MIB);
        assert_eq!(locked(8 * GIB, 6 * GIB, Some(100 * GIB)), 5 * GIB);
        // Sizes far above 64 GiB do not wrap.
        assert_eq!(
            locked(1024 * GIB, 1000 * GIB, None),
            1000 * GIB - 1024 * GIB / 10
        );
    }

    #[test]
    fn commit_bounds_the_target_when_it_is_tighter() {
        // 64 GiB machine with the page file disabled: the commit limit equals
        // physical memory, and other programs have committed more than they
        // keep resident. Physical alone would give 48 - 6.4 = 41.6 GiB.
        let t = targets(&status(64 * GIB, 48 * GIB, 64 * GIB, 42 * GIB), None);
        assert_eq!(t.locked, 42 * GIB - 64 * GIB / 10);
        // Page file disabled and little commit left: nothing to test.
        let t = targets(&status(16 * GIB, 8 * GIB, 16 * GIB, GIB), None);
        assert_eq!(
            t,
            Targets {
                locked: 0,
                unlocked: 0
            }
        );
        // A large system-managed page file: plenty of commit, physical decides.
        let t = targets(&status(64 * GIB, 48 * GIB, 128 * GIB, 100 * GIB), None);
        assert_eq!(t.locked, 48 * GIB - 64 * GIB / 10);
        // Above 64 GiB, with commit tighter and its headroom taken from the
        // commit limit (10% of 300 GiB), not from physical memory.
        let t = targets(&status(256 * GIB, 240 * GIB, 300 * GIB, 200 * GIB), None);
        assert_eq!(t.locked, 200 * GIB - 30 * GIB);
        // A small commit limit uses the 1 GiB minimum headroom.
        let t = targets(&status(4 * GIB, 3 * GIB, 5 * GIB, 2 * GIB), None);
        assert_eq!(t.locked, GIB);
    }

    #[test]
    fn unlocked_target_is_bounded_by_commit_and_half_of_free_ram() {
        // Commit tighter than half the free RAM.
        let t = targets(&status(64 * GIB, 60 * GIB, 64 * GIB, 20 * GIB), None);
        assert_eq!(t.unlocked, 20 * GIB - 64 * GIB / 10);
        // Half the free RAM tighter than commit.
        let t = targets(&status(64 * GIB, 60 * GIB, 1024 * GIB, 1000 * GIB), None);
        assert_eq!(t.unlocked, 30 * GIB);
        // The cap applies to the fallback too.
        let t = targets(
            &status(64 * GIB, 60 * GIB, 1024 * GIB, 1000 * GIB),
            Some(MIB),
        );
        assert_eq!(
            t,
            Targets {
                locked: MIB,
                unlocked: MIB
            }
        );
    }

    #[test]
    fn chunk_lengths_are_capped_and_page_rounded() {
        assert_eq!(next_chunk_len(100 * GIB, CHUNK_BYTES), CHUNK_BYTES);
        assert_eq!(
            next_chunk_len(CHUNK_BYTES as u64 + 5, CHUNK_BYTES),
            CHUNK_BYTES
        );
        assert_eq!(next_chunk_len(10_000, CHUNK_BYTES), 8192);
        assert_eq!(next_chunk_len(4095, CHUNK_BYTES), 0);
        assert_eq!(next_chunk_len(0, CHUNK_BYTES), 0);
    }

    /// Pins the first `succeed` chunks it is asked about, then refuses.
    struct ScriptedPinner {
        succeed: usize,
        asked: usize,
    }

    impl Pinner for ScriptedPinner {
        fn pin(&mut self, _chunk: &Chunk) -> bool {
            self.asked += 1;
            self.asked <= self.succeed
        }
    }

    fn sizes(chunks: &[Chunk]) -> Vec<usize> {
        chunks.iter().map(|c| c.bytes()).collect()
    }

    fn always_pins() -> ScriptedPinner {
        ScriptedPinner {
            succeed: usize::MAX,
            asked: 0,
        }
    }

    #[test]
    fn growth_stops_at_the_first_chunk_that_cannot_be_pinned() {
        let c = Counters::new();
        let before = live_bytes_on_this_thread();
        let mut pinner = ScriptedPinner {
            succeed: 3,
            asked: 0,
        };
        let chunks = grow(
            &c.ctx(SMALL, None),
            10 * SMALL as u64,
            Some(&mut pinner),
            u64::MAX,
        );
        assert_eq!(pinner.asked, 4);
        assert_eq!(sizes(&chunks), vec![SMALL; 3]);
        assert!(chunks.iter().all(|c| c.locked));
        // The chunk that failed to pin was freed straight away.
        assert_eq!(live_bytes_on_this_thread() - before, 3 * SMALL as u64);
        drop(chunks);
        assert_eq!(live_bytes_on_this_thread(), before);
    }

    #[test]
    fn growth_reaches_an_uneven_target_when_everything_pins() {
        let c = Counters::new();
        let mut pinner = always_pins();
        let target = 2 * SMALL as u64 + SMALL as u64 / 2;
        let chunks = grow(&c.ctx(SMALL, None), target, Some(&mut pinner), u64::MAX);
        assert_eq!(sizes(&chunks), vec![SMALL, SMALL, SMALL / 2]);
        assert!(chunks.iter().all(|c| c.locked));
    }

    #[test]
    fn falls_back_to_unlocked_memory_up_to_its_own_cap() {
        let c = Counters::new();
        let mut pinner = ScriptedPinner {
            succeed: 0,
            asked: 0,
        };
        let unlocked_target = 2 * SMALL as u64 + 3 * PAGE_BYTES as u64 + 7;
        let chunks = grow(
            &c.ctx(SMALL, None),
            10 * SMALL as u64,
            Some(&mut pinner),
            unlocked_target,
        );
        assert_eq!(pinner.asked, 1);
        assert_eq!(sizes(&chunks), vec![SMALL, SMALL, 3 * PAGE_BYTES]);
        assert!(chunks.iter().all(|c| !c.locked));
    }

    #[test]
    fn without_a_pinner_memory_is_unlocked() {
        let c = Counters::new();
        let chunks = grow(&c.ctx(SMALL, None), 0, None, 3 * SMALL as u64);
        assert_eq!(sizes(&chunks), vec![SMALL; 3]);
        assert!(chunks.iter().all(|c| !c.locked));
    }

    #[test]
    fn a_refused_allocation_part_way_keeps_what_was_obtained() {
        let before = live_bytes_on_this_thread();
        // Locked growth: the third allocation is refused (commit exhausted).
        let c = Counters::new();
        let mut pinner = always_pins();
        let chunks = grow(
            &c.ctx(SMALL, Some(2)),
            10 * SMALL as u64,
            Some(&mut pinner),
            u64::MAX,
        );
        assert_eq!(sizes(&chunks), vec![SMALL; 2]);
        assert!(chunks.iter().all(|c| c.locked));
        assert_eq!(pinner.asked, 2);
        drop(chunks);
        // Unlocked growth: same.
        let c = Counters::new();
        let chunks = grow(&c.ctx(SMALL, Some(3)), 0, None, 10 * SMALL as u64);
        assert_eq!(sizes(&chunks), vec![SMALL; 3]);
        drop(chunks);
        // Refused from the start, with and without locking: nothing at all.
        let c = Counters::new();
        let mut pinner = always_pins();
        let chunks = grow(
            &c.ctx(SMALL, Some(0)),
            10 * SMALL as u64,
            Some(&mut pinner),
            u64::MAX,
        );
        assert!(chunks.is_empty());
        assert_eq!(pinner.asked, 0);
        assert_eq!(live_bytes_on_this_thread(), before);
    }

    #[test]
    fn growth_reports_its_target_and_what_it_obtained() {
        // Locked: the target is the locked target, and only kept chunks count.
        let c = Counters::new();
        let mut pinner = ScriptedPinner {
            succeed: 2,
            asked: 0,
        };
        let _chunks = grow(
            &c.ctx(SMALL, None),
            5 * SMALL as u64,
            Some(&mut pinner),
            u64::MAX,
        );
        assert_eq!(c.target.load(Ordering::Relaxed), 5 * SMALL as u64);
        assert_eq!(c.obtained.load(Ordering::Relaxed), 2 * SMALL as u64);
        // Fallback: the target switches to the unlocked target.
        let c = Counters::new();
        let mut pinner = ScriptedPinner {
            succeed: 0,
            asked: 0,
        };
        let _chunks = grow(
            &c.ctx(SMALL, None),
            5 * SMALL as u64,
            Some(&mut pinner),
            3 * SMALL as u64,
        );
        assert_eq!(c.target.load(Ordering::Relaxed), 3 * SMALL as u64);
        assert_eq!(c.obtained.load(Ordering::Relaxed), 3 * SMALL as u64);
    }

    #[test]
    fn a_stop_request_ends_growth_with_nothing_more_allocated() {
        let c = Counters::new();
        c.stop.store(true, Ordering::Relaxed);
        let mut pinner = always_pins();
        let chunks = grow(
            &c.ctx(SMALL, None),
            10 * SMALL as u64,
            Some(&mut pinner),
            10 * SMALL as u64,
        );
        assert!(chunks.is_empty());
        assert_eq!(pinner.asked, 0);
    }

    #[test]
    fn arena_respects_the_cap_and_memory_is_usable() {
        let _serial = test_alloc_lock();
        let c = Counters::new();
        let before = live_bytes_on_this_thread();
        let arena = Arena::allocate(Some(MIB + 100), &c.ctx(CHUNK_BYTES, None));
        assert_eq!(arena.total_bytes(), MIB);
        assert_eq!(live_bytes_on_this_thread() - before, MIB);
        for c in arena.chunks() {
            let words = c.bytes() / 8;
            // SAFETY: both indexes are inside the chunk, which is live and
            // used by nothing else.
            unsafe {
                c.ptr().write_volatile(1);
                c.ptr().add(words - 1).write_volatile(2);
                assert_eq!(c.ptr().read_volatile(), 1);
                assert_eq!(c.ptr().add(words - 1).read_volatile(), 2);
            }
        }
        drop(arena);
        assert_eq!(live_bytes_on_this_thread(), before);
    }

    #[test]
    fn arena_with_a_zero_cap_is_empty() {
        let _serial = test_alloc_lock();
        let c = Counters::new();
        let arena = Arena::allocate(Some(0), &c.ctx(CHUNK_BYTES, None));
        assert_eq!(arena.total_bytes(), 0);
        assert!(arena.chunks().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_locks_the_memory_and_restores_the_working_set() {
        let _serial = test_alloc_lock();
        let c = Counters::new();
        let original = win::current_working_set();
        let arena = Arena::allocate(Some(64 * MIB), &c.ctx(CHUNK_BYTES, None));
        assert_eq!(arena.total_bytes(), 64 * MIB);
        assert_eq!(arena.locked_bytes(), 64 * MIB);
        let (raised_min, _) = win::current_working_set();
        assert!(
            raised_min as u64 >= 64 * MIB,
            "minimum working set {raised_min}"
        );
        drop(arena);
        assert_eq!(win::current_working_set(), original);
        assert!(total_physical_bytes().is_some_and(|t| t >= 64 * MIB));
    }

    #[cfg(not(windows))]
    #[test]
    fn dev_default_is_256_mib_unlocked() {
        let _serial = test_alloc_lock();
        let c = Counters::new();
        let arena = Arena::allocate(None, &c.ctx(CHUNK_BYTES, None));
        assert_eq!(arena.total_bytes(), 256 * MIB);
        assert_eq!(arena.locked_bytes(), 0);
        assert_eq!(total_physical_bytes(), None);
    }
}
