extern crate block;

use block::ConcreteBlock;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct Capture(Arc<AtomicUsize>);

impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn stack_and_copied_blocks_preserve_calls_and_drop_the_capture_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture(Arc::clone(&drops));
    let stack = ConcreteBlock::new(move |value: i32| {
        let _capture = &capture;
        value + 7
    });
    assert_eq!(unsafe { stack.call((5,)) }, 12);

    let copied = stack.copy();
    let retained = copied.clone();
    drop(copied);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(unsafe { retained.call((6,)) }, 13);
    drop(retained);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
