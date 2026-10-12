//! Running backend-lang's machine code natively, in this process.
//!
//! The approach is backend-lang's own test loader (`tests/common/native.rs`
//! there), extended with what lowered HIR needs at run time:
//!
//! - **Executable memory** from the operating system (`mmap`/`mprotect` on
//!   Linux, `VirtualAlloc`/`VirtualProtect` on Windows, declared here: no
//!   crate). The module's code and data are copied in and every relocation
//!   is patched; the code pages become read-execute, the data pages stay
//!   read-write.
//! - **An entry thunk** assembled with isa-lang: called under SysV from Rust,
//!   it calls a compiled function under SysV or Win64 with arguments from an
//!   array, and returns a status. The trap handler (`__lexersketch_trap`)
//!   records the code and returns straight to the thunk.
//! - **An unwinder.** backend-lang 0.4 records each `invoke`'s landing pad
//!   and each function's frame layout but emits no unwind tables (its
//!   ROADMAP), so this file carries a native unwinder over those records,
//!   the same walk as backend-lang's emulator unwinder in
//!   `tests/stackmaps.rs`: `__lexersketch_resume` saves the machine state,
//!   a Rust routine walks the real stack frame by frame (restoring each
//!   unwound frame's callee-saved registers from where its prologue pushed
//!   them) to the first call site with a landing pad, and the stub enters
//!   the pad with the payload in `rax`. An exception that leaves the entry
//!   function returns to the thunk as [`Ran::Unwound`].
//! - **Host functions** (the exception-record allocator, the tests' host
//!   functions, the float `pow` runtime) as Rust `extern "sysv64"`
//!   functions behind a veneer per import that adapts the calling
//!   convention and can unwind (a host function that throws).
//!
//! x86-64 Linux and Windows only. `unsafe` lives here, in test support; the
//! library is `forbid(unsafe_code)`.

#![allow(unsafe_code, dead_code, clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::missing_safety_doc)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

use backend_lang::Compiled;
use backend_lang::x86_64::RelocKind;
use hir_lang::Prim;
use irgen_lang::rt;
use isa_lang::x86_64::{Assembler, Cond, Inst, Mem, Mnemonic as M, Operand as O, Reg, Size};

#[cfg(target_os = "linux")]
mod os {
    unsafe extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }

    /// # Safety
    ///
    /// `len` must be nonzero.
    pub unsafe fn alloc(len: usize) -> *mut u8 {
        // SAFETY: an anonymous private read-write mapping (PROT_READ |
        // PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS); MAP_FAILED is -1.
        let p = unsafe { mmap(std::ptr::null_mut(), len, 3, 0x22, -1, 0) };
        if p as isize == -1 {
            std::ptr::null_mut()
        } else {
            p
        }
    }

    /// # Safety
    ///
    /// `p..p+len` must be a page-aligned range of a mapping from `alloc`.
    pub unsafe fn protect_exec(p: *mut u8, len: usize) -> bool {
        // SAFETY: per the caller's contract; PROT_READ | PROT_EXEC.
        unsafe { mprotect(p, len, 5) == 0 }
    }

    /// # Safety
    ///
    /// `p` and `len` must be a mapping from `alloc`, not used afterwards.
    pub unsafe fn free(p: *mut u8, len: usize) {
        // SAFETY: per the caller's contract.
        let _ = unsafe { munmap(p, len) };
    }
}

#[cfg(windows)]
mod os {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualAlloc(addr: *mut u8, size: usize, kind: u32, protect: u32) -> *mut u8;
        fn VirtualProtect(addr: *mut u8, size: usize, protect: u32, old: *mut u32) -> i32;
        fn VirtualFree(addr: *mut u8, size: usize, kind: u32) -> i32;
    }

    /// # Safety
    ///
    /// `len` must be nonzero.
    pub unsafe fn alloc(len: usize) -> *mut u8 {
        // SAFETY: commits fresh read-write pages (MEM_COMMIT | MEM_RESERVE,
        // PAGE_READWRITE).
        unsafe { VirtualAlloc(std::ptr::null_mut(), len, 0x3000, 0x04) }
    }

    /// # Safety
    ///
    /// `p..p+len` must be within an allocation from `alloc`.
    pub unsafe fn protect_exec(p: *mut u8, len: usize) -> bool {
        let mut old = 0u32;
        // SAFETY: per the caller's contract; PAGE_EXECUTE_READ.
        unsafe { VirtualProtect(p, len, 0x20, &mut old) != 0 }
    }

    /// # Safety
    ///
    /// `p` must come from `alloc` and not be used afterwards.
    pub unsafe fn free(p: *mut u8, _len: usize) {
        // SAFETY: per the caller's contract; MEM_RELEASE with size 0.
        let _ = unsafe { VirtualFree(p, 0, 0x8000) };
    }
}

