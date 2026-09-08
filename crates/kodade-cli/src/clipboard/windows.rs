//! Transfer Unicode text directly, without a console code page or a BOM.

use anyhow::{Context, Result};
use std::{
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{GlobalFree, HGLOBAL, HWND},
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
        Ole::CF_UNICODETEXT,
    },
    UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE},
};

pub async fn copy(text: &str, timeout: Duration) -> Result<()> {
    anyhow::ensure!(
        !text.contains('\0'),
        "clipboard text contains a null character"
    );
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let canceled = Arc::new(AtomicBool::new(false));
    let _cancel = Cancel(canceled.clone());
    // Window creation, use and destruction must stay on the same OS thread.
    tokio::task::spawn_blocking(move || copy_blocking(&text, timeout, &canceled))
        .await
        .context("Windows clipboard worker")?
}

struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn copy_blocking(text: &[u16], timeout: Duration, canceled: &AtomicBool) -> Result<()> {
    // A message-only window supplies a real owner even under ConPTY, where
    // there may be no console HWND. STATIC is a built-in window class.
    let window = Window(unsafe {
        CreateWindowExW(
            0,
            windows_sys::core::w!("STATIC"),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            null_mut(),
            null(),
        )
    });
    anyhow::ensure!(
        !window.0.is_null(),
        "create clipboard owner: {}",
        std::io::Error::last_os_error()
    );
    let mut memory = Memory(unsafe { GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of_val(text)) });
    anyhow::ensure!(!memory.0.is_null(), "allocate clipboard text");
    let destination = unsafe { GlobalLock(memory.0) }.cast::<u16>();
    anyhow::ensure!(!destination.is_null(), "lock clipboard text");
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), destination, text.len());
        GlobalUnlock(memory.0);
    }
    let deadline = Instant::now() + timeout;
    loop {
        anyhow::ensure!(
            !canceled.load(Ordering::Acquire),
            "clipboard write canceled"
        );
        if unsafe { OpenClipboard(window.0) } != 0 {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "Windows clipboard is busy");
        std::thread::sleep(Duration::from_millis(10));
    }
    let _clipboard = OpenedClipboard;
    anyhow::ensure!(
        !canceled.load(Ordering::Acquire),
        "clipboard write canceled"
    );
    anyhow::ensure!(unsafe { EmptyClipboard() } != 0, "empty Windows clipboard");
    anyhow::ensure!(
        !unsafe { SetClipboardData(CF_UNICODETEXT.into(), memory.0) }.is_null(),
        "write Windows clipboard: {}",
        std::io::Error::last_os_error()
    );
    // The clipboard owns this allocation after a successful transfer.
    memory.0 = null_mut();
    Ok(())
}

struct Window(HWND);
impl Drop for Window {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DestroyWindow(self.0) };
        }
    }
}

struct Memory(HGLOBAL);
impl Drop for Memory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { GlobalFree(self.0) };
        }
    }
}

struct OpenedClipboard;
impl Drop for OpenedClipboard {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}
