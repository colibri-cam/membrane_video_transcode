use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use anyhow::{Context, Result, bail}; // gives you Result and bail!
use drm::buffer::{DrmModifier, Handle as GemHandle, PlanarBuffer};

use drm::control::{
    Device as _, FbCmd2Flags, atomic::AtomicModeReq, connector, crtc, framebuffer, plane, property,
};
use drm::{ClientCapability, Device as _, buffer, control};
use drm_fourcc::DrmFourcc;
use rustler::{Atom, Decoder, Encoder, NifResult, ResourceArc};

#[cfg(feature = "rpi")]
const DRM_FORMAT_MOD_BROADCOM_SAND128: u64 = 0x0700_0000_0000_0004;

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

#[derive(Debug)]
struct Fd(OwnedFd);

impl AsFd for Fd {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[derive(Debug, rustler::NifStruct)]
#[module = "Membrane.PrimePlane"]
struct PrimePlane {
    fd: Fd,
    pitch: u32,
    offset: u32,
    modifier: Option<u64>,
}

#[derive(Debug, rustler::NifStruct)]
#[module = "Membrane.PrimeDesc"]
struct PrimeDesc {
    width: u32,
    height: u32,
    format: Fourcc,
    planes: Vec<PrimePlane>,
}

#[derive(Debug, rustler::NifStruct)]
#[module = "Membrane.DRM.PrimeSink.DisplayInfo"]
struct DisplayInfo {
    card_path: String,
    connector_id: u32,
    connector_type: String,
    crtc_id: u32,
    plane_id: u32,
    mode: (u32, u32, u32),
}

impl Encoder for Fd {
    fn encode<'a>(&self, env: rustler::Env<'a>) -> rustler::Term<'a> {
        let dup_fd = unsafe { libc::dup(self.0.as_raw_fd()) };
        dup_fd.encode(env)
    }
}

impl<'a> Decoder<'a> for Fd {
    fn decode(term: rustler::Term<'a>) -> NifResult<Self> {
        let fd: i32 = term.decode()?;
        if fd < 0 {
            Err(rustler::Error::BadArg)
        } else {
            Ok(Fd(unsafe { OwnedFd::from_raw_fd(fd) }))
        }
    }
}

#[derive(Debug)]
struct Fourcc(DrmFourcc);

#[derive(Clone, Debug)]
pub struct FbWithHandles {
    pub fb: framebuffer::Handle,
    pub handles: [Option<GemHandle>; 4], // keep to close after vblank
}

/// In-memory PlanarBuffer based on your PrimeDesc.
/// drm 0.14 expects a single global modifier via `modifier()`.
struct PrimePlanarBuf {
    w: u32,
    h: u32,
    fourcc: DrmFourcc,
    pitches: [u32; 4],
    offsets: [u32; 4],
    handles: [Option<GemHandle>; 4],
    modifier: Option<DrmModifier>, // single, not per-plane
}

impl PlanarBuffer for PrimePlanarBuf {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn format(&self) -> DrmFourcc {
        self.fourcc
    }
    fn pitches(&self) -> [u32; 4] {
        self.pitches
    }
    fn handles(&self) -> [Option<GemHandle>; 4] {
        self.handles
    }
    fn offsets(&self) -> [u32; 4] {
        self.offsets
    }
    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier
    }
}

impl Encoder for Fourcc {
    fn encode<'a>(&self, env: rustler::Env<'a>) -> rustler::Term<'a> {
        (self.0 as u32).encode(env)
    }
}