const PAGE: usize = 4096;

/// The most arguments a native call passes.
pub const MAX_ARGS: usize = 12;

/// A calling convention for the compiled code (and the matching backend
/// options).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conv {
    SysV,
    Win64,
}

impl Conv {
    /// Both conventions.
    pub const ALL: [Conv; 2] = [Conv::SysV, Conv::Win64];

    /// The integer argument registers, in order.
    fn arg_regs(self) -> &'static [u8] {
        match self {
            Conv::SysV => &[7, 6, 2, 1, 8, 9],
            Conv::Win64 => &[1, 2, 8, 9],
        }
    }

    /// backend-lang's options for this convention.
    pub fn options(self) -> backend_lang::Options {
        match self {
            Conv::SysV => backend_lang::Options::sysv(),
            Conv::Win64 => backend_lang::Options::win64(),
        }
    }
}

/// What a native call did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ran {
    /// Returned `rax`.
    Returned(u64),
    /// Called the trap handler with this code.
    Trap(u64),
    /// An exception left the entry function with this payload.
    Unwound(u64),
}

// ------------------------------------------------------------------ cells

// Words of the read-write cell block the stubs and the unwinder share.
/// The thunk's stack pointer, restored on a trap or an unwind out.
const CELL_THUNK_RSP: usize = 0;
/// Where the thunk writes `rax`.
const CELL_OUT: usize = 1;
/// The trap code, or the payload of an exception that left the entry.
const CELL_CODE: usize = 2;
/// The unwinder's stack pointer: in, at the resume call; out, at the pad.
const CELL_RSP: usize = 3;
/// The exception payload.
const CELL_PAYLOAD: usize = 4;
/// The landing pad's address.
const CELL_TARGET: usize = 5;
/// Sixteen words, the general-purpose registers by number (only the
/// callee-saved ones are read and written).
const CELL_REGS: usize = 6;
/// The address of the module's [`UnwindTable`].
const CELL_TABLE: usize = CELL_REGS + 16;
const CELLS: usize = CELL_TABLE + 1;

/// Registers callee-saved under SysV or Win64: every one the unwinder
/// restores (rbx, rbp, rsi, rdi, r12-r15).
const SAVED: [u8; 8] = [3, 5, 6, 7, 12, 13, 14, 15];

const STATUS_RETURNED: u64 = 0;
const STATUS_TRAP: u64 = 1;
const STATUS_UNWOUND: u64 = 2;
/// The unwinder met a return address in no compiled function.
const STATUS_LOST: u64 = 3;
/// The unwinder found a landing pad.
const LANDED: u64 = 0;

/// Landing pads the unwinder has entered, process-wide (tests read
/// differences, so concurrent test threads only add).
static LANDINGS: AtomicU64 = AtomicU64::new(0);

/// How many times the unwinder has entered a landing pad so far.
pub fn landings() -> u64 {
    LANDINGS.load(Ordering::Relaxed)
}

/// A frame walk bound: deeper than any test's call depth.
const MAX_FRAMES: usize = 1 << 20;

/// One compiled function, as the unwinder reads it.
struct FrameInfo {
    start: usize,
    end: usize,
    cfa: u64,
    frame_pointer: bool,
    /// Callee-saved registers the prologue pushed, in push order.
    pushed: Vec<usize>,
    /// (return offset, landing pad offset) per `invoke`.
    pads: Vec<(usize, usize)>,
}

struct UnwindTable {
    base: u64,
    /// The address after the thunk's call: unwinding to it leaves the entry.
    thunk_ret: u64,
    frames: Vec<FrameInfo>,
}

