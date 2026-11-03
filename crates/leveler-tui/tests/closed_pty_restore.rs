//! Closing the PTY master makes stdout return EIO. Terminal restore must
//! return rather than panic.
//!
//! This sits outside the library crate: that crate forbids unsafe, and
//! `dup2` is how a real closed PTY is put on stdout. Re-executing the
//! library test harness with stdout already closed fails inside libtest
//! before the restore call.

use std::panic::AssertUnwindSafe;

#[test]
fn restore_does_not_panic_when_the_pty_is_already_closed() {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "openpty");

    let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
    assert!(saved >= 0, "dup stdout");
    struct RestoreStdout(libc::c_int);
    impl Drop for RestoreStdout {
        fn drop(&mut self) {
            unsafe {
                libc::dup2(self.0, libc::STDOUT_FILENO);
                libc::close(self.0);
            }
        }
    }
    let saved = RestoreStdout(saved);

    unsafe {
        libc::close(master);
        assert_eq!(libc::dup2(slave, libc::STDOUT_FILENO), libc::STDOUT_FILENO);
        libc::close(slave);
    }

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        leveler_tui::terminal::restore_terminal();
    }));
    drop(saved);
    assert!(result.is_ok(), "restore panicked on a closed pty");
}
