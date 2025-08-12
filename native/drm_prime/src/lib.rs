use std::ffi::{CStr, CString};
use std::fs::OpenOptions;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use drm::control::{Device as _, atomic::AtomicModeReq, connector, crtc, plane, property};
use drm::{ClientCapability, Device as _, buffer, control};
use rustler::{Atom, NifResult, ResourceArc};

#[cfg(feature = "verbose")]
macro_rules! log {
    ($($t:tt)*) => { println!($($t)*); };
}
#[cfg(not(feature = "verbose"))]
macro_rules! log {
    ($($t:tt)*) => {};
}

rustler::atoms! {
    ok
}

#[derive(rustler::NifStruct)]
#[module = "Membrane.DRM.Prime"]
struct PrimeDesc {
    fds: Vec<i32>,
    width: u32,
    height: u32,
    pitches: Vec<u32>,
    offsets: Vec<u32>,
}

struct ImportedBuffer {
    w: u32,
    h: u32,
    pitches: [u32; 4],
    offsets: [u32; 4],
    handles: [Option<buffer::Handle>; 4],
}

impl buffer::PlanarBuffer for ImportedBuffer {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn format(&self) -> buffer::DrmFourcc {
        buffer::DrmFourcc::Nv12
    }
    fn modifier(&self) -> Option<buffer::DrmModifier> {
        None
    }
    fn pitches(&self) -> [u32; 4] {
        self.pitches
    }
    fn handles(&self) -> [Option<buffer::Handle>; 4] {
        self.handles
    }
    fn offsets(&self) -> [u32; 4] {
        self.offsets
    }
}

