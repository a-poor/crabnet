//! Test-only forwarding VFS for DELETE-journal I/O fault injection.
//!
//! One process-lifetime registration (never the default VFS) delegates to the
//! original native VFS. Per-path registrations are removed on drop; open files
//! retain their own Arc so even a connection outliving its controller is safe.
//! Registration can precede connection construction to cover header reads;
//! per-path live-file counts check failed-open cleanup without changing I/O.
//! xRead/xWrite/xSync faults precede the native operation. Opt-in journal
//! xDelete faults precede deletion or follow successful native deletion. Each
//! fault is consumed once, leaving later I/O healthy. Version-1 file methods
//! exclude WAL and mmap; neither is part of this harness's contract.
//!
//! FFI invariants: SQLite allocates an aligned, szOsFile-sized File; its repr(C)
//! header is first. Native file storage comes from sqlite3_malloc64 and stays
//! alive until xClose. All callbacks receive the original native file/VFS, not
//! our wrapper. The built-in VFS and our registered method tables live for the
//! process lifetime. No test assertion or poisoned-lock panic crosses the ABI.

use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_int, c_void},
    path::Path,
    ptr,
    sync::{Arc, Mutex, MutexGuard, Once, OnceLock},
};

use rusqlite::{Connection, OpenFlags, ffi};

use crate::Database;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileKind {
    Database,
    Journal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    Read,
    Write,
    Sync,
    Delete,
}

impl Operation {
    pub(super) fn error_code(self) -> c_int {
        match self {
            Self::Read => ffi::SQLITE_IOERR_READ,
            Self::Write => ffi::SQLITE_IOERR_WRITE,
            Self::Sync => ffi::SQLITE_IOERR_FSYNC,
            Self::Delete => ffi::SQLITE_IOERR_DELETE,
        }
    }
}

/// Before unlink, or after successful native deletion (a directory-sync error).
#[derive(Clone, Copy, Debug)]
pub(super) enum DeleteFault {
    Before,
    After,
}

