//! The app's place in the system: starting at login, one instance, the Dock, the clipboard.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The single-instance lock: a loopback port. A port is what every platform can do
/// without a native lock; another program on it means no lock, and the app runs anyway.
/// Each app needs its own port, or two of them would take each other for themselves.
#[derive(Clone, Copy, Debug)]
pub struct InstanceConfig {
    pub port: u16,
    /// What the two instances say to each other, both ways, to know it is them.
    pub hello: &'static [u8],
}

pub enum Instance {
    First(TcpListener),
    Second,
}

/// Ask for the lock, or tell the running app to show itself.
///
/// Sends nothing beyond the hello. [`claim_instance_sending`] is the same handshake
/// with the launching process's arguments after it.
pub fn claim_instance(config: &InstanceConfig) -> Option<Instance> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
    match TcpListener::bind(addr) {
        Ok(listener) => Some(Instance::First(listener)),
        Err(_) => {
            let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500))
            else {
                return None;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let _ = stream.write_all(config.hello);
            let mut answer = vec![0; config.hello.len()];
            let confirmed = stream.read_exact(&mut answer).is_ok() && answer == config.hello;
            confirmed.then_some(Instance::Second)
        }
    }
}

/// A second launch knocks here; `second` is called each time, on the listener's thread.
///
/// The bytes after the hello are not read, so this behaves as it always has whether or
/// not the other side sends arguments. [`serve_instance_with_args`] reads them.
pub fn serve_instance(
    config: &InstanceConfig,
    listener: TcpListener,
    second: impl Fn() + Send + 'static,
) {
    spawn_server(config.hello, listener, None, second);
}

/// [`serve_instance`] with a way to let go: [`InstanceServer::release`], or dropping it,
/// closes the port at once. A restart releases it before starting the new process, which
/// otherwise finds the lock still held by the one leaving, takes itself for a second
/// launch, and quits.
pub fn serve_instance_releasable(
    config: &InstanceConfig,
    listener: TcpListener,
    second: impl Fn() + Send + 'static,
) -> InstanceServer {
    let port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(config.port);
    let stop = Arc::new(AtomicBool::new(false));
    let thread = spawn_server(config.hello, listener, Some(Arc::clone(&stop)), second);
    InstanceServer { port, stop, thread }
}

/// The listener [`serve_instance_releasable`] runs; it holds the lock until released.
pub struct InstanceServer {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InstanceServer {
    /// Stop listening and free the port; returns once it is free.
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for InstanceServer {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.stop.store(true, Ordering::SeqCst);
        // The thread waits in accept: a knock of our own wakes it to see the flag.
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(500));
        let _ = thread.join();
    }
}

fn spawn_server(
    hello: &'static [u8],
    listener: TcpListener,
    stop: Option<Arc<AtomicBool>>,
    second: impl Fn() + Send + 'static,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("single-instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if stop.as_ref().is_some_and(|s| s.load(Ordering::SeqCst)) {
                    break;
                }
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut knock = vec![0; hello.len()];
                if stream.read_exact(&mut knock).is_ok() && knock == hello {
                    let _ = stream.write_all(hello);
                    second();
                }
            }
        })
        .map_err(|err| log::warn!(target: "app", "單一執行個體的監聽起不來：{err}"))
        .ok()
}

/// [`claim_instance`] that also hands `args` to the running app.
///
/// The running app receives them only from [`serve_instance_with_args`]. One that used
/// [`serve_instance`] still only hears that a second launch happened; these bytes sit
/// unread and the hello exchange is the one [`claim_instance`] does. Binding the port
/// still means this process is first, and `args` are not sent anywhere.
pub fn claim_instance_sending(config: &InstanceConfig, args: &[String]) -> Option<Instance> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
    match TcpListener::bind(addr) {
        Ok(listener) => Some(Instance::First(listener)),
        Err(_) => {
            let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500))
            else {
                return None;
            };
            let _ = stream.set_nodelay(true);
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
            if stream.write_all(config.hello).is_err() {
                return None;
            }
            // The frame follows the hello before the reply is read, so a server that
            // answers and then reads is not waiting on a write that has not happened.
            if let Err(err) = write_arg_frame(&mut stream, args) {
                log::warn!(target: "app", "第二個實例的參數送不出去：{err}");
            }
            let mut answer = vec![0; config.hello.len()];
            let confirmed = stream.read_exact(&mut answer).is_ok() && answer == config.hello;
            confirmed.then_some(Instance::Second)
        }
    }
}