struct Card(std::fs::File);
impl AsFd for Card {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl control::Device for Card {}

struct DisplayInner {
    card: Card,
    conn: connector::Handle,
    crtc: crtc::Handle,
    plane: plane::Handle,
    prop_fb: property::Handle,
    prop_crtc: property::Handle,
    prop_conn_crtc: property::Handle,
    prop_mode: property::Handle,
    prop_active: property::Handle,
    prop_src_x: property::Handle,
    prop_src_y: property::Handle,
    prop_src_w: property::Handle,
    prop_src_h: property::Handle,
    prop_crtc_x: property::Handle,
    prop_crtc_y: property::Handle,
    prop_crtc_w: property::Handle,
    prop_crtc_h: property::Handle,
    mode_blob: u64,
    setup: bool,
    last: Option<(control::framebuffer::Handle, Vec<buffer::Handle>)>,
}

fn open_card(path: &str) -> std::io::Result<Card> {
    log!("Opening DRM device: {path}");
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    Ok(Card(file))
}

fn enable_atomic(card: &Card) -> std::io::Result<()> {
    card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
    card.set_client_capability(ClientCapability::Atomic, true)?;
    log!("Enabled client caps: UNIVERSAL_PLANES + ATOMIC");
    Ok(())
}

fn find_prop(
    card: &Card,
    obj: impl control::ResourceHandle,
    name: &CStr,
) -> std::io::Result<property::Handle> {
    let props = card.get_properties(obj)?;
    for (handle, _) in props.iter() {
        let info = card.get_property(*handle)?;
        if info.name().to_bytes() == name.to_bytes() {
            return Ok(*handle);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("property {} not found", name.to_string_lossy()),
    ))
}

impl DisplayInner {
    fn new(card_path: &str) -> std::io::Result<Self> {
        let card = open_card(card_path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("open card: {e}")))?;
        enable_atomic(&card)
            .map_err(|e| std::io::Error::new(e.kind(), format!("enable atomic: {e}")))?;
        let res = card
            .resource_handles()
            .map_err(|e| std::io::Error::new(e.kind(), format!("get resources: {e}")))?;
        let conn = res
            .connectors()
            .iter()
            .find_map(|h| {
                let info = card.get_connector(*h, true).ok()?;
                if info.state() == connector::State::Connected && !info.modes().is_empty() {
                    Some(info)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no connected connector with modes",
                )
            })?;
        log!(
            "Selected connector: id={}, type={:?}, modes={}",
            u32::from(conn.handle()),
            conn.interface(),
            conn.modes().len()
        );
        let enc = conn.encoders().first().copied().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "connector has no encoders")
        })?;
        let enc_info = card
            .get_encoder(enc)
            .map_err(|e| std::io::Error::new(e.kind(), format!("get encoder: {e}")))?;
        log!(
            "Selected encoder: id={}, type={:?}",
            u32::from(enc_info.handle()),
            enc_info.kind()
        );
        let crtc = enc_info.crtc().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "encoder has no crtc")
        })?;
        log!("Selected CRTC: id={}", u32::from(crtc));
        let mode = conn.modes().first().copied().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "connector has no modes")
        })?;
        log!(
            "Selected mode: {}x{}@{}",
            mode.size().0,
            mode.size().1,
            mode.vrefresh()
        );
        let planes = card
            .plane_handles()
            .map_err(|e| std::io::Error::new(e.kind(), format!("plane handles: {e}")))?;
        let plane = planes
            .as_slice()
            .iter()
            .find_map(|p| {
                let info = card.get_plane(*p).ok()?;
                let allowed = res.filter_crtcs(info.possible_crtcs());
                if allowed.contains(&crtc)
                    && info.formats().contains(&(buffer::DrmFourcc::Nv12 as u32))
                {
                    Some(*p)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no suitable NV12 plane")
            })?;
        log!("Selected plane: id={}", u32::from(plane));

        let mode_blob_val = card
            .create_property_blob(&mode)
            .map_err(|e| std::io::Error::new(e.kind(), format!("create mode blob: {e}")))?;
        let blob_id = if let property::Value::Blob(id) = mode_blob_val {
            id
        } else {
            0
        };
        log!("Created mode blob id={}", blob_id);

        let name = |s: &str| CString::new(s).unwrap();
        let prop_fb = find_prop(&card, plane, &name("FB_ID"))?;
        let prop_crtc = find_prop(&card, plane, &name("CRTC_ID"))?;
        let prop_conn_crtc = find_prop(&card, conn.handle(), &name("CRTC_ID"))?;
        let prop_mode = find_prop(&card, crtc, &name("MODE_ID"))?;
        let prop_active = find_prop(&card, crtc, &name("ACTIVE"))?;
        let prop_src_x = find_prop(&card, plane, &name("SRC_X"))?;
        let prop_src_y = find_prop(&card, plane, &name("SRC_Y"))?;
        let prop_src_w = find_prop(&card, plane, &name("SRC_W"))?;
        let prop_src_h = find_prop(&card, plane, &name("SRC_H"))?;
        let prop_crtc_x = find_prop(&card, plane, &name("CRTC_X"))?;
        let prop_crtc_y = find_prop(&card, plane, &name("CRTC_Y"))?;
        let prop_crtc_w = find_prop(&card, plane, &name("CRTC_W"))?;
        let prop_crtc_h = find_prop(&card, plane, &name("CRTC_H"))?;

        Ok(Self {
            card,
            conn: conn.handle(),
            crtc,
            plane,
            prop_fb,
            prop_crtc,
            prop_conn_crtc,
            prop_mode,
            prop_active,
            prop_src_x,
            prop_src_y,
            prop_src_w,
            prop_src_h,
            prop_crtc_x,
            prop_crtc_y,
            prop_crtc_w,
            prop_crtc_h,
            mode_blob: blob_id,
            setup: false,
            last: None,
        })
    }

    fn display(&mut self, desc: PrimeDesc) -> std::io::Result<()> {
        let mut pitches = [0u32; 4];
        let mut offsets = [0u32; 4];
        for (i, p) in desc.pitches.iter().enumerate().take(4) {
            pitches[i] = *p;
        }
        for (i, o) in desc.offsets.iter().enumerate().take(4) {
            offsets[i] = *o;
        }
        let mut handles = [None; 4];
        let mut imported = Vec::new();
        for (i, fd) in desc.fds.iter().copied().enumerate().take(4) {
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
            let handle = self.card.prime_fd_to_buffer(borrowed).map_err(|e| {
                unsafe { libc::close(fd) };
                for h in imported.iter() {
                    let _ = self.card.close_buffer(*h);
                }
                for fd2 in desc.fds.iter().skip(i + 1) {
                    unsafe { libc::close(*fd2) };
                }
                std::io::Error::new(e.kind(), format!("prime fd to buffer: {e}"))
            })?;
            unsafe { libc::close(fd) };
            handles[i] = Some(handle);
            imported.push(handle);
        }
        for fd in desc.fds.iter().skip(4) {
            unsafe { libc::close(*fd) };
        }
        let buffer = ImportedBuffer {
            w: desc.width,
            h: desc.height,
            pitches,
            offsets,
            handles,
        };
        let fb = self
            .card
            .add_planar_framebuffer(&buffer, control::FbCmd2Flags::empty())
            .map_err(|e| {
                for h in imported.iter() {
                    let _ = self.card.close_buffer(*h);
                }
                std::io::Error::new(e.kind(), format!("add framebuffer: {e}"))
            })?;

        let mut req = AtomicModeReq::new();
        if !self.setup {
            req.add_property(
                self.plane,
                self.prop_crtc,
                property::Value::CRTC(Some(self.crtc)),
            );
            req.add_property(
                self.conn,
                self.prop_conn_crtc,
                property::Value::CRTC(Some(self.crtc)),
            );
            req.add_property(
                self.crtc,
                self.prop_mode,
                property::Value::Blob(self.mode_blob),
            );
            req.add_property(self.crtc, self.prop_active, property::Value::Boolean(true));
            req.add_property(
                self.plane,
                self.prop_src_x,
                property::Value::UnsignedRange(0),
            );
            req.add_property(
                self.plane,
                self.prop_src_y,
                property::Value::UnsignedRange(0),
            );
            req.add_property(
                self.plane,
                self.prop_src_w,
                property::Value::UnsignedRange((desc.width as u64) << 16),
            );
            req.add_property(
                self.plane,
                self.prop_src_h,
                property::Value::UnsignedRange((desc.height as u64) << 16),
            );
            req.add_property(
                self.plane,
                self.prop_crtc_x,
                property::Value::SignedRange(0),
            );
            req.add_property(
                self.plane,
                self.prop_crtc_y,
                property::Value::SignedRange(0),
            );
            req.add_property(
                self.plane,
                self.prop_crtc_w,
                property::Value::UnsignedRange(desc.width as u64),
            );
            req.add_property(
                self.plane,
                self.prop_crtc_h,
                property::Value::UnsignedRange(desc.height as u64),
            );
            self.setup = true;
        }
        req.add_property(
            self.plane,
            self.prop_fb,
            property::Value::Framebuffer(Some(fb)),
        );
        let flags = if self.last.is_none() {
            control::AtomicCommitFlags::ALLOW_MODESET
        } else {
            control::AtomicCommitFlags::empty()
        };
        if let Err(e) = self.card.atomic_commit(flags, req) {
            let _ = self.card.destroy_framebuffer(fb);
            for h in imported.iter() {
                let _ = self.card.close_buffer(*h);
            }
            return Err(std::io::Error::new(e.kind(), format!("atomic commit: {e}")));
        }

        if let Some((old_fb, old_handles)) = self.last.take() {
            let _ = self.card.destroy_framebuffer(old_fb);
            for h in old_handles {
                let _ = self.card.close_buffer(h);
            }
        }
        self.last = Some((fb, imported));
        Ok(())
    }
}