/// The unwinder's walk, called from the resume stub with the cell block.
/// Returns [`LANDED`] (the cells then hold the pad's stack pointer,
/// registers, and address), [`STATUS_UNWOUND`], or [`STATUS_LOST`].
extern "sysv64" fn unwind_plan(cells: *mut u64) -> u64 {
    // SAFETY: `cells` is the module's cell block (the resume stub passes
    // it); the table pointer in it is the `Native`'s boxed table, alive
    // while its code runs; every stack address read is within the frames
    // the walk crosses, which the frame records describe.
    unsafe { walk(cells) }
}

unsafe fn walk(cells: *mut u64) -> u64 {
    // SAFETY: see `unwind_plan`.
    unsafe {
        let table = &*(cells.add(CELL_TABLE).read() as *const UnwindTable);
        let mut regs = [0u64; 16];
        for r in SAVED {
            regs[r as usize] = cells.add(CELL_REGS + r as usize).read();
        }
        let mut rsp = cells.add(CELL_RSP).read();
        for _ in 0..MAX_FRAMES {
            let ret = (rsp as *const u64).read();
            if ret == table.thunk_ret {
                return STATUS_UNWOUND;
            }
            let Some(off) = ret.checked_sub(table.base).map(|o| o as usize) else {
                return STATUS_LOST;
            };
            let Some(f) = table.frames.iter().find(|f| off > f.start && off <= f.end) else {
                return STATUS_LOST;
            };
            // The caller's stack pointer at the call, before the return
            // address was pushed.
            let body = rsp + 8;
            if let Some(&(_, pad)) = f.pads.iter().find(|(r, _)| *r == off) {
                cells.add(CELL_RSP).write(body);
                cells.add(CELL_TARGET).write(table.base + pad as u64);
                for r in SAVED {
                    cells.add(CELL_REGS + r as usize).write(regs[r as usize]);
                }
                let _ = LANDINGS.fetch_add(1, Ordering::Relaxed);
                return LANDED;
            }
            // Unwind this frame: restore what its prologue saved.
            let cfa = body + f.cfa;
            let mut at = cfa - 16;
            if f.frame_pointer {
                regs[5] = (at as *const u64).read();
                at -= 8;
            }
            for &r in &f.pushed {
                regs[r] = (at as *const u64).read();
                at -= 8;
            }
            rsp = cfa - 8;
        }
        STATUS_LOST
    }
}

// ------------------------------------------------------------------ hosts

/// A host function's result: the value (`rax`) and whether it unwinds
/// with `value` as the payload instead of returning. Returned in
/// `rax:rdx` under SysV.
#[repr(C)]
pub struct HostRet {
    pub value: u64,
    pub unwind: u64,
}

/// A host function: up to two integer arguments.
pub type HostFn = extern "sysv64" fn(u64, u64) -> HostRet;

// Records and objects are boxed so their addresses stay fixed while the
// compiled code holds them and the vectors grow.
#[derive(Default)]
#[allow(clippy::vec_box)]
struct HostState {
    trace: Vec<i64>,
    records: Vec<Box<[u64; 2]>>,
    objects: Vec<Box<u64>>,
    object_set: BTreeSet<u64>,
}

thread_local! {
    static HOST: RefCell<HostState> = RefCell::new(HostState::default());
}

/// Clears the host state (trace, records, objects) before a run.
pub fn reset_host() {
    HOST.with(|h| *h.borrow_mut() = HostState::default());
}

/// The host trace of the runs since the last reset.
pub fn take_trace() -> Vec<i64> {
    HOST.with(|h| core::mem::take(&mut h.borrow_mut().trace))
}

/// The tag and bits of the exception record at `p`, if `p` is one the host
/// allocated.
pub fn record(p: u64) -> Option<(u32, u64)> {
    HOST.with(|h| {
        h.borrow()
            .records
            .iter()
            .find(|r| r.as_ptr() as u64 == p)
            .map(|r| (r[0] as u32, r[1]))
    })
}

fn new_record(tag: u32, bits: u64) -> u64 {
    // The record layout is irgen-lang's (`rt`): the tag at offset 0, the
    // bits at offset 8.
    debug_assert_eq!((rt::TAG_OFFSET, rt::BITS_OFFSET), (0, 8));
    HOST.with(|h| {
        let r = Box::new([u64::from(tag), bits]);
        let p = r.as_ptr() as u64;
        h.borrow_mut().records.push(r);
        p
    })
}

