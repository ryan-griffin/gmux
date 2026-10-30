//! Measure the production GDK layout lookup and device lock queries.
use gmux_protocol::transport::KeyLayout;
use gtk::{gdk, glib, prelude::*};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell as ValueCell, RefCell},
    hint::black_box,
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};
#[path = "../src/terminal/keyboard.rs"]
mod keyboard;
struct CountAllocations;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for CountAllocations {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountAllocations = CountAllocations;
fn main() {
    gtk::init().unwrap();
    let display = gdk::Display::default().unwrap();
    let device = display.default_seat().unwrap().keyboard().unwrap();
    let native = keyboard::NativeKeyboard::new(display.clone());
    let iterations = 1_000_000;
    // Also keep the event adapter compiled in this standalone benchmark.
    black_box(keyboard::NativeKeyboard::layout);
    black_box(keyboard::mods(gdk::ModifierType::SHIFT_MASK));
    black_box(native.lookup(38, 0, 1));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let started = Instant::now();
    let mut cpu = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut cpu);
    }
    let before = cpu.tv_sec as f64 + cpu.tv_nsec as f64 / 1e9;
    for _ in 0..iterations {
        black_box(native.lookup(black_box(38), black_box(0), black_box(1)));
        black_box(native.locks(&device));
    }
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut cpu);
    }
    let cpu_ns =
        ((cpu.tv_sec as f64 + cpu.tv_nsec as f64 / 1e9) - before) * 1e9 / iterations as f64;
    println!(
        "{{\"backend\":\"{:?}\",\"iterations\":{},\"cpu_ns_per_key\":{:.1},\"wall_ns_per_key\":{:.1},\"rust_allocations\":{}}}",
        display.backend(),
        iterations,
        cpu_ns,
        started.elapsed().as_nanos() as f64 / iterations as f64,
        ALLOCATIONS.load(Ordering::Relaxed) - allocations
    );
}
