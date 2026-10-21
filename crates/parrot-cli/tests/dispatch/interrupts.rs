use parrot_cli::dispatch::interrupts::{CliInterruptListener, Interrupts};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn no_listener_and_unclaimed_interrupt_stop_the_application() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let count = stopped.clone();
    let interrupts = Interrupts::new(Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
    }));
    interrupts.signal();
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    interrupts.install(Arc::new(CliInterruptListener(|| false)));
    interrupts.signal();
    assert_eq!(stopped.load(Ordering::SeqCst), 2);
}

#[test]
fn busy_listener_claims_first_interrupt_only_and_removal_restores_process_stop() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let count = stopped.clone();
    let interrupts = Interrupts::new(Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
    }));
    let offered = Arc::new(AtomicUsize::new(0));
    let offers = offered.clone();
    interrupts.install(Arc::new(CliInterruptListener(move || {
        offers.fetch_add(1, Ordering::SeqCst) == 0
    })));
    interrupts.signal();
    assert_eq!(offered.load(Ordering::SeqCst), 1);
    assert_eq!(stopped.load(Ordering::SeqCst), 0);
    interrupts.signal();
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    interrupts.remove();
    interrupts.signal();
    assert_eq!(stopped.load(Ordering::SeqCst), 2);
}

#[test]
fn sigterm_semantics_bypass_the_session_listener() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let count = stopped.clone();
    let interrupts = Interrupts::new(Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
    }));
    interrupts.install(Arc::new(CliInterruptListener(|| {
        panic!("SIGTERM must not be offered")
    })));
    interrupts.terminate();
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
}

#[test]
fn installing_replaces_the_single_listener_and_callback_can_remove_itself() {
    let interrupts = Arc::new(Interrupts::new(Arc::new(|| {})));
    interrupts.install(Arc::new(CliInterruptListener(|| {
        panic!("replaced listener")
    })));
    let owner = Arc::downgrade(&interrupts);
    interrupts.install(Arc::new(CliInterruptListener(move || {
        owner.upgrade().unwrap().remove();
        true
    })));
    interrupts.signal();
    interrupts.signal();
}