impl<'a> Decoder<'a> for Fourcc {
    fn decode(term: rustler::Term<'a>) -> NifResult<Self> {
        let val: u32 = term.decode()?;
        Ok(Fourcc(
            DrmFourcc::try_from(val).map_err(|_| rustler::Error::BadArg)?,
        ))
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

fn driver_is_vc4(card: &Card) -> bool {
    card.get_driver()
        .ok()
        .and_then(|info| info.name().to_str().map(|s| s.to_owned()))
        .map(|name| name.contains("vc4"))
        .unwrap_or(false)
}

fn find_vc4_card() -> std::io::Result<String> {
    for entry in std::fs::read_dir("/dev/dri")? {
        let path = entry?.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("card") {
                if let Ok(card) = open_card(path.to_str().unwrap()) {
                    if driver_is_vc4(&card) {
                        return Ok(path.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "No vc4 DRM device",
    ))
}

struct DisplayInner {
    card: Card,
    #[cfg(feature = "rpi")]
    is_vc4: bool,
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
    stale: Option<FbWithHandles>,
    in_flight: Option<FbWithHandles>,
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

fn close_unique_handles<F, H>(handles: &[Option<H>], mut close_fn: F) -> std::io::Result<()>
where
    H: Eq + std::hash::Hash + Copy,
    F: FnMut(H) -> std::io::Result<()>,
{
    let mut seen = HashSet::new();
    for h in handles.iter().flatten().copied() {
        if seen.insert(h) {
            close_fn(h)?;
        }
    }
    Ok(())
}

fn pick_connected_connector(
    card: &Card,
    res: &control::ResourceHandles,
    prefer_hdmi: bool,
) -> std::io::Result<connector::Info> {
    let mut fallback = None;
    for &conn_h in res.connectors() {
        let info = card.get_connector(conn_h, true)?;
        if info.state() == connector::State::Connected && !info.modes().is_empty() {
            if prefer_hdmi {
                match info.interface() {
                    connector::Interface::HDMIA | connector::Interface::HDMIB => {
                        log!(
                            "Selected HDMI connector: id={}, modes={}",
                            u32::from(info.handle()),
                            info.modes().len()
                        );
                        return Ok(info);
                    }
                    _ => {}
                }
            }
            if fallback.is_none() {
                fallback = Some(info);
            }
        }
    }
    if let Some(info) = fallback {
        log!(
            "Selected connector: id={}, type={:?}, modes={}",
            u32::from(info.handle()),
            info.interface(),
            info.modes().len()
        );
        Ok(info)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no connected connector",
        ))
    }
}

fn plane_is_primary(card: &Card, ph: plane::Handle) -> bool {
    if let Ok(props) = card.get_properties(ph) {
        for (handle, value) in props.iter() {
            if let Ok(info) = card.get_property(*handle) {
                if info.name().to_bytes() == b"type" {
                    return *value == 0;
                }
            }
        }
    }
    false
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
    fn new(
        card_path: &str,
        preferred_mode: Option<(u32, u32, u32)>,
    ) -> std::io::Result<(Self, DisplayInfo)> {
        let card = open_card(card_path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("open card: {e}")))?;
        let is_vc4 = driver_is_vc4(&card);
        enable_atomic(&card)
            .map_err(|e| std::io::Error::new(e.kind(), format!("enable atomic: {e}")))?;
        let res = card
            .resource_handles()
            .map_err(|e| std::io::Error::new(e.kind(), format!("get resources: {e}")))?;
        let conn = pick_connected_connector(&card, &res, is_vc4)?;
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
        let modes = conn.modes();
        let mode = if let Some((w, h, r)) = preferred_mode {
            modes
                .iter()
                .find(|m| {
                    let size = m.size();
                    u32::from(size.0) == w && u32::from(size.1) == h && m.vrefresh() == r
                })
                .copied()
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("preferred mode {w}x{h}@{r} not found"),
                    )
                })?
        } else {
            modes
                .iter()
                .max_by(|a, b| {
                    let size_a = a.size();
                    let size_b = b.size();
                    let area_a = u32::from(size_a.0) * u32::from(size_a.1);
                    let area_b = u32::from(size_b.0) * u32::from(size_b.1);
                    match area_a.cmp(&area_b) {
                        std::cmp::Ordering::Equal => a.vrefresh().cmp(&b.vrefresh()),
                        other => other,
                    }
                })
                .copied()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, "connector has no modes")
                })?
        };
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
                    && (!is_vc4 || plane_is_primary(&card, *p))
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

        let mode_info = (
            u32::from(mode.size().0),
            u32::from(mode.size().1),
            mode.vrefresh(),
        );
        let info = DisplayInfo {
            card_path: card_path.to_string(),
            connector_id: u32::from(conn.handle()),
            connector_type: format!("{:?}", conn.interface()),
            crtc_id: u32::from(crtc),
            plane_id: u32::from(plane),
            mode: mode_info,
        };

        Ok((
            Self {
                card,
                #[cfg(feature = "rpi")]
                is_vc4,
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
                stale: None,
                in_flight: None,
            },
            info,
        ))
    }

    fn display(&mut self, desc: PrimeDesc) -> std::io::Result<()> {
        let new_fb = self.add_fb_from_prime_desc(&desc).map_err(|e| {
            log!("Add fb from prime error: {}", e);
            io::Error::other(e)
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
            property::Value::Framebuffer(Some(new_fb.fb)),
        );

        let flags = if self.stale.is_none() && self.in_flight.is_none() {
            control::AtomicCommitFlags::ALLOW_MODESET
        } else {
            control::AtomicCommitFlags::empty()
        };
        if let Err(e) = self.card.atomic_commit(flags, req) {
            let _ = self.card.destroy_framebuffer(new_fb.fb);
            close_unique_handles(&new_fb.handles, |h| self.card.close_buffer(h))?;
            return Err(std::io::Error::new(e.kind(), format!("atomic commit: {e}")));
        };
        if let Some(stale_fb) = self.stale.take() {
            log!("Dropping stale framebuffer {:?}\n", stale_fb);
            // 1) Drop the KMS FB
            self.card.destroy_framebuffer(stale_fb.fb)?;
            // 2) Close handles associated with the stale framebuffer
            close_unique_handles(&stale_fb.handles, |h| self.card.close_buffer(h))?;
        }

        self.stale = self.in_flight.take();
        self.in_flight = Some(new_fb);
        Ok(())
    }

    /// Create a KMS framebuffer from your PrimeDesc using drm-rs 0.14.
    /// Accept any type that implements both `drm::Device` and `drm::control::Device`
    /// (e.g., `&drm::DeviceFd`).
    fn add_fb_from_prime_desc(&mut self, prime: &PrimeDesc) -> Result<FbWithHandles> {
        if prime.planes.is_empty() {
            bail!("PrimeDesc has no planes");
        }
        let n = prime.planes.len().min(4);

        // Import planes (dmabuf -> GEM)
        let mut handles: [Option<GemHandle>; 4] = [None, None, None, None];
        let mut pitches: [u32; 4] = [0; 4];
        let mut offsets: [u32; 4] = [0; 4];

        // Collect per-plane modifiers, then collapse to one
        let mut mods_raw: [Option<u64>; 4] = [None, None, None, None];
        for i in 0..n {
            let p = &prime.planes[i];
            let gem = self
                .card
                .prime_fd_to_buffer(p.fd.as_fd())
                .with_context(|| format!("prime_fd_to_buffer failed on plane {}", i))?;
            handles[i] = Some(gem);
            pitches[i] = p.pitch;
            offsets[i] = p.offset;
            mods_raw[i] = p.modifier;
        }

        // Collapse modifiers: all Some(m) must be the same, otherwise bail.
        let mut common_mod: Option<u64> = None;
        for m in mods_raw.iter().flatten().copied() {
            match common_mod {
                None => common_mod = Some(m),
                Some(prev) if prev == m => {}
                Some(prev) => bail!("mixed plane modifiers not supported ({} vs {})", prev, m),
            }
        }
        #[allow(unused_mut)]
        let mut modifier = common_mod.map(DrmModifier::from);
        #[cfg(feature = "rpi")]
        {
            if modifier.is_none() && self.is_vc4 {
                modifier = Some(DrmModifier::from(DRM_FORMAT_MOD_BROADCOM_SAND128));
            }
        }

        // Build PlanarBuffer
        let pb = PrimePlanarBuf {
            w: prime.width,
            h: prime.height,
            fourcc: prime.format.0, // your Fourcc newtype wraps DrmFourcc
            pitches,
            offsets,
            handles,
            modifier,
        };

        // Use MODIFIERS flag iff we actually have one
        let flags = if modifier.is_some() {
            FbCmd2Flags::MODIFIERS
        } else {
            FbCmd2Flags::empty()
        };

        let fb = self
            .card
            .add_planar_framebuffer(&pb, flags)
            .context("add_planar_framebuffer failed")?;

        Ok(FbWithHandles { fb, handles })
    }
}