/// [`serve_instance_releasable`] that also receives what [`claim_instance_sending`] sent.
///
/// `on_args` runs on the listener's thread. A peer that sends no frame
/// ([`claim_instance`]) arrives as an empty list, and so does a frame that cannot be
/// read; the launch still happened. Dropping the server, or [`InstanceServer::release`],
/// frees the port at once, as [`serve_instance_releasable`] does.
pub fn serve_instance_with_args(
    config: &InstanceConfig,
    listener: TcpListener,
    on_args: impl Fn(Vec<String>) + Send + 'static,
) -> InstanceServer {
    let port = listener
        .local_addr()
        .map(|addr| addr.port())
        .unwrap_or(config.port);
    let stop = Arc::new(AtomicBool::new(false));
    let thread = spawn_server_with_args(config.hello, listener, Arc::clone(&stop), on_args);
    InstanceServer { port, stop, thread }
}

/// After the hello, both ways: a `u32` little-endian count, then each argument as a
/// `u32` little-endian byte length and its UTF-8. Count and length stay within these,
/// so a peer on the port cannot make the listener allocate without bound.
const MAX_FORWARDED_ARGS: u32 = 64;
const MAX_FORWARDED_ARG_LEN: u32 = 8 * 1024;

fn write_arg_frame(stream: &mut TcpStream, args: &[String]) -> std::io::Result<()> {
    let count = u32::try_from(args.len()).map_err(invalid_frame)?;
    if count > MAX_FORWARDED_ARGS {
        return Err(invalid_frame("參數太多"));
    }
    stream.write_all(&count.to_le_bytes())?;
    for arg in args {
        let len = u32::try_from(arg.len()).map_err(invalid_frame)?;
        if len > MAX_FORWARDED_ARG_LEN {
            return Err(invalid_frame("參數太長"));
        }
        stream.write_all(&len.to_le_bytes())?;
        stream.write_all(arg.as_bytes())?;
    }
    Ok(())
}

fn invalid_frame(err: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
}

/// `None`: the peer closed or never sent a frame (the plain [`claim_instance`]).
/// `Some(Err)`: a frame arrived and was not usable.
fn read_arg_frame(stream: &mut TcpStream) -> Option<Result<Vec<String>, String>> {
    let mut buf = [0u8; 4];
    if let Err(err) = stream.read_exact(&mut buf) {
        return no_frame(err);
    }
    let count = u32::from_le_bytes(buf);
    if count > MAX_FORWARDED_ARGS {
        return Some(Err(format!("參數太多：{count}")));
    }
    let mut args = Vec::new();
    for _ in 0..count {
        if let Err(err) = stream.read_exact(&mut buf) {
            return Some(Err(err.to_string()));
        }
        let len = u32::from_le_bytes(buf);
        if len > MAX_FORWARDED_ARG_LEN {
            return Some(Err(format!("參數太長：{len}")));
        }
        let Ok(len) = usize::try_from(len) else {
            return Some(Err(format!("參數太長：{len}")));
        };
        let mut bytes = vec![0u8; len];
        if let Err(err) = stream.read_exact(&mut bytes) {
            return Some(Err(err.to_string()));
        }
        match String::from_utf8(bytes) {
            Ok(text) => args.push(text),
            Err(err) => return Some(Err(err.to_string())),
        }
    }
    Some(Ok(args))
}

/// A missing frame is the old handshake, not a failure. Anything else is.
fn no_frame(err: std::io::Error) -> Option<Result<Vec<String>, String>> {
    match err.kind() {
        std::io::ErrorKind::UnexpectedEof
        | std::io::ErrorKind::TimedOut
        | std::io::ErrorKind::WouldBlock
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted => None,
        _ => Some(Err(err.to_string())),
    }
}

fn spawn_server_with_args(
    hello: &'static [u8],
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    on_args: impl Fn(Vec<String>) + Send + 'static,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("single-instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let mut stream = stream;
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut knock = vec![0; hello.len()];
                if stream.read_exact(&mut knock).is_err() || knock != hello {
                    continue;
                }
                let _ = stream.write_all(hello);
                let args = match read_arg_frame(&mut stream) {
                    Some(Ok(args)) => args,
                    // The plain `claim_instance` sends no frame and then closes.
                    None => Vec::new(),
                    Some(Err(err)) => {
                        log::warn!(target: "app", "第二個實例的參數讀不到：{err}");
                        Vec::new()
                    }
                };
                on_args(args);
            }
        })
        .map_err(|err| log::warn!(target: "app", "單一執行個體的監聽起不來：{err}"))
        .ok()
}

