use errno::{set_errno, Errno};
use nix::sys::signal::{self, Signal};
use std::{
    cell::RefCell,
    ptr,
    sync::Mutex,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use copypasta::osx_clipboard::OSXClipboardContext;
#[cfg(target_os = "windows")]
use copypasta::windows_clipboard::WindowsClipboardContext;
use copypasta::ClipboardProvider;
#[cfg(all(unix, not(target_os = "macos")))]
use copypasta::{
    wayland_clipboard::create_clipboards_from_external,
    x11_clipboard::{Clipboard, X11ClipboardContext},
};

use libc::{c_void, fd_set, pselect, sigset_t, timespec};
use once_cell::sync::Lazy;
#[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
use winit::platform::wayland::EventLoopWindowTargetExtWayland;
use winit::{
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
    monitor::MonitorHandle,
    platform::run_return::EventLoopExtRunReturn,
    window::Window,
    window::WindowId,
};

use surfman::Connection;
use surfman::SurfaceType;
use webrender_surfman::WebrenderSurfman;

use emacs::bindings::{inhibit_window_system, make_timespec, thread_select};

pub type GUIEvent = Event<'static, i32>;

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum Platform {
    X11,
    Wayland(*mut c_void),
    MacOS,
    Windows,
}

unsafe impl Send for Platform {}

pub struct WrEventLoop {
    clipboard: Box<dyn ClipboardProvider>,
    el: EventLoop<i32>,
    pub connection: Option<Connection>,
}

unsafe impl Send for WrEventLoop {}
unsafe impl Sync for WrEventLoop {}

impl WrEventLoop {
    pub fn el(&self) -> &EventLoop<i32> {
        &self.el
    }

    pub fn connection(&mut self) -> &Connection {
        if self.connection.is_none() {
            self.open_native_display();
        }
        self.connection.as_ref().unwrap()
    }

    pub fn create_proxy(&self) -> EventLoopProxy<i32> {
        self.el.create_proxy()
    }

    pub fn new_webrender_surfman(&mut self, window: &Window) -> WebrenderSurfman {
        let connection = self.connection();
        let adapter = connection
            .create_adapter()
            .expect("Failed to create adapter");
        let native_widget = connection
            .create_native_widget_from_winit_window(&window)
            .expect("Failed to create native widget");
        let surface_type = SurfaceType::Widget { native_widget };
        let webrender_surfman = WebrenderSurfman::create(&connection, &adapter, surface_type)
            .expect("Failed to create WR surfman");

        webrender_surfman
    }

    pub fn open_native_display(&mut self) -> &Option<Connection> {
        let window_builder = winit::window::WindowBuilder::new().with_visible(false);
        let window = window_builder.build(&self.el).unwrap();

        // Initialize surfman
        let connection =
            Connection::from_winit_window(&window).expect("Failed to create connection");

        self.connection = Some(connection);

        &self.connection
    }

    pub fn wait_for_window_resize(&mut self, target_window_id: WindowId) {
        let deadline = Instant::now() + Duration::from_millis(100);
        self.el.run_return(|e, _, control_flow| match e {
            Event::NewEvents(StartCause::Init) => {
                *control_flow = ControlFlow::WaitUntil(deadline);
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                *control_flow = ControlFlow::Exit;
            }

            Event::WindowEvent {
                event: WindowEvent::Resized(_),
                window_id,
            } => {
                if target_window_id == window_id {
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        });
    }

    pub fn get_available_monitors(&self) -> impl Iterator<Item = MonitorHandle> {
        self.el.available_monitors()
    }

    pub fn get_primary_monitor(&self) -> MonitorHandle {
        self.el
            .primary_monitor()
            .unwrap_or_else(|| -> MonitorHandle { self.get_available_monitors().next().unwrap() })
    }

    pub fn get_clipboard(&mut self) -> &mut Box<dyn ClipboardProvider> {
        &mut self.clipboard
    }
}

fn build_clipboard(_event_loop: &EventLoop<i32>) -> Box<dyn ClipboardProvider> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if _event_loop.is_wayland() {
            let wayland_display = _event_loop
                .wayland_display()
                .expect("Fetch Wayland display failed");
            let (_, clipboard) = unsafe { create_clipboards_from_external(wayland_display) };
            Box::new(clipboard)
        } else {
            Box::new(X11ClipboardContext::<Clipboard>::new().unwrap())
        }
    }
    #[cfg(target_os = "windows")]
    {
        return Box::new(WindowsClipboardContext::new().unwrap());
    }
    #[cfg(target_os = "macos")]
    {
        return Box::new(OSXClipboardContext::new().unwrap());
    }
}

pub static EVENT_LOOP: Lazy<Mutex<WrEventLoop>> = Lazy::new(|| {
    let el = winit::event_loop::EventLoopBuilder::<i32>::with_user_event().build();
    let clipboard = build_clipboard(&el);
    let connection = None;

    Mutex::new(WrEventLoop {
        clipboard,
        el,
        connection,
    })
});

pub static EVENT_BUFFER: Lazy<Mutex<Vec<GUIEvent>>> = Lazy::new(|| Mutex::new(Vec::new()));

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FdSet(pub *mut fd_set);

unsafe impl Send for FdSet {}
unsafe impl Sync for FdSet {}

impl FdSet {
    fn clear(&self) {
        if self.0 != ptr::null_mut() {
            unsafe { libc::FD_ZERO(self.0) };
        }
    }
}

impl Drop for FdSet {
    fn drop(&mut self) {
        self.clear()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Timespec(pub *mut timespec);

unsafe impl Send for Timespec {}
unsafe impl Sync for Timespec {}

#[no_mangle]
pub extern "C" fn wr_select1(
    nfds: i32,
    readfds: *mut fd_set,
    writefds: *mut fd_set,
    _exceptfds: *mut fd_set,
    timeout: *mut timespec,
    _sigmask: *mut sigset_t,
) -> i32 {
    if unsafe { inhibit_window_system } {
        return unsafe {
            thread_select(
                Some(pselect),
                nfds,
                readfds,
                writefds,
                _exceptfds,
                timeout,
                _sigmask,
            )
        };
    }

    let mut event_loop = EVENT_LOOP.lock().unwrap();

    let deadline = Instant::now()
        + unsafe { Duration::new((*timeout).tv_sec as u64, (*timeout).tv_nsec as u32) };

    let nfds_result = RefCell::new(0);

    // We mush run winit in main thread, because the macOS platfrom limitation.
    event_loop.el.run_return(|e, _, control_flow| {
        control_flow.set_wait_until(deadline);

        match e {
            Event::WindowEvent { ref event, .. } => match event {
                WindowEvent::Resized(_)
                | WindowEvent::KeyboardInput { .. }
                | WindowEvent::ReceivedCharacter(_)
                | WindowEvent::ModifiersChanged(_)
                | WindowEvent::MouseInput { .. }
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::Focused(_)
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::CloseRequested => {
                    EVENT_BUFFER.lock().unwrap().push(e.to_static().unwrap());

                    // notify emacs's code that a keyboard event arrived.
                    match signal::raise(Signal::SIGIO) {
                        Ok(_) => {}
                        Err(err) => log::error!("sigio err: {err:?}"),
                    };
                    /* Pretend that `select' is interrupted by a signal.  */
                    set_errno(Errno(libc::EINTR));
                    debug_assert_eq!(nix::errno::errno(), libc::EINTR);
                    nfds_result.replace(-1);
                    control_flow.set_exit();
                }
                _ => {}
            },
            Event::UserEvent(nfds) => {
                nfds_result.replace(nfds);
                control_flow.set_exit();
            }
            Event::RedrawEventsCleared => {
                control_flow.set_exit();
            }
            _ => {}
        };
    });
    let ret = nfds_result.into_inner();
    if ret == 0 {
        let timespec = unsafe { make_timespec(0, 0) };
        // Add some delay here avoding high cpu usage on macOS
        #[cfg(target_os = "macos")]
        spin_sleep::sleep(Duration::from_millis(16));
        let nfds =
            unsafe { libc::pselect(nfds, readfds, writefds, _exceptfds, &timespec, _sigmask) };
        log::trace!("pselect: {nfds:?}");
        return nfds;
    }

    log::trace!("winit event run_return: {ret:?}");

    ret
}