impl Drop for DisplayInner {
    fn drop(&mut self) {
        if let Some(FbWithHandles { fb, handles }) = self.stale.take() {
            let _ = self.card.destroy_framebuffer(fb);
            let _ = close_unique_handles(&handles, |h| self.card.close_buffer(h));
        }
        if let Some(FbWithHandles { fb, handles }) = self.in_flight.take() {
            let _ = self.card.destroy_framebuffer(fb);
            let _ = close_unique_handles(&handles, |h| self.card.close_buffer(h));
        }
        let _ = self.card.destroy_property_blob(self.mode_blob);
    }
}

impl DisplayInner {
    fn run(mut self, rx: mpsc::Receiver<PrimeDesc>, err: Arc<Mutex<Option<String>>>) {
        while let Ok(mut desc) = rx.recv() {
            // If there are more frames in the queue
            // take only last one
            while let Ok(d) = rx.try_recv() {
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
}

impl Display {
    fn new(
        card_path: &str,
        preferred_mode: Option<(u32, u32, u32)>,
    ) -> std::io::Result<(Self, DisplayInfo)> {
        let (inner, info) = DisplayInner::new(card_path, preferred_mode)?;
        let (tx, rx) = mpsc::channel();
        let err = Arc::new(Mutex::new(None));
        let err_clone = Arc::clone(&err);
        let handle = thread::spawn(move || inner.run(rx, err_clone));
        Ok((
            Self {
                tx: Some(tx),
                handle: Some(handle),
            },
            info,
        ))
    }

    fn display(&self, desc: PrimeDesc) -> std::io::Result<()> {
        let tx = self.tx.as_ref().ok_or_else(|| {
            log!("Display channel missing");
            std::io::Error::from(std::io::ErrorKind::BrokenPipe)
        })?;
        tx.send(desc).map_err(|_| {
            log!("Send desc failed");
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
fn init_display(
    card_path: String,
    preferred_mode: Option<(u32, u32, u32)>,
) -> NifResult<(Atom, DisplayInfo, ResourceArc<DisplayRes>)> {
    let path = if card_path.is_empty() {
        find_vc4_card().map_err(nif_err)?
    } else {
        card_path
    };
    let (display, info) = Display::new(&path, preferred_mode).map_err(nif_err)?;
    Ok((
        ok(),
        info,
        ResourceArc::new(DisplayRes(Mutex::new(Some(display)))),
    ))
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

rustler::init!("Elixir.Membrane.DRM.PrimeSink.Native", load = load);
