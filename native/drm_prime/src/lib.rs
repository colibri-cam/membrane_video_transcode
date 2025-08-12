use std::ffi::{CStr, CString};
use std::fs::OpenOptions;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::{Mutex, mpsc};
use std::thread;

use drm::control::{Device as _, atomic::AtomicModeReq, connector, crtc, plane, property};
use drm::{ClientCapability, Device as _};
use drm::{buffer, control};
use rustler::{Atom, Env, NifResult, ResourceArc};

rustler::atoms! {
    ok
}

#[derive(rustler::NifStruct)]
#[module = "Membrane.DRM.Prime"]
struct PrimeDesc {
    fd: i32,
    width: u32,
    height: u32,
    pixel_format: Atom,
    pitches: Vec<u32>,
    offsets: Vec<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(clippy::upper_case_acronyms)]
enum PixelFormat {
    I420,
    I422,
    I444,
    RGB,
    BGRA,
    RGBA,
    NV12,
    NV21,
    YV12,
    AYUV,
    YUY2,
}

impl PixelFormat {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "I420" => Some(Self::I420),
            "I422" => Some(Self::I422),
            "I444" => Some(Self::I444),
            "RGB" => Some(Self::RGB),
            "BGRA" => Some(Self::BGRA),
            "RGBA" => Some(Self::RGBA),
            "NV12" => Some(Self::NV12),
            "NV21" => Some(Self::NV21),
            "YV12" => Some(Self::YV12),
            "AYUV" => Some(Self::AYUV),
            "YUY2" => Some(Self::YUY2),
            _ => None,
        }
    }

    fn fourcc(self) -> buffer::DrmFourcc {
        use buffer::DrmFourcc as F;
        match self {
            Self::I420 => F::Yuv420,
            Self::I422 => F::Yuv422,
            Self::I444 => F::Yuv444,
            Self::RGB => F::Rgb888,
            Self::BGRA => F::Bgra8888,
            Self::RGBA => F::Rgba8888,
            Self::NV12 => F::Nv12,
            Self::NV21 => F::Nv21,
            Self::YV12 => F::Yvu420,
            Self::AYUV => F::Ayuv,
            Self::YUY2 => F::Yuyv,
        }
    }
}

struct ImportedBuffer {
    w: u32,
    h: u32,
    fmt: buffer::DrmFourcc,
    pitches: [u32; 4],
    offsets: [u32; 4],
    handles: [Option<buffer::Handle>; 4],
}

impl buffer::PlanarBuffer for ImportedBuffer {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn format(&self) -> buffer::DrmFourcc {
        self.fmt
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
    fmt: PixelFormat,
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
    last: Option<(control::framebuffer::Handle, buffer::Handle)>,
}

fn open_card(path: &str) -> std::io::Result<Card> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    Ok(Card(file))
}

fn enable_atomic(card: &Card) -> std::io::Result<()> {
    card.set_client_capability(ClientCapability::Atomic, true)?;
    card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
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
    Err(std::io::Error::from(std::io::ErrorKind::NotFound))
}

impl DisplayInner {
    fn new(card_path: &str, fmt: PixelFormat) -> std::io::Result<Self> {
        let card = open_card(card_path)?;
        enable_atomic(&card)?;
        let res = card.resource_handles()?;
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
            .ok_or(std::io::Error::from(std::io::ErrorKind::NotFound))?;
        let enc = conn
            .encoders()
            .first()
            .copied()
            .ok_or(std::io::Error::from(std::io::ErrorKind::NotFound))?;
        let enc_info = card.get_encoder(enc)?;
        let crtc = enc_info
            .crtc()
            .ok_or(std::io::Error::from(std::io::ErrorKind::NotFound))?;
        let mode = conn
            .modes()
            .first()
            .copied()
            .ok_or(std::io::Error::from(std::io::ErrorKind::NotFound))?;
        let planes = card.plane_handles()?;
        let plane = planes
            .as_slice()
            .iter()
            .find_map(|p| {
                let info = card.get_plane(*p).ok()?;
                let allowed = res.filter_crtcs(info.possible_crtcs());
                if allowed.contains(&crtc) && info.formats().contains(&(fmt.fourcc() as u32)) {
                    Some(*p)
                } else {
                    None
                }
            })
            .ok_or(std::io::Error::from(std::io::ErrorKind::NotFound))?;

        let mode_blob_val = card.create_property_blob(&mode)?;
        let blob_id = if let property::Value::Blob(id) = mode_blob_val {
            id
        } else {
            0
        };

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
            fmt,
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
        let borrowed = unsafe { BorrowedFd::borrow_raw(desc.fd) };
        let handle = self.card.prime_fd_to_buffer(borrowed)?;
        unsafe { libc::close(desc.fd) };
        let mut pitches = [0u32; 4];
        let mut offsets = [0u32; 4];
        for (i, p) in desc.pitches.iter().enumerate().take(4) {
            pitches[i] = *p;
        }
        for (i, o) in desc.offsets.iter().enumerate().take(4) {
            offsets[i] = *o;
        }
        let buffer = ImportedBuffer {
            w: desc.width,
            h: desc.height,
            fmt: self.fmt.fourcc(),
            pitches,
            offsets,
            handles: [Some(handle), None, None, None],
        };
        let fb = self
            .card
            .add_planar_framebuffer(&buffer, control::FbCmd2Flags::empty())?;

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
            control::AtomicCommitFlags::PAGE_FLIP_EVENT
        };
        self.card.atomic_commit(flags, req)?;

        if let Some((old_fb, old_handle)) = self.last.take() {
            let _ = self.card.destroy_framebuffer(old_fb);
            let _ = self.card.close_buffer(old_handle);
        }
        self.last = Some((fb, handle));
        Ok(())
    }
}

impl Drop for DisplayInner {
    fn drop(&mut self) {
        if let Some((fb, handle)) = self.last.take() {
            let _ = self.card.destroy_framebuffer(fb);
            let _ = self.card.close_buffer(handle);
        }
        let _ = self.card.destroy_property_blob(self.mode_blob);
    }
}

impl DisplayInner {
    fn run(mut self, rx: mpsc::Receiver<PrimeDesc>) {
        while let Ok(mut desc) = rx.recv() {
            while let Ok(d) = rx.try_recv() {
                unsafe {
                    libc::close(d.fd);
                }
                desc = d;
            }
            if self.display(desc).is_err() {
                break;
            }
        }
        // dropping self cleans up resources
    }
}

struct Display {
    tx: Option<mpsc::Sender<PrimeDesc>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Display {
    fn new(card_path: &str, fmt: PixelFormat) -> std::io::Result<Self> {
        let inner = DisplayInner::new(card_path, fmt)?;
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || inner.run(rx));
        Ok(Self {
            tx: Some(tx),
            handle: Some(handle),
        })
    }

    fn display(&self, desc: PrimeDesc) -> std::io::Result<()> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        tx.send(desc).map_err(|e| {
            unsafe {
                libc::close(e.0.fd);
            }
            std::io::Error::from(std::io::ErrorKind::BrokenPipe)
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
fn init_display<'a>(
    env: Env<'a>,
    card_path: String,
    pixel_format: Atom,
) -> NifResult<ResourceArc<DisplayRes>> {
    let pf_str = pixel_format
        .to_term(env)
        .atom_to_string()
        .map_err(|e| nif_err(format!("{e:?}")))?;
    let pf = PixelFormat::from_str(&pf_str).ok_or_else(|| nif_err("unknown pixel format"))?;
    let display = Display::new(&card_path, pf).map_err(nif_err)?;
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