impl Drop for DisplayInner {
    fn drop(&mut self) {
        if let Some((fb, handles)) = self.last.take() {
            let _ = self.card.destroy_framebuffer(fb);
            for h in handles {
                let _ = self.card.close_buffer(h);
            }
        }
        let _ = self.card.destroy_property_blob(self.mode_blob);
    }
}

impl DisplayInner {
    fn run(mut self, rx: mpsc::Receiver<PrimeDesc>, err: Arc<Mutex<Option<String>>>) {
        while let Ok(mut desc) = rx.recv() {
            while let Ok(d) = rx.try_recv() {
                for fd in desc.fds.drain(..) {
                    unsafe { libc::close(fd) };
                }
                desc = d;
            }
            if let Err(e) = self.display(desc) {
                let msg = e.to_string();
                let _ = err.lock().map(|mut g| *g = Some(msg.clone()));
                log!("Display failed: {msg}");
                break;
            }
        }
        // dropping self cleans up resources
    }
}

struct Display {
    tx: Option<mpsc::Sender<PrimeDesc>>,
    handle: Option<thread::JoinHandle<()>>,
    err: Arc<Mutex<Option<String>>>,
}

impl Display {
    fn new(card_path: &str) -> std::io::Result<Self> {
        let inner = DisplayInner::new(card_path)?;
        let (tx, rx) = mpsc::channel();
        let err = Arc::new(Mutex::new(None));
        let err_clone = Arc::clone(&err);
        let handle = thread::spawn(move || inner.run(rx, err_clone));
        Ok(Self {
            tx: Some(tx),
            handle: Some(handle),
            err,
        })
    }

    fn display(&self, desc: PrimeDesc) -> std::io::Result<()> {
        let tx = self.tx.as_ref().ok_or_else(|| {
            log!("Display channel missing");
            std::io::Error::from(std::io::ErrorKind::BrokenPipe)
        })?;
        tx.send(desc).map_err(|e| {
            for fd in e.0.fds {
                unsafe { libc::close(fd) };
            }
            let msg = self
                .err
                .lock()
                .ok()
                .and_then(|mut g| g.take())
                .unwrap_or_else(|| "broken pipe".to_string());
            log!("Display thread disconnected: {msg}");
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, msg)
        })
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct DisplayRes(Mutex<Option<Display>>);

unsafe impl Send for DisplayRes {}
unsafe impl Sync for DisplayRes {}

fn nif_err<E: std::fmt::Display>(e: E) -> rustler::Error {
    rustler::Error::Term(Box::new(format!("{e}")))
}

#[allow(non_local_definitions)]
fn load(env: rustler::Env, _info: rustler::Term) -> bool {
    rustler::resource!(DisplayRes, env)
}

#[rustler::nif]
fn init_display(card_path: String) -> NifResult<ResourceArc<DisplayRes>> {
    let display = Display::new(&card_path).map_err(nif_err)?;
    Ok(ResourceArc::new(DisplayRes(Mutex::new(Some(display)))))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn display_prime(res: ResourceArc<DisplayRes>, desc: PrimeDesc) -> NifResult<Atom> {
    let mut guard = res.0.lock().map_err(|_| nif_err("lock"))?;
    if let Some(display) = guard.as_mut() {
        let res = display.display(desc);
        if let Err(err) = res {
            let _ = guard.take();
            Err(nif_err(err))
        } else {
            Ok(ok())
        }
    } else {
        Err(nif_err("display closed"))
    }
}

#[rustler::nif]
fn close_display(res: ResourceArc<DisplayRes>) -> NifResult<Atom> {
    let mut guard = res.0.lock().map_err(|_| nif_err("lock"))?;
    let _ = guard.take();
    Ok(ok())
}

rustler::init!("Elixir.DrmPrime.Native", load = load);
