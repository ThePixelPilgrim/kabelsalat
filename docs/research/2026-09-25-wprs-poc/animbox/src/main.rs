//! animbox: a dependency-free (pure-Rust wayland-client, wl_shm) xdg-shell test client.
//!
//! Modes (read from $ANIMBOX_MODE_FILE, default /tmp/animbox-mode, once per second):
//!   idle   - static content, redraws only on input
//!   plasma - full-window smooth animation every frame (compressible)
//!   noise  - 400x400 incompressible random patch every frame, rest static
//!   scroll - full-window text-like pattern scrolling 8 px per frame
//! A 120x120 marker at the top-left flips red<->green on every key press / button press,
//! so input-to-screen latency can be measured from outside. Every input is logged to
//! stdout as `INPUT <unix_ms> n=<count>`, and once per second `STAT <unix_ms> fps=<n> mode=<m>`.
use std::os::fd::AsFd;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const NBUF: usize = 3;

struct Buf {
    buffer: wl_buffer::WlBuffer,
    busy: bool,
}

struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    surface: Option<wl_surface::WlSurface>,
    width: i32,
    height: i32,
    pending_size: Option<(i32, i32)>,
    configured: bool,
    closed: bool,
    bufs: Vec<Buf>,
    mmap: Option<memmap2::MmapMut>,
    frame_pending: bool,
    need_redraw: bool,
    inputs: u64,
    frame: u64,
    frames_this_sec: u64,
    mode: String,
    rng: u64,
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

impl State {
    fn alloc(&mut self, qh: &QueueHandle<Self>) {
        for b in self.bufs.drain(..) {
            b.buffer.destroy();
        }
        let (w, h) = (self.width, self.height);
        let stride = w * 4;
        let size = (stride * h) as usize;
        let name = std::ffi::CString::new("animbox").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create");
        let file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        file.set_len((size * NBUF) as u64).unwrap();
        let mmap = unsafe { memmap2::MmapMut::map_mut(&file).unwrap() };
        let pool = self
            .shm
            .as_ref()
            .unwrap()
            .create_pool(file.as_fd(), (size * NBUF) as i32, qh, ());
        for i in 0..NBUF {
            let buffer = pool.create_buffer(
                (i * size) as i32,
                w,
                h,
                stride,
                wl_shm::Format::Xrgb8888,
                qh,
                i,
            );
            self.bufs.push(Buf { buffer, busy: false });
        }
        pool.destroy();
        self.mmap = Some(mmap);
        // Every new buffer starts undefined, so the first frames must be full.
        self.need_redraw = true;
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let Some(idx) = self.bufs.iter().position(|b| !b.busy) else {
            return;
        };
        let (w, h) = (self.width as usize, self.height as usize);
        let size = w * h * 4;
        let t = self.frame;
        let mode = self.mode.clone();
        let marker = if self.inputs % 2 == 0 { 0xffd01010u32 } else { 0xff10c010u32 };
        let mut rng = self.rng;
        let mmap = self.mmap.as_mut().unwrap();
        let px: &mut [u32] = bytemuck_cast(&mut mmap[idx * size..(idx + 1) * size]);
        // Buffers rotate, so always repaint the whole buffer, but report only real damage.
        let mut damage: Vec<(i32, i32, i32, i32)> = vec![];
        match mode.as_str() {
            "plasma" => {
                for y in 0..h {
                    for x in 0..w {
                        let r = ((x as u64 + t * 4) & 255) as u32;
                        let g = ((y as u64 + t * 2) & 255) as u32;
                        let b = (((x + y) as u64 / 2 + t) & 255) as u32;
                        px[y * w + x] = 0xff000000 | (r << 16) | (g << 8) | b;
                    }
                }
                damage.push((0, 0, w as i32, h as i32));
            }
            "scroll" => {
                let off = (t * 8) as usize;
                for y in 0..h {
                    let line = (y + off) / 18;
                    let in_line = (y + off) % 18;
                    for x in 0..w {
                        let col = x / 9;
                        let glyph = (line.wrapping_mul(2654435761) ^ col.wrapping_mul(40503)) % 7;
                        let ink = in_line > 3 && in_line < 15 && x % 9 < 7 && glyph > 1
                            && (x + in_line) % 3 != 0 && col % 40 < 34;
                        px[y * w + x] = if ink { 0xff202020 } else { 0xfff4f4f0 };
                    }
                }
                damage.push((0, 0, w as i32, h as i32));
            }
            _ => {
                // idle / noise background: static gradient
                for y in 0..h {
                    for x in 0..w {
                        let v = (x * 255 / w.max(1)) as u32;
                        px[y * w + x] = 0xff000000 | (v << 8) | 0x40;
                    }
                }
                if mode == "noise" {
                    let (x0, y0) = (200usize.min(w), 150usize.min(h));
                    let (x1, y1) = ((x0 + 400).min(w), (y0 + 400).min(h));
                    for y in y0..y1 {
                        for x in x0..x1 {
                            rng ^= rng << 13;
                            rng ^= rng >> 7;
                            rng ^= rng << 17;
                            px[y * w + x] = 0xff000000 | (rng as u32 & 0xffffff);
                        }
                    }
                    damage.push((x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32));
                } else {
                    damage.push((0, 0, w as i32, h as i32));
                }
            }
        }
        for y in 0..120.min(h) {
            for x in 0..120.min(w) {
                px[y * w + x] = marker;
            }
        }
        damage.push((0, 0, 120, 120));
        self.rng = rng;

        let surface = self.surface.as_ref().unwrap();
        surface.attach(Some(&self.bufs[idx].buffer), 0, 0);
        for (x, y, dw, dh) in damage {
            surface.damage_buffer(x, y, dw, dh);
        }
        let animating = mode != "idle";
        if animating {
            surface.frame(qh, ());
            self.frame_pending = true;
        }
        surface.commit();
        self.bufs[idx].busy = true;
        self.frame += 1;
        self.frames_this_sec += 1;
        self.need_redraw = false;
    }
}