const fn ret(value: u64) -> HostRet {
    HostRet { value, unwind: 0 }
}

extern "sysv64" fn host_exception(tag: u64, bits: u64) -> HostRet {
    ret(new_record(tag as u32, bits))
}

extern "sysv64" fn host_trace(x: u64, _: u64) -> HostRet {
    HOST.with(|h| h.borrow_mut().trace.push(x as i64));
    ret(0)
}

extern "sysv64" fn host_nop(_: u64, _: u64) -> HostRet {
    ret(0)
}

/// `obj_new(v)`: an 8-byte object holding `v`. The objects never move
/// (a collector that never runs), so every stack map is trivially honored;
/// the GC-reference programs still exercise the spills and reloads around
/// each safepoint.
extern "sysv64" fn host_obj_new(v: u64, _: u64) -> HostRet {
    HOST.with(|h| {
        let mut h = h.borrow_mut();
        let o = Box::new(v);
        let p = &*o as *const u64 as u64;
        h.objects.push(o);
        let _ = h.object_set.insert(p);
        ret(p)
    })
}

extern "sysv64" fn host_obj_get(o: u64, _: u64) -> HostRet {
    HOST.with(|h| {
        let h = h.borrow();
        if !h.object_set.contains(&o) {
            return ret(0xBAD0_BAD0_BAD0_BAD0);
        }
        // SAFETY: `o` is one of the boxes in `objects`, alive until reset.
        ret(unsafe { (o as *const u64).read() })
    })
}

extern "sysv64" fn host_obj_set(o: u64, v: u64) -> HostRet {
    HOST.with(|h| {
        let h = h.borrow();
        if h.object_set.contains(&o) {
            // SAFETY: `o` is one of the boxes in `objects`, alive until
            // reset, and no reference to its contents is held.
            unsafe { (o as *mut u64).write(v) };
        }
        ret(0)
    })
}

extern "sysv64" fn host_throw(x: u64, _: u64) -> HostRet {
    let tag = rt::tag(Prim::I64).unwrap_or(0);
    HostRet {
        value: new_record(tag, x),
        unwind: 1,
    }
}

/// Unwinds with the value when it is a multiple of 3 (the tests' host).
extern "sysv64" fn host_check(x: u64, _: u64) -> HostRet {
    if (x as i64) % 3 == 0 {
        host_throw(x, 0)
    } else {
        ret(0)
    }
}

/// The runtime's `f64` pow on bit patterns: ir-lang's `ls_pow`.
extern "sysv64" fn host_pow_f64(x: u64, y: u64) -> HostRet {
    ret(ir_lang::ls_pow(f64::from_bits(x), f64::from_bits(y)).to_bits())
}

/// The runtime's `f32` pow: `ls_pow` of the widened operands, rounded once.
extern "sysv64" fn host_pow_f32(x: u64, y: u64) -> HostRet {
    let (x, y) = (f32::from_bits(x as u32), f32::from_bits(y as u32));
    ret(u64::from(
        (ir_lang::ls_pow(f64::from(x), f64::from(y)) as f32).to_bits(),
    ))
}

/// The host function bound to an import, if any.
fn host_binding(name: &str) -> Option<HostFn> {
    Some(match name {
        rt::EXCEPTION => host_exception,
        "trace" => host_trace,
        "gc_poll" | "__lexersketch_gc_poll" => host_nop,
        "obj_new" => host_obj_new,
        "obj_get" => host_obj_get,
        "obj_set" => host_obj_set,
        "host_throw" => host_throw,
        "host_check" => host_check,
        "__lexersketch_pow_f64" => host_pow_f64,
        "__lexersketch_pow_f32" => host_pow_f32,
        _ => return None,
    })
}

// ----------------------------------------------------------------- loader

/// A compiled module loaded into executable memory.
pub struct Native {
    base: *mut u8,
    len: usize,
    symbols: Vec<(String, u64)>,
    thunk: u64,
    cells: u64,
    /// Read by the unwinder through the cell block; boxed so its address is
    /// stable.
    _table: Box<UnwindTable>,
}

fn r(n: u8) -> Reg {
    Reg::new(Size::Qword, n).unwrap()
}