impl DeleteFault {
    pub(super) fn error_code(self) -> c_int {
        match self {
            Self::Before => ffi::SQLITE_IOERR_DELETE,
            Self::After => ffi::SQLITE_IOERR_DIR_FSYNC,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Event {
    pub file: FileKind,
    pub operation: Operation,
    pub offset: i64,
    // Read/write byte count, file sync flags, or xDelete directory-sync flag.
    pub amount: c_int,
    pub result: c_int,
}

#[derive(Default)]
struct State {
    open_files: usize,
    recording: bool,
    reads: bool,
    deletes: bool,
    delete_fault: Option<DeleteFault>,
    fail_at: Option<usize>,
    target: Option<Target>,
    events: Vec<Event>,
}

struct Target {
    file: FileKind,
    operation: Operation,
    offset: i64,
    amount: c_int,
    remaining: usize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

type Registry = HashMap<Vec<u8>, Arc<Mutex<State>>>;

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// Clones share only this database's fault state, e.g. with a commit hook.
#[derive(Clone)]
pub(super) struct Control(Arc<Mutex<State>>);

impl Control {
    pub(super) fn start(&self, fail_at: Option<usize>) {
        let mut state = lock(&self.0);
        *state = State {
            open_files: state.open_files,
            recording: true,
            reads: false,
            deletes: false,
            delete_fault: None,
            fail_at,
            target: None,
            events: Vec::new(),
        };
    }

    /// Deletion recording is opt-in so existing read/write/sync traces stay intact.
    pub(super) fn start_deletes(&self, fault: Option<DeleteFault>) {
        self.start(None);
        let mut state = lock(&self.0);
        state.deletes = true;
        state.delete_fault = fault;
    }

    /// Keep existing write/sync sweeps unchanged; source-read sweeps opt in.
    pub(super) fn start_reads(&self) {
        self.start(None);
        lock(&self.0).reads = true;
    }

    /// Target the nth read of these bytes. Integrity-check scan order can vary
    /// between connections, so a global read ordinal is not a stable target.
    pub(super) fn fail_read(&self, expected: &Event, occurrence: usize) {
        assert_eq!(expected.operation, Operation::Read);
        self.fail_io(expected, occurrence);
    }

    /// Select an operation on the database or its journal without confusing
    /// equal byte ranges across files or reads with recovery writes/syncs.
    pub(super) fn fail_io(&self, expected: &Event, occurrence: usize) {
        lock(&self.0).target = Some(Target {
            file: expected.file,
            operation: expected.operation,
            offset: expected.offset,
            amount: expected.amount,
            remaining: occurrence,
        });
    }

    pub(super) fn stop(&self) -> Vec<Event> {
        let mut state = lock(&self.0);
        state.recording = false;
        state.delete_fault = None;
        state.fail_at = None;
        state.target = None;
        std::mem::take(&mut state.events)
    }

    pub(super) fn event_count(&self) -> usize {
        lock(&self.0).events.len()
    }

    pub(super) fn open_file_count(&self) -> usize {
        lock(&self.0).open_files
    }
}

pub(super) struct Registration {
    path: Vec<u8>,
    pub control: Control,
}

impl Registration {
    /// Connect only after the caller has armed opening faults, returning the
    /// original SQLite error if even connection construction fails.
    pub(super) fn connect(&self, flags: OpenFlags) -> rusqlite::Result<Connection> {
        Connection::open_with_flags_and_vfs(
            std::str::from_utf8(&self.path).unwrap(),
            flags,
            c"crabnet-test-commit-io",
        )
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        lock(registry()).remove(&self.path);
    }
}

pub(super) fn open(path: &Path) -> (Database, Registration) {
    let (connection, registration) = connect(path);
    (Database::from_connection(connection).unwrap(), registration)
}

/// Connect without running `crabnet` initialization, so migration faults can be
/// armed before any schema work. The fixture file must already exist.
pub(super) fn connect(path: &Path) -> (Connection, Registration) {
    connect_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
}

pub(super) fn connect_with_flags(path: &Path, access: OpenFlags) -> (Connection, Registration) {
    let registration = register(path);
    let connection = registration
        .connect(access | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE)
        .unwrap();
    (connection, registration)
}

/// Register a fixture independently of connection construction so faults can
/// cover opening headers and initialization. An absent leaf is allowed when
/// its parent exists; registration itself never creates the database file.
pub(super) fn register(path: &Path) -> Registration {
    register_vfs();
    // Canonicalize existing files or an absent leaf's parent to match
    // xFullPathname through temporary-directory symlinks (e.g. /var on macOS).
    let path = if path.try_exists().unwrap() {
        path.canonicalize().unwrap()
    } else {
        path.parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join(path.file_name().unwrap())
    };
    let key = path.to_str().unwrap().as_bytes().to_vec();
    let control = Control(Arc::default());
    {
        let mut entries = lock(registry());
        assert!(
            !entries.contains_key(&key),
            "fixture path is already registered"
        );
        entries.insert(key.clone(), control.0.clone());
    }
    Registration { path: key, control }
}

#[repr(C)]
struct File {
    header: ffi::sqlite3_file,
    native: *mut ffi::sqlite3_file,
    state: Option<Arc<Mutex<State>>>,
    kind: Option<FileKind>,
}

// SAFETY: Called only for our registered VFS, whose pAppData is the built-in VFS.
unsafe fn native_vfs(vfs: *mut ffi::sqlite3_vfs) -> *mut ffi::sqlite3_vfs {
    unsafe { (*vfs).pAppData.cast() }
}

unsafe extern "C" fn open_file(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    // SAFETY: SQLite supplies all buffers under the VFS contract. On any open
    // failure we close/free native storage and leave pMethods null, preventing
    // SQLite from calling xClose on a partially initialized Rust File.
    unsafe {
        (*file).pMethods = ptr::null();
        let base = native_vfs(vfs);
        let size = (*base).szOsFile as usize;
        let native = ffi::sqlite3_malloc64(size as u64).cast::<ffi::sqlite3_file>();
        if native.is_null() {
            return ffi::SQLITE_NOMEM;
        }
        ptr::write_bytes(native.cast::<u8>(), 0, size);
        let result = (*base).xOpen.unwrap()(base, name, native, flags, out_flags);
        if result != ffi::SQLITE_OK {
            if !(*native).pMethods.is_null() {
                (*(*native).pMethods).xClose.unwrap()(native);
            }
            ffi::sqlite3_free(native.cast());
            return result;
        }
        let kind = if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
            Some(FileKind::Database)
        } else if flags & ffi::SQLITE_OPEN_MAIN_JOURNAL != 0 {
            Some(FileKind::Journal)
        } else {
            None
        };
        let state = if name.is_null() {
            None
        } else {
            let name = CStr::from_ptr(name).to_bytes();
            let key = match kind {
                Some(FileKind::Database) => Some(name),
                Some(FileKind::Journal) => name.strip_suffix(b"-journal"),
                None => None,
            };
            key.and_then(|key| lock(registry()).get(key).cloned())
        };
        if let Some(state) = &state {
            lock(state).open_files += 1;
        }
        file.cast::<File>().write(File {
            header: ffi::sqlite3_file { pMethods: &METHODS },
            native,
            state,
            kind,
        });
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn close_file(file: *mut ffi::sqlite3_file) -> c_int {
    // SAFETY: xClose is called exactly once for a successfully opened File.
    unsafe {
        let wrapper = file.cast::<File>();
        let native = (*wrapper).native;
        let result = (*(*native).pMethods).xClose.unwrap()(native);
        ffi::sqlite3_free(native.cast());
        if let Some(state) = &(*wrapper).state {
            let mut state = lock(state);
            state.open_files = state.open_files.saturating_sub(1);
        }
        ptr::drop_in_place(wrapper);
        (*file).pMethods = ptr::null();
        result
    }
}

// SAFETY: The wrapper and its native file remain live throughout this callback.
unsafe fn io(
    file: *mut ffi::sqlite3_file,
    operation: Operation,
    offset: i64,
    amount: c_int,
    forward: impl FnOnce(*mut ffi::sqlite3_file) -> c_int,
) -> c_int {
    unsafe {
        let wrapper = &*file.cast::<File>();
        let Some((state, kind)) = wrapper.state.as_ref().zip(wrapper.kind) else {
            return forward(wrapper.native);
        };
        let mut state = lock(state);
        if !state.recording || (operation == Operation::Read && !state.reads) {
            return forward(wrapper.native);
        }
        let target_fail = state.target.as_mut().is_some_and(|target| {
            if (target.file, target.operation, target.offset, target.amount)
                != (kind, operation, offset, amount)
            {
                false
            } else if target.remaining == 0 {
                true
            } else {
                target.remaining -= 1;
                false
            }
        });
        let fail = target_fail || state.fail_at == Some(state.events.len());
        let result = if fail {
            state.fail_at = None;
            state.target = None;
            operation.error_code()
        } else {
            forward(wrapper.native)
        };
        state.events.push(Event {
            file: kind,
            operation,
            offset,
            amount,
            result,
        });
        result
    }
}

unsafe extern "C" fn write_file(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    unsafe {
        io(file, Operation::Write, offset, amount, |native| {
            (*(*native).pMethods).xWrite.unwrap()(native, buffer, amount, offset)
        })
    }
}

unsafe extern "C" fn read_file(
    file: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    unsafe {
        io(file, Operation::Read, offset, amount, |native| {
            (*(*native).pMethods).xRead.unwrap()(native, buffer, amount, offset)
        })
    }
}

unsafe extern "C" fn sync_file(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    unsafe {
        io(file, Operation::Sync, 0, flags, |native| {
            (*(*native).pMethods).xSync.unwrap()(native, flags)
        })
    }
}

// The remaining version-1 callbacks preserve native arguments and results.
macro_rules! file_forward {
    ($name:ident, $method:ident, ($($arg:ident: $ty:ty),*)) => {
        unsafe extern "C" fn $name(file: *mut ffi::sqlite3_file, $($arg: $ty),*) -> c_int {
            unsafe {
                let native = (*file.cast::<File>()).native;
                (*(*native).pMethods).$method.unwrap()(native, $($arg),*)
            }
        }
    };
}

file_forward!(truncate_file, xTruncate, (size: i64));
file_forward!(file_size, xFileSize, (size: *mut i64));
file_forward!(lock_file, xLock, (level: c_int));
file_forward!(unlock_file, xUnlock, (level: c_int));
file_forward!(reserved_lock, xCheckReservedLock, (result: *mut c_int));
file_forward!(file_control, xFileControl, (op: c_int, arg: *mut c_void));
file_forward!(sector_size, xSectorSize, ());
file_forward!(device_characteristics, xDeviceCharacteristics, ());

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(close_file),
    xRead: Some(read_file),
    xWrite: Some(write_file),
    xTruncate: Some(truncate_file),
    xSync: Some(sync_file),
    xFileSize: Some(file_size),
    xLock: Some(lock_file),
    xUnlock: Some(unlock_file),
    xCheckReservedLock: Some(reserved_lock),
    xFileControl: Some(file_control),
    xSectorSize: Some(sector_size),
    xDeviceCharacteristics: Some(device_characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};

macro_rules! vfs_forward {
    ($name:ident, $method:ident, ($($arg:ident: $ty:ty),*) -> $ret:ty) => {
        unsafe extern "C" fn $name(vfs: *mut ffi::sqlite3_vfs, $($arg: $ty),*) -> $ret {
            unsafe {
                let native = native_vfs(vfs);
                (*native).$method.unwrap()(native, $($arg),*)
            }
        }
    };
}

// SAFETY: SQLite supplies a live VFS and a NUL-terminated filename. Unlike
// file callbacks, xDelete has no file handle; scope it by the exact journal path.
unsafe extern "C" fn delete(vfs: *mut ffi::sqlite3_vfs, name: *const c_char, sync: c_int) -> c_int {
    unsafe {
        let native = native_vfs(vfs);
        let forward = || (*native).xDelete.unwrap()(native, name, sync);
        let state = CStr::from_ptr(name)
            .to_bytes()
            .strip_suffix(b"-journal")
            .and_then(|path| lock(registry()).get(path).cloned());
        let Some(state) = state else { return forward() };
        let mut state = lock(&state);
        if !state.recording || !state.deletes {
            return forward();
        }
        // Consume before returning, leaving recovery and later commits healthy.
        let result = match state.delete_fault.take() {
            Some(DeleteFault::Before) => DeleteFault::Before.error_code(),
            Some(DeleteFault::After) => {
                let result = forward();
                if result == ffi::SQLITE_OK {
                    DeleteFault::After.error_code()
                } else {
                    result
                }
            }
            None => forward(),
        };
        state.events.push(Event {
            file: FileKind::Journal,
            operation: Operation::Delete,
            offset: 0,
            amount: sync,
            result,
        });
        result
    }
}
type DlSymbol = Option<unsafe extern "C" fn(*mut ffi::sqlite3_vfs, *mut c_void, *const c_char)>;
vfs_forward!(access, xAccess, (name: *const c_char, flags: c_int, out: *mut c_int) -> c_int);
vfs_forward!(full_path, xFullPathname, (name: *const c_char, size: c_int, out: *mut c_char) -> c_int);
vfs_forward!(dl_open, xDlOpen, (name: *const c_char) -> *mut c_void);
vfs_forward!(dl_error, xDlError, (size: c_int, out: *mut c_char) -> ());
vfs_forward!(dl_sym, xDlSym, (handle: *mut c_void, name: *const c_char) -> DlSymbol);
vfs_forward!(dl_close, xDlClose, (handle: *mut c_void) -> ());
vfs_forward!(randomness, xRandomness, (size: c_int, out: *mut c_char) -> c_int);
vfs_forward!(sleep, xSleep, (micros: c_int) -> c_int);
vfs_forward!(current_time, xCurrentTime, (time: *mut f64) -> c_int);

unsafe extern "C" fn last_error(
    vfs: *mut ffi::sqlite3_vfs,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        let native = native_vfs(vfs);
        (*native)
            .xGetLastError
            .map_or(0, |callback| callback(native, size, out))
    }
}

fn register_vfs() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        // SAFETY: initialize/find/register are SQLite's thread-safe registry
        // APIs. The built-in VFS is never unregistered. Leak exactly one small
        // registration intentionally: SQLite can retain its pointer for any
        // connection/file lifetime, including across parallel test teardown.
        unsafe {
            assert_eq!(ffi::sqlite3_initialize(), ffi::SQLITE_OK);
            let base = ffi::sqlite3_vfs_find(ptr::null());
            assert!(!base.is_null());
            let vfs = Box::leak(Box::new(ffi::sqlite3_vfs {
                iVersion: 1,
                szOsFile: size_of::<File>().try_into().unwrap(),
                mxPathname: (*base).mxPathname,
                pNext: ptr::null_mut(),
                zName: c"crabnet-test-commit-io".as_ptr(),
                pAppData: base.cast(),
                xOpen: Some(open_file),
                xDelete: Some(delete),
                xAccess: Some(access),
                xFullPathname: Some(full_path),
                xDlOpen: Some(dl_open),
                xDlError: Some(dl_error),
                xDlSym: Some(dl_sym),
                xDlClose: Some(dl_close),
                xRandomness: Some(randomness),
                xSleep: Some(sleep),
                xCurrentTime: Some(current_time),
                xGetLastError: Some(last_error),
                xCurrentTimeInt64: None,
                xSetSystemCall: None,
                xGetSystemCall: None,
                xNextSystemCall: None,
            }));
            assert_eq!(ffi::sqlite3_vfs_register(vfs, 0), ffi::SQLITE_OK);
        }
    });
}