fn bytemuck_cast(b: &mut [u8]) -> &mut [u32] {
    assert!(b.as_ptr() as usize % 4 == 0 && b.len() % 4 == 0);
    unsafe { std::slice::from_raw_parts_mut(b.as_mut_ptr() as *mut u32, b.len() / 4) }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => s.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => s.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => s.wm_base = Some(reg.bind(name, 1, qh, ())),
                "wl_seat" => s.seat = Some(reg.bind(name, version.min(5), qh, ())),
                _ => {}
            }
        }
    }
}

macro_rules! ignore {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore!(wl_compositor::WlCompositor, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface);

impl Dispatch<wl_buffer::WlBuffer, usize> for State {
    fn event(s: &mut Self, _: &wl_buffer::WlBuffer, ev: wl_buffer::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = ev {
            if let Some(b) = s.bufs.get_mut(*idx) {
                b.busy = false;
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(s: &mut Self, _: &wl_callback::WlCallback, _: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        s.frame_pending = false;
        s.need_redraw = true;
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(_: &mut Self, wm: &xdg_wm_base::XdgWmBase, ev: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(s: &mut Self, xs: &xdg_surface::XdgSurface, ev: xdg_surface::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xs.ack_configure(serial);
            let resize = match s.pending_size.take() {
                Some((w, h)) if w > 0 && h > 0 && (w, h) != (s.width, s.height) => {
                    s.width = w;
                    s.height = h;
                    true
                }
                _ => false,
            };
            if !s.configured || resize {
                s.alloc(qh);
            }
            s.configured = true;
            s.need_redraw = true;
            s.frame_pending = false;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(s: &mut Self, _: &xdg_toplevel::XdgToplevel, ev: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                s.pending_size = Some((width, height))
            }
            xdg_toplevel::Event::Close => s.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, seat: &wl_seat::WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = ev {
            if c.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
            if c.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(qh, ());
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(s: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_keyboard::Event::Key { state: WEnum::Value(wl_keyboard::KeyState::Pressed), key, .. } = ev {
            s.inputs += 1;
            s.need_redraw = true;
            println!("INPUT {} n={} key={}", now_ms(), s.inputs, key);
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(s: &mut Self, _: &wl_pointer::WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_pointer::Event::Button { state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } = ev {
            s.inputs += 1;
            s.need_redraw = true;
            println!("INPUT {} n={} button", now_ms(), s.inputs);
        }
    }
}

fn main() {
    let mode_file = std::env::var("ANIMBOX_MODE_FILE").unwrap_or_else(|_| "/tmp/animbox-mode".into());
    let read_mode = || {
        std::fs::read_to_string(&mode_file)
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "idle".into())
    };
    let conn = Connection::connect_to_env().expect("connect");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut s = State {
        compositor: None,
        shm: None,
        wm_base: None,
        seat: None,
        surface: None,
        width: 1000,
        height: 700,
        pending_size: None,
        configured: false,
        closed: false,
        bufs: vec![],
        mmap: None,
        frame_pending: false,
        need_redraw: true,
        inputs: 0,
        frame: 0,
        frames_this_sec: 0,
        mode: read_mode(),
        rng: 0x9e3779b97f4a7c15,
    };
    queue.roundtrip(&mut s).unwrap();
    queue.roundtrip(&mut s).unwrap();
    let surface = s.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xs = s.wm_base.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let tl = xs.get_toplevel(&qh, ());
    tl.set_title("animbox".into());
    tl.set_app_id("animbox".into());
    surface.commit();
    s.surface = Some(surface);

    let mut last_stat = Instant::now();
    while !s.closed {
        queue.flush().unwrap();
        if let Some(guard) = queue.prepare_read() {
            let fd = guard.connection_fd();
            let mut pfd = libc::pollfd {
                fd: std::os::fd::AsRawFd::as_raw_fd(&fd),
                events: libc::POLLIN,
                revents: 0,
            };
            let r = unsafe { libc::poll(&mut pfd, 1, 50) };
            if r > 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        eprintln!("read error: {e}");
                        break;
                    }
                }
            }
        }
        if let Err(e) = queue.dispatch_pending(&mut s) {
            eprintln!("dispatch error: {e}");
            break;
        }
        if last_stat.elapsed() >= Duration::from_secs(1) {
            last_stat = Instant::now();
            let m = read_mode();
            if m != s.mode {
                s.mode = m;
                s.need_redraw = true;
                s.frame_pending = false;
            }
            println!("STAT {} fps={} mode={} inputs={}", now_ms(), s.frames_this_sec, s.mode, s.inputs);
            s.frames_this_sec = 0;
        }
        if s.configured && s.need_redraw && !s.frame_pending {
            s.draw(&qh);
        }
    }
}