fn r32(n: u8) -> Reg {
    Reg::new(Size::Dword, n).unwrap()
}

fn cell(n: usize) -> Mem {
    Mem::new(Size::Qword, r(11)).with_disp((8 * n) as i32)
}

fn e(asm: &mut Assembler, m: M, ops: &[O]) {
    asm.emit(&Inst::from_slice(m, ops).unwrap()).unwrap();
}

impl Native {
    /// Loads `c`: [code | stubs] (made read-execute) then [data | cells]
    /// (read-write). Calls through the thunk use `conv`; every import must
    /// be the trap handler, `__lexersketch_resume`, or a known host
    /// function.
    pub fn load(c: &Compiled, conv: Conv) -> Result<Native, String> {
        let code_len = c.code().len().div_ceil(16) * 16;
        let stub_len = 2048 + 96 * c.imports().len();
        let text = (code_len + stub_len).div_ceil(PAGE) * PAGE;
        let data_len: usize = c
            .data()
            .iter()
            .map(|d| d.size() as usize + d.align() as usize)
            .sum::<usize>()
            + 8 * CELLS
            + 64;
        let len = text + data_len.div_ceil(PAGE) * PAGE;
        // SAFETY: len is nonzero.
        let base = unsafe { os::alloc(len) };
        if base.is_null() {
            return Err("allocation failed".into());
        }
        let addr = base as u64;
        let mut image = vec![0u8; len];
        image[..c.code().len()].copy_from_slice(c.code());
        let mut symbols: Vec<(String, u64)> = Vec::new();
        for f in c.functions() {
            symbols.push((f.name().to_string(), addr + f.offset() as u64));
        }
        let mut at = text;
        let mut placed = Vec::new();
        for d in c.data() {
            let a = d.align().max(1) as usize;
            at = at.div_ceil(a) * a;
            if let Some(b) = d.bytes() {
                image[at..at + b.len()].copy_from_slice(b);
            }
            symbols.push((d.name().to_string(), addr + at as u64));
            placed.push(at);
            at += d.size() as usize;
        }
        at = at.div_ceil(8) * 8;
        let cells = addr + at as u64;

        // The stubs, assembled for their final address.
        let stubs = code_len;
        let mut hosts = Vec::new();
        for name in c.imports() {
            match name.as_str() {
                "__lexersketch_trap" | "__lexersketch_resume" => {}
                other => match host_binding(other) {
                    Some(f) => hosts.push((name.clone(), f)),
                    None => return Err(format!("no native binding for import `{name}`")),
                },
            }
        }
        let st = stubs_for(conv, cells, &hosts);
        if st.code.len() > stub_len {
            return Err("stubs too large".into());
        }
        image[stubs..stubs + st.code.len()].copy_from_slice(&st.code);
        let at_stub = |o: usize| addr + (stubs + o) as u64;
        for name in c.imports() {
            let a = match name.as_str() {
                "__lexersketch_trap" => at_stub(st.trap),
                "__lexersketch_resume" => at_stub(st.resume),
                other => {
                    let k = hosts.iter().position(|(n, _)| n == other).unwrap();
                    at_stub(st.veneers[k])
                }
            };
            symbols.push((name.clone(), a));
        }
        let find = |s: &str| -> Result<u64, String> {
            symbols
                .iter()
                .find(|(n, _)| n == s)
                .map(|&(_, a)| a)
                .ok_or_else(|| format!("unresolved `{s}`"))
        };
        for rel in c.relocations() {
            let s = find(&rel.symbol)?;
            let p = addr + rel.offset as u64;
            match rel.kind {
                RelocKind::Call32 | RelocKind::Pc32 => {
                    let v =
                        i32::try_from((s as i64).wrapping_add(rel.addend).wrapping_sub(p as i64))
                            .map_err(|_| "out of reach".to_string())?;
                    image[rel.offset..rel.offset + 4].copy_from_slice(&v.to_le_bytes());
                }
                RelocKind::Abs64 => {
                    let v = (s as i64).wrapping_add(rel.addend) as u64;
                    image[rel.offset..rel.offset + 8].copy_from_slice(&v.to_le_bytes());
                }
                other => return Err(format!("relocation kind {other:?}")),
            }
        }
        for (d, &o) in c.data().iter().zip(&placed) {
            for rel in d.relocations() {
                let v = (find(&rel.symbol)? as i64).wrapping_add(rel.addend) as u64;
                image[o + rel.offset..o + rel.offset + 8].copy_from_slice(&v.to_le_bytes());
            }
        }

        let table = Box::new(UnwindTable {
            base: addr,
            thunk_ret: at_stub(st.thunk_ret),
            frames: c
                .functions()
                .iter()
                .map(|f| FrameInfo {
                    start: f.offset(),
                    end: f.offset() + f.size(),
                    cfa: u64::from(f.frame().cfa_offset()),
                    frame_pointer: f.frame().frame_pointer(),
                    pushed: f.frame().pushed().iter().map(|p| p.index()).collect(),
                    pads: f
                        .call_sites()
                        .iter()
                        .filter_map(|s| s.landing_pad().map(|p| (s.return_offset(), p)))
                        .collect(),
                })
                .collect(),
        });
        let cell0 = at;
        let table_ptr = &*table as *const UnwindTable as u64;
        image[cell0 + 8 * CELL_TABLE..cell0 + 8 * CELL_TABLE + 8]
            .copy_from_slice(&table_ptr.to_le_bytes());

        // SAFETY: `base` is a fresh read-write allocation of `len` bytes and
        // `image` is exactly `len` bytes; then the first `text` bytes (whole
        // pages) become read-execute and the rest stays read-write.
        unsafe {
            std::ptr::copy_nonoverlapping(image.as_ptr(), base, len);
            if !os::protect_exec(base, text) {
                os::free(base, len);
                return Err("protect failed".into());
            }
        }
        Ok(Native {
            base,
            len,
            symbols,
            thunk: at_stub(st.thunk),
            cells,
            _table: table,
        })
    }