/// The login item: under what name, and what it passes the app.
#[cfg(feature = "autostart")]
#[derive(Clone, Copy, Debug)]
pub struct AutostartConfig {
    pub name: &'static str,
    pub args: &'static [&'static str],
}

#[cfg(feature = "autostart")]
fn auto_launch(config: &AutostartConfig) -> Result<auto_launch::AutoLaunch, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let path = exe.to_string_lossy().into_owned();
    let mut builder = auto_launch::AutoLaunchBuilder::new();
    builder
        .set_app_name(config.name)
        .set_app_path(&path)
        .set_args(config.args);
    #[cfg(target_os = "macos")]
    builder.set_use_launch_agent(true);
    builder.build().map_err(|e| e.to_string())
}

#[cfg(feature = "autostart")]
pub fn set_autostart(config: &AutostartConfig, on: bool) -> Result<(), String> {
    let launch = auto_launch(config)?;
    if on {
        launch.enable().map_err(|e| e.to_string())
    } else if launch.is_enabled().unwrap_or(false) {
        launch.disable().map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

/// Keep the login item aligned with the setting. Rewritten on every launch while enabled:
/// the item records the path the app ran from. A failure must not block monitoring.
#[cfg(feature = "autostart")]
pub fn sync_autostart(config: &AutostartConfig, on: bool) {
    if let Err(err) = set_autostart(config, on) {
        log::debug!(target: "app", "開機啟動同步失敗：{err}");
    }
}

/// `osType osVersion (arch)`, for a bug report.
pub fn system_description() -> String {
    let info = os_info::get();
    format!(
        "{} {} ({})",
        std::env::consts::OS,
        info.version(),
        std::env::consts::ARCH
    )
}

/// Put text on the clipboard with what the system has for it.
pub fn copy_text(text: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(target_os = "windows")]
    let candidates: &[(&str, &[&str])] = &[("clip", &[])];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    let mut last = "沒有可用的剪貼簿工具".to_string();
    for (program, args) in candidates {
        match Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin
                        .write_all(text.as_bytes())
                        .map_err(|e| e.to_string())?;
                }
                drop(child.stdin.take());
                return child
                    .wait()
                    .map_err(|e| e.to_string())
                    .and_then(|s| s.success().then_some(()).ok_or_else(|| s.to_string()));
            }
            Err(err) => last = err.to_string(),
        }
    }
    Err(last)
}

/// macOS: the Dock icon follows the main window. Closed to the tray, it has none.
#[cfg(target_os = "macos")]
pub fn set_dock(visible: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    let app = NSApplication::sharedApplication(mtm);
    if !app.setActivationPolicy(policy) {
        log::warn!(target: "window", "Dock 圖示切換失敗");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock(_visible: bool) {}

/// macOS: bring the app forward, whatever else is in front.
#[cfg(target_os = "macos")]
pub fn activate_app() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).activate();
    }
}

#[cfg(not(target_os = "macos"))]
pub fn activate_app() {}

/// macOS: undo Cmd-H. A hidden app stays hidden whatever its windows are told.
#[cfg(target_os = "macos")]
pub fn unhide_app() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).unhide(None);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn unhide_app() {}

/// macOS: a window on every Space, also over full-screen apps. PiP's.
#[cfg(target_os = "macos")]
pub fn pin_to_all_spaces(window: &winit::window::Window) {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSView, NSWindowCollectionBehavior};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    // SAFETY: winit hands out a live NSView, on the main thread, for this window.
    let view: Option<Retained<NSView>> =
        unsafe { Retained::retain(appkit.ns_view.as_ptr().cast()) };
    if let Some(ns_window) = view.and_then(|v| v.window()) {
        ns_window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
    }
}

#[cfg(not(target_os = "macos"))]
pub fn pin_to_all_spaces(_window: &winit::window::Window) {}