    /// Calls `func` with `args` (register images, upper bits included).
    pub fn call(&self, func: &str, args: &[u64]) -> Result<Ran, String> {
        let target = self
            .symbols
            .iter()
            .find(|(n, _)| n == func)
            .map(|&(_, a)| a)
            .ok_or_else(|| format!("no function `{func}`"))?;
        if args.len() > MAX_ARGS {
            return Err("too many arguments".into());
        }
        let mut a = [0u64; MAX_ARGS + 2];
        a[0] = target;
        a[1] = args.len() as u64;
        a[2..2 + args.len()].copy_from_slice(args);
        let mut out = [0u64; 1];
        // SAFETY: `thunk` is the entry of code assembled by `stubs_for` in
        // this allocation, which stays executable for `self`'s life; it
        // follows the SysV convention, reads `a`, writes `out`, and restores
        // every SysV callee-saved register and the stack pointer, also when
        // the compiled code traps or an exception leaves it (both return
        // through the thunk's exit with its saved stack pointer).
        let status = unsafe {
            let f: extern "sysv64" fn(*const u64, *mut u64) -> u64 =
                std::mem::transmute(self.thunk as usize);
            f(a.as_ptr(), out.as_mut_ptr())
        };
        // SAFETY: the cells are inside the read-write data pages.
        let code = unsafe { ((self.cells + 8 * CELL_CODE as u64) as *const u64).read_volatile() };
        match status {
            STATUS_RETURNED => Ok(Ran::Returned(out[0])),
            STATUS_TRAP => Ok(Ran::Trap(code & 0xFFFF_FFFF)),
            STATUS_UNWOUND => Ok(Ran::Unwound(code)),
            _ => Err("the unwinder met a frame of no compiled function".into()),
        }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: `base` and `len` are this allocation, no longer used.
        unsafe { os::free(self.base, self.len) };
    }
}

struct Stubs {
    code: Vec<u8>,
    thunk: usize,
    thunk_ret: usize,
    trap: usize,
    resume: usize,
    veneers: Vec<usize>,
}