/// Give freed pages back to the system. macOS's allocator keeps them for reuse, and they
/// count against the process until memory is short.
#[cfg(target_os = "macos")]
pub fn return_free_memory() {
    extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    // SAFETY: a null zone means every zone, and a goal of 0 means as much as possible.
    unsafe {
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn return_free_memory() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_launch_is_answered_only_by_its_own_hello() {
        // A port free for this test alone.
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|l| l.local_addr())
            .expect("a free port")
            .port();
        let ours = InstanceConfig {
            port,
            hello: b"trem-gfx-test",
        };
        let Some(Instance::First(listener)) = claim_instance(&ours) else {
            panic!("the first launch takes the lock");
        };
        let (knocked, heard) = std::sync::mpsc::channel();
        serve_instance(&ours, listener, move || {
            let _ = knocked.send(());
        });
        assert!(matches!(claim_instance(&ours), Some(Instance::Second)));
        heard
            .recv_timeout(Duration::from_secs(2))
            .expect("the first launch is told");
        // Another app on the same port is not taken for this one.
        let theirs = InstanceConfig {
            port,
            hello: b"someone-else!",
        };
        assert!(claim_instance(&theirs).is_none());
        assert!(heard.try_recv().is_err());
    }

    #[test]
    fn a_released_server_frees_the_port_at_once() {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|l| l.local_addr())
            .expect("a free port")
            .port();
        let ours = InstanceConfig {
            port,
            hello: b"trem-gfx-release",
        };
        let Some(Instance::First(listener)) = claim_instance(&ours) else {
            panic!("the first launch takes the lock");
        };
        let (knocked, heard) = std::sync::mpsc::channel();
        let server = serve_instance_releasable(&ours, listener, move || {
            let _ = knocked.send(());
        });
        assert!(matches!(claim_instance(&ours), Some(Instance::Second)));
        heard
            .recv_timeout(Duration::from_secs(2))
            .expect("the first launch is told");
        server.release();
        // The port is free: the next launch is the first, and nobody was told.
        assert!(matches!(claim_instance(&ours), Some(Instance::First(_))));
        assert!(heard.try_recv().is_err());
    }

    #[test]
    fn a_second_launch_hands_its_arguments_across() {
        let port = free_port();
        let ours = InstanceConfig {
            port,
            hello: b"trem-gfx-args",
        };
        let Some(Instance::First(listener)) = claim_instance(&ours) else {
            panic!("the first launch takes the lock");
        };
        let (sent, got) = std::sync::mpsc::channel();
        let server = serve_instance_with_args(&ours, listener, move |args| {
            let _ = sent.send(args);
        });
        let args = vec![
            "trem-monitor://oauth/callback?code=1&state=a%20b".to_string(),
            "--restarted".to_string(),
        ];
        assert!(matches!(
            claim_instance_sending(&ours, &args),
            Some(Instance::Second)
        ));
        assert_eq!(
            got.recv_timeout(Duration::from_secs(2))
                .expect("the arguments arrive"),
            args
        );
        // An explicit empty list is a frame of count zero, not a missing frame.
        assert!(matches!(
            claim_instance_sending(&ours, &[]),
            Some(Instance::Second)
        ));
        assert_eq!(
            got.recv_timeout(Duration::from_secs(2))
                .expect("an empty argument list"),
            Vec::<String>::new()
        );
        // No frame at all: the plain claim, which must still be heard.
        assert!(matches!(claim_instance(&ours), Some(Instance::Second)));
        assert_eq!(
            got.recv_timeout(Duration::from_secs(2))
                .expect("a launch with no arguments"),
            Vec::<String>::new()
        );
        // Not our hello: nothing is delivered.
        let theirs = InstanceConfig {
            port,
            hello: b"someone-else!",
        };
        assert!(claim_instance_sending(&theirs, &args).is_none());
        assert!(got.try_recv().is_err());
        server.release();
    }

    #[test]
    fn arguments_do_not_change_the_old_handshake() {
        let port = free_port();
        let ours = InstanceConfig {
            port,
            hello: b"trem-gfx-old",
        };
        let Some(Instance::First(listener)) = claim_instance(&ours) else {
            panic!("the first launch takes the lock");
        };
        let (knocked, heard) = std::sync::mpsc::channel();
        serve_instance(&ours, listener, move || {
            let _ = knocked.send(());
        });
        let args = vec!["trem-monitor://oauth/callback?code=1".to_string()];
        assert!(matches!(
            claim_instance_sending(&ours, &args),
            Some(Instance::Second)
        ));
        heard
            .recv_timeout(Duration::from_secs(2))
            .expect("the first launch is told");
        assert!(heard.try_recv().is_err());
    }

    fn free_port() -> u16 {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .expect("a free port")
            .port()
    }
}