/// The entry thunk `(a: *const u64, out: *mut u64) -> status` (SysV) where
/// `a = [target, n, args...]`, calling the target under `conv`; the trap
/// handler; the resume stub (the unwinder's entry); and a veneer per host
/// function.
fn stubs_for(conv: Conv, cells: u64, hosts: &[(String, HostFn)]) -> Stubs {
    let mut asm = Assembler::new();
    let entry = asm.label();
    let after_call = asm.label();
    let exit = asm.label();
    let trap = asm.label();
    let resume = asm.label();
    let out_of_entry = asm.label();
    let cells_imm = O::Imm(cells as i64);

    // ---- the thunk
    asm.bind(entry).unwrap();
    for n in [3u8, 5, 12, 13, 14, 15] {
        e(&mut asm, M::Push, &[O::Reg(r(n))]);
    }
    e(&mut asm, M::Mov, &[O::Reg(r(11)), cells_imm]);
    e(
        &mut asm,
        M::Mov,
        &[cell(CELL_THUNK_RSP).into(), O::Reg(r(4))],
    );
    e(&mut asm, M::Mov, &[cell(CELL_OUT).into(), O::Reg(r(6))]);
    // Shadow space (Win64) plus up to MAX_ARGS stack arguments, keeping rsp
    // 16-aligned at the call (entry is 8 off; six pushes keep it 8 off).
    let regs = conv.arg_regs();
    let shadow: i64 = if conv == Conv::Win64 { 32 } else { 0 };
    let space = shadow + 8 * MAX_ARGS as i64 + 8;
    e(&mut asm, M::Sub, &[O::Reg(r(4)), O::Imm(space)]);
    e(&mut asm, M::Mov, &[O::Reg(r(10)), O::Reg(r(7))]);
    for k in 0..MAX_ARGS - regs.len() {
        let src = Mem::new(Size::Qword, r(10)).with_disp((8 * (2 + regs.len() + k)) as i32);
        e(&mut asm, M::Mov, &[O::Reg(r(11)), src.into()]);
        let dst = Mem::new(Size::Qword, r(4)).with_disp((shadow + 8 * k as i64) as i32);
        e(&mut asm, M::Mov, &[dst.into(), O::Reg(r(11))]);
    }
    for (k, &reg) in regs.iter().enumerate() {
        let src = Mem::new(Size::Qword, r(10)).with_disp((8 * (2 + k)) as i32);
        e(&mut asm, M::Mov, &[O::Reg(r(reg)), src.into()]);
    }
    e(
        &mut asm,
        M::Mov,
        &[O::Reg(r(11)), Mem::new(Size::Qword, r(10)).into()],
    );
    e(&mut asm, M::Call, &[O::Reg(r(11))]);
    asm.bind(after_call).unwrap();
    // out[0] = rax; status 0.
    e(&mut asm, M::Mov, &[O::Reg(r(11)), cells_imm]);
    e(&mut asm, M::Mov, &[O::Reg(r(11)), cell(CELL_OUT).into()]);
    e(
        &mut asm,
        M::Mov,
        &[Mem::new(Size::Qword, r(11)).into(), O::Reg(r(0))],
    );
    e(&mut asm, M::Xor, &[O::Reg(r32(0)), O::Reg(r32(0))]);
    // The exit (status in eax): the thunk's stack pointer and registers.
    asm.bind(exit).unwrap();
    e(&mut asm, M::Mov, &[O::Reg(r(11)), cells_imm]);
    e(
        &mut asm,
        M::Mov,
        &[O::Reg(r(4)), cell(CELL_THUNK_RSP).into()],
    );
    for n in [15u8, 14, 13, 12, 5, 3] {
        e(&mut asm, M::Pop, &[O::Reg(r(n))]);
    }
    e(&mut asm, M::Ret, &[]);

    // ---- the trap handler: the code (first argument), status 1.
    asm.bind(trap).unwrap();
    e(&mut asm, M::Mov, &[O::Reg(r(11)), cells_imm]);
    e(
        &mut asm,
        M::Mov,
        &[cell(CELL_CODE).into(), O::Reg(r(regs[0]))],
    );
    e(
        &mut asm,
        M::Mov,
        &[O::Reg(r32(0)), O::Imm(STATUS_TRAP as i64)],
    );
    asm.jmp(exit).unwrap();

    // ---- the resume stub: save the state, walk, enter the pad.
    asm.bind(resume).unwrap();
    e(&mut asm, M::Mov, &[O::Reg(r(11)), cells_imm]);
    e(&mut asm, M::Mov, &[cell(CELL_RSP).into(), O::Reg(r(4))]);
    e(
        &mut asm,
        M::Mov,
        &[cell(CELL_PAYLOAD).into(), O::Reg(r(regs[0]))],
    );
    for n in SAVED {
        e(
            &mut asm,
            M::Mov,
            &[cell(CELL_REGS + n as usize).into(), O::Reg(r(n))],
        );
    }
    // The walk runs below the resuming frame (never returned to), on a
    // 16-aligned stack.
    e(&mut asm, M::And, &[O::Reg(r(4)), O::Imm(-16)]);
    e(&mut asm, M::Mov, &[O::Reg(r(7)), O::Reg(r(11))]);
    let plan = unwind_plan as extern "sysv64" fn(*mut u64) -> u64 as usize as i64;
    e(&mut asm, M::Mov, &[O::Reg(r(0)), O::Imm(plan)]);
    e(&mut asm, M::Call, &[O::Reg(r(0))]);
    e(&mut asm, M::Mov, &[O::Reg(r(11)), cells_imm]);
    e(&mut asm, M::Test, &[O::Reg(r(0)), O::Reg(r(0))]);
    asm.jcc(Cond::Ne, out_of_entry).unwrap();
    for n in SAVED {
        e(
            &mut asm,
            M::Mov,
            &[O::Reg(r(n)), cell(CELL_REGS + n as usize).into()],
        );
    }
    e(&mut asm, M::Mov, &[O::Reg(r(4)), cell(CELL_RSP).into()]);
    e(&mut asm, M::Mov, &[O::Reg(r(0)), cell(CELL_PAYLOAD).into()]);
    e(&mut asm, M::Mov, &[O::Reg(r(11)), cell(CELL_TARGET).into()]);
    e(&mut asm, M::Jmp, &[O::Reg(r(11))]);
    // Out of the entry (status 2) or lost (status 3): the payload to the
    // code cell, back through the thunk's exit.
    asm.bind(out_of_entry).unwrap();
    e(&mut asm, M::Mov, &[O::Reg(r(1)), cell(CELL_PAYLOAD).into()]);
    e(&mut asm, M::Mov, &[cell(CELL_CODE).into(), O::Reg(r(1))]);
    asm.jmp(exit).unwrap();

    // ---- host veneers: the SysV call into Rust, and the unwind path.
    let mut labels = Vec::new();
    for (_, f) in hosts {
        let l = asm.label();
        let unwinds = asm.label();
        asm.bind(l).unwrap();
        labels.push(l);
        // rdi and rsi are callee-saved under Win64 and the arguments under
        // SysV; the two pushes and the 8 keep rsp 16-aligned at the call.
        e(&mut asm, M::Push, &[O::Reg(r(7))]);
        e(&mut asm, M::Push, &[O::Reg(r(6))]);
        e(&mut asm, M::Sub, &[O::Reg(r(4)), O::Imm(8)]);
        if conv == Conv::Win64 {
            e(&mut asm, M::Mov, &[O::Reg(r(7)), O::Reg(r(1))]);
            e(&mut asm, M::Mov, &[O::Reg(r(6)), O::Reg(r(2))]);
        }
        let fp = *f as usize as i64;
        e(&mut asm, M::Mov, &[O::Reg(r(0)), O::Imm(fp)]);
        e(&mut asm, M::Call, &[O::Reg(r(0))]);
        e(&mut asm, M::Add, &[O::Reg(r(4)), O::Imm(8)]);
        e(&mut asm, M::Pop, &[O::Reg(r(6))]);
        e(&mut asm, M::Pop, &[O::Reg(r(7))]);
        e(&mut asm, M::Test, &[O::Reg(r(2)), O::Reg(r(2))]);
        asm.jcc(Cond::Ne, unwinds).unwrap();
        e(&mut asm, M::Ret, &[]);
        // A throwing host function: resume from its caller's call site, as
        // if the call itself had unwound.
        asm.bind(unwinds).unwrap();
        e(&mut asm, M::Mov, &[O::Reg(r(regs[0])), O::Reg(r(0))]);
        asm.jmp(resume).unwrap();
    }

    let code = asm.finish().unwrap();
    let at = |l| code.label_offset(l).unwrap();
    Stubs {
        thunk: at(entry),
        thunk_ret: at(after_call),
        trap: at(trap),
        resume: at(resume),
        veneers: labels.iter().map(|&l| at(l)).collect(),
        code: code.into_bytes(),
    }
}
