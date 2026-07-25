use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail}; // gives you Result and bail!
use drm::buffer::{Buffer as _, DrmModifier, Handle as GemHandle, PlanarBuffer};

use drm::control::{
    Device as _, FbCmd2Flags, atomic::AtomicModeReq, connector, crtc, dumbbuffer as dumbbuf,
    encoder, framebuffer, plane, property,
};
use drm::{ClientCapability, Device as _, buffer, control};
use drm_fourcc::DrmFourcc;
use rustler::env::{OwnedEnv, SavedTerm};
use rustler::{Atom, Binary, Decoder, Encoder, Env, LocalPid, NifResult, ResourceArc, Term};
use std::io::ErrorKind;

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

    fn bpp(self) -> u32 {
        match self {
            Self::RGB => 24,
            Self::BGRA | Self::RGBA | Self::AYUV => 32,
            Self::YUY2 => 16,
            _ => 8,
        }
    }

    fn buffer_height(self, height: u32) -> u32 {
        match self {
            Self::I420 | Self::YV12 => height * 2,
            Self::NV12 | Self::NV21 => height * 3 / 2,
            Self::I422 | Self::I444 => height * 3,
            _ => height,
        }
    }

    fn frame_size(self, width: u32, height: u32) -> usize {
        let width = width as usize;
        let height = height as usize;

        match self {
            Self::I420 | Self::NV12 | Self::NV21 | Self::YV12 => width * height * 3 / 2,
            Self::I422 | Self::YUY2 => width * height * 2,
            Self::I444 | Self::RGB => width * height * 3,
            Self::BGRA | Self::RGBA | Self::AYUV => width * height * 4,
        }
    }

    fn num_planes(self) -> usize {
        match self {
            Self::I420 | Self::I422 | Self::I444 | Self::YV12 => 3,
            Self::NV12 | Self::NV21 => 2,
            _ => 1,
        }
    }

    fn fb_format(self) -> Self {
        match self {
            Self::I420 => Self::NV12,
            other => other,
        }
    }
}

pub struct FrozenTerm {
    env: Option<OwnedEnv>,    // taken on first send
    saved: Option<SavedTerm>, // taken on first send
}

impl FrozenTerm {
    /// If you need to read the term before it's sent, load it from a target env.
    /// Panics if already used (you can change to return Result if you prefer).
    pub fn load<'a>(&self, env: Env<'a>) -> Term<'a> {
        self.saved
            .as_ref()
            .expect("FrozenTerm already used")
            .load(env)
    }

    /// One-shot send: moves the OwnedEnv + SavedTerm and sends a message built by `make_msg`.
    pub fn send_once_with<F>(&mut self, pid: &LocalPid, make_msg: F)
    where
        // HRTB: closure works for any env lifetime
        F: for<'a> FnOnce(Env<'a>, Term<'a>) -> Term<'a>,
    {
        if let (Some(mut oenv), Some(saved)) = (self.env.take(), self.saved.take()) {
            let _ = oenv.send_and_clear(pid, move |env| {
                let payload = saved.load(env);
                make_msg(env, payload)
            });
        } else {
            // already used; ignore or log as you wish
        }
    }
}

impl<'a> Decoder<'a> for FrozenTerm {
    fn decode(t: Term<'a>) -> NifResult<Self> {
        let oenv = OwnedEnv::new();
        let saved = oenv.save(t);
        Ok(FrozenTerm {
            env: Some(oenv),
            saved: Some(saved), // <-- wrap in Some(...)
        })
    }
}

impl Encoder for FrozenTerm {
    fn encode<'b>(&self, env: Env<'b>) -> Term<'b> {
        // allow encoding before send_once_with is called
        self.saved
            .as_ref()
            .expect("FrozenTerm already used")
            .load(env)
    }
}

rustler::atoms! {
    ok,
    keepalive,
    display_connected,
    display_waiting,
    display_disconnected,
    trace_event,
    native_submit,
    native_submit_error,
    native_release_replaced,
    native_release_displayed,
    native_release_pending
}

#[derive(Debug)]
struct Fd(OwnedFd);

impl AsFd for Fd {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[derive(rustler::NifStruct)]
#[module = "Membrane.PrimePlane"]
struct PrimePlane {
    obj_idx: u32,
    pitch: u32,
    offset: u32,
}

#[derive(rustler::NifStruct)]
#[module = "Membrane.PrimeObject"]
struct PrimeObject {
    fd: Fd,
    modifier: Option<u64>,
}

#[derive(rustler::NifStruct)]
#[module = "Membrane.PrimeDesc"]
struct PrimeDesc {
    width: u32,
    height: u32,
    format: Fourcc,
    objects: Vec<PrimeObject>,
    planes: Vec<PrimePlane>,
    keepalive: FrozenTerm,
    owner_pid: LocalPid,
    trace_token: Option<TraceToken>,
}

#[derive(Clone, Debug, rustler::NifStruct)]
#[module = "Membrane.Instrumentation.TraceToken"]
struct TraceToken {
    trace_id: u64,
    frame_id: u64,
    created_at_ns: u64,
    sampled: bool,
    pts: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, rustler::NifStruct)]
#[module = "Membrane.Display.Sink.DisplayInfo"]
struct DisplayInfo {
    card_path: String,
    connector_id: u32,
    connector_type: String,
    crtc_id: u32,
    plane_id: u32,
    mode: (u32, u32, u32),
}

fn send_keepalive_message(mut keepalive_term: FrozenTerm, owner_pid: &LocalPid) {
    keepalive_term.send_once_with(owner_pid, |env, payload| (keepalive(), payload).encode(env));
}

fn send_trace_event(pid: &LocalPid, stage: Atom, token: TraceToken, duration_ns: Option<u64>) {
    send_message(pid, move |env| {
        (trace_event(), stage, token, duration_ns).encode(env)
    });
}

fn release_prime_desc(desc: PrimeDesc, listener: &LocalPid, stage: Atom) {
    if let Some(token) = desc.trace_token.clone()
        && token.sampled
    {
        send_trace_event(listener, stage, token, None);
    }
    send_keepalive_message(desc.keepalive, &desc.owner_pid);
}

fn send_message<F>(pid: &LocalPid, make_msg: F)
where
    F: for<'a> FnOnce(Env<'a>) -> Term<'a>,
{
    let mut env = OwnedEnv::new();
    let _ = env.send_and_clear(pid, make_msg);
}

fn notify_display_waiting(pid: &LocalPid, reason: String) {
    send_message(pid, move |env| (display_waiting(), reason).encode(env));
}

fn notify_display_connected(pid: &LocalPid, info: DisplayInfo) {
    send_message(pid, move |env| (display_connected(), info).encode(env));
}

fn notify_display_disconnected(pid: &LocalPid, reason: String) {
    send_message(pid, move |env| (display_disconnected(), reason).encode(env));
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
struct FbWithHandles {
    fb: framebuffer::Handle,
    handles: [Option<GemHandle>; 4], // keep to close after vblank
}

struct DisplayUnit {
    fbwh: FbWithHandles,
    keepalive: FrozenTerm,
    owner_pid: LocalPid,
    trace_token: Option<TraceToken>,
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
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with("card")
            && let Ok(card) = open_card(path.to_str().unwrap())
            && driver_is_vc4(&card)
        {
            return Ok(path.to_string_lossy().into_owned());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "No vc4 DRM device",
    ))
}

struct DisplayInner {
    card: Card,
    listener: LocalPid,
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
    // Optional/driver-specific properties
    prop_zpos: Option<property::Handle>,
    mode_blob: u64,
    setup: bool,
    stale: Option<DisplayUnit>,
    in_flight: Option<DisplayUnit>,
}

fn open_card(path: &str) -> std::io::Result<Card> {
    log!("Opening DRM device: {path}");
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    Ok(Card(file))
}

fn enable_atomic(card: &Card) -> std::io::Result<()> {
    card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
    card.set_client_capability(ClientCapability::Atomic, true)?;
    // Enable AddFB2 modifiers where supported (ignore error on old kernels)
    // Note: AddFB2Modifiers is a device capability (not a client capability) in drm-rs 0.14.
    // We will probe it where needed instead.
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
            match close_fn(h) {
                Ok(_) => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e)
                    if e.kind() == ErrorKind::InvalidInput || e.kind() == ErrorKind::NotFound =>
                {
                    // Some drivers or wrapper layers report these when a handle was already closed.
                    log!("close_buffer(): already closed ({e})");
                    break;
                }
                #[cfg(feature = "verbose")]
                Err(e) => {
                    // Don’t crash the process/NIF; just log and continue.
                    log!("close_buffer() failed: {e}");
                    break;
                }
                #[cfg(not(feature = "verbose"))]
                Err(_) => {
                    break;
                }
            }
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

/// DRM plane type enum values are driver-defined but most drivers use:
/// 0=OVERLAY, 1=PRIMARY, 2=CURSOR (matches modetest)
fn plane_type_value(card: &Card, ph: plane::Handle) -> Option<u64> {
    if let Ok(props) = card.get_properties(ph) {
        for (handle, value) in props.iter() {
            if let Ok(info) = card.get_property(*handle)
                && info.name().to_bytes() == b"type"
            {
                return Some(*value);
            }
        }
    }
    None
}

fn plane_is_overlay(card: &Card, ph: plane::Handle) -> bool {
    plane_type_value(card, ph) == Some(0)
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

fn default_mode(conn: &connector::Info) -> std::io::Result<control::Mode> {
    conn.modes()
        .first()
        .copied()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "connector has no modes"))
}

fn pick_mode(
    conn: &connector::Info,
    preferred_mode: Option<(u32, u32, u32)>,
) -> std::io::Result<control::Mode> {
    let modes = conn.modes();

    let mode = if let Some((width, height, refresh)) = preferred_mode {
        modes
            .iter()
            .find(|mode| {
                let size = mode.size();
                u32::from(size.0) == width
                    && u32::from(size.1) == height
                    && mode.vrefresh() == refresh
            })
            .copied()
            .or_else(|| {
                modes
                    .iter()
                    .filter(|mode| {
                        let size = mode.size();
                        u32::from(size.0) == width && u32::from(size.1) == height
                    })
                    .min_by_key(|mode| mode.vrefresh().abs_diff(refresh))
                    .copied()
            })
            .or_else(|| default_mode(conn).ok())
    } else {
        default_mode(conn).ok()
    };

    mode.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "connector has no modes"))
}

fn pick_encoder_and_crtc(
    card: &Card,
    conn: &connector::Info,
) -> std::io::Result<(encoder::Info, crtc::Handle)> {
    let enc = conn.encoders().first().copied().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "connector has no encoders")
    })?;
    let enc_info = card
        .get_encoder(enc)
        .map_err(|e| std::io::Error::new(e.kind(), format!("get encoder: {e}")))?;
    let crtc = enc_info
        .crtc()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "encoder has no crtc"))?;

    Ok((enc_info, crtc))
}

fn pick_plane(
    card: &Card,
    res: &control::ResourceHandles,
    crtc: crtc::Handle,
    is_vc4: bool,
) -> std::io::Result<plane::Handle> {
    let planes = card
        .plane_handles()
        .map_err(|e| std::io::Error::new(e.kind(), format!("plane handles: {e}")))?;
    let mut chosen = None;

    for &p in planes.as_slice() {
        let info = card.get_plane(p).map_err(|e| {
            std::io::Error::new(e.kind(), format!("get plane {}: {e}", u32::from(p)))
        })?;
        let allowed = res.filter_crtcs(info.possible_crtcs());
        let supports_nv12 = info.formats().contains(&(buffer::DrmFourcc::Nv12 as u32));
        if !allowed.contains(&crtc) || !supports_nv12 {
            continue;
        }
        if is_vc4 && plane_is_overlay(card, p) {
            chosen = Some(p);
            break;
        }
        if chosen.is_none() {
            chosen = Some(p);
        }
    }

    chosen
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no suitable NV12 plane"))
}

fn build_display_info(
    card_path: &str,
    conn: &connector::Info,
    crtc: crtc::Handle,
    plane: plane::Handle,
    mode: control::Mode,
) -> DisplayInfo {
    DisplayInfo {
        card_path: card_path.to_string(),
        connector_id: u32::from(conn.handle()),
        connector_type: format!("{:?}", conn.interface()),
        crtc_id: u32::from(crtc),
        plane_id: u32::from(plane),
        mode: (
            u32::from(mode.size().0),
            u32::from(mode.size().1),
            mode.vrefresh(),
        ),
    }
}

fn scan_display_info(
    card_path: &str,
    preferred_mode: Option<(u32, u32, u32)>,
) -> std::io::Result<DisplayInfo> {
    let card = open_card(card_path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("open card: {e}")))?;
    let is_vc4 = driver_is_vc4(&card);
    enable_atomic(&card)
        .map_err(|e| std::io::Error::new(e.kind(), format!("enable atomic: {e}")))?;
    let res = card
        .resource_handles()
        .map_err(|e| std::io::Error::new(e.kind(), format!("get resources: {e}")))?;
    let conn = pick_connected_connector(&card, &res, is_vc4)?;
    let (_, crtc) = pick_encoder_and_crtc(&card, &conn)?;
    let mode = pick_mode(&conn, preferred_mode)?;
    let plane = pick_plane(&card, &res, crtc, is_vc4)?;

    Ok(build_display_info(card_path, &conn, crtc, plane, mode))
}

struct RawFbBundle {
    db: dumbbuf::DumbBuffer,
    fb: framebuffer::Handle,
}

struct RawDumbWrapper<'a> {
    db: &'a dumbbuf::DumbBuffer,
    width: u32,
    height: u32,
    format: PixelFormat,
    modifier: Option<DrmModifier>,
}

impl PlanarBuffer for RawDumbWrapper<'_> {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn format(&self) -> buffer::DrmFourcc {
        self.format.fourcc()
    }

    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier
    }

    fn pitches(&self) -> [u32; 4] {
        let pitch = self.db.pitch();

        match self.format {
            PixelFormat::I420 | PixelFormat::I422 | PixelFormat::I444 | PixelFormat::YV12 => {
                [pitch, pitch, pitch, 0]
            }
            PixelFormat::NV12 | PixelFormat::NV21 => [pitch, pitch, 0, 0],
            _ => [pitch, 0, 0, 0],
        }
    }

    fn handles(&self) -> [Option<buffer::Handle>; 4] {
        let handle = self.db.handle();

        match self.format.num_planes() {
            3 => [Some(handle), Some(handle), Some(handle), None],
            2 => [Some(handle), Some(handle), None, None],
            _ => [Some(handle), None, None, None],
        }
    }

    fn offsets(&self) -> [u32; 4] {
        let pitch = self.db.pitch();

        match self.format {
            PixelFormat::I420 => [
                0,
                pitch * self.height,
                pitch * self.height + pitch * (self.height / 2),
                0,
            ],
            PixelFormat::YV12 => [
                0,
                pitch * self.height,
                pitch * self.height + pitch * (self.height / 2),
                0,
            ],
            PixelFormat::I422 | PixelFormat::I444 => {
                [0, pitch * self.height, pitch * self.height * 2, 0]
            }
            PixelFormat::NV12 | PixelFormat::NV21 => [0, pitch * self.height, 0, 0],
            _ => [0, 0, 0, 0],
        }
    }
}

fn plane_is_primary(card: &Card, plane_handle: plane::Handle) -> bool {
    if let Ok(props) = card.get_properties(plane_handle) {
        for (handle, value) in props.iter() {
            if let Ok(info) = card.get_property(*handle)
                && info.name().to_bytes() == b"type"
            {
                return *value == 0;
            }
        }
    }

    false
}

fn pick_raw_mode(
    conn: &connector::Info,
    frame_width: u32,
    frame_height: u32,
    preferred_mode: Option<(u32, u32, u32)>,
) -> std::io::Result<control::Mode> {
    if let Some((width, height, _refresh)) = preferred_mode
        && (width != frame_width || height != frame_height)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "preferred mode {width}x{height} does not match raw frame size {frame_width}x{frame_height}"
            ),
        ));
    }

    let mut candidates = conn.modes().iter().copied().filter(|mode| {
        let size = mode.size();
        u32::from(size.0) == frame_width && u32::from(size.1) == frame_height
    });

    let first = candidates.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no display mode matches raw frame size {frame_width}x{frame_height}"),
        )
    })?;

    let mode = if let Some((_, _, refresh)) = preferred_mode {
        conn.modes()
            .iter()
            .filter(|mode| {
                let size = mode.size();
                u32::from(size.0) == frame_width && u32::from(size.1) == frame_height
            })
            .min_by_key(|mode| mode.vrefresh().abs_diff(refresh))
            .copied()
            .unwrap_or(first)
    } else {
        first
    };

    Ok(mode)
}

fn find_raw_plane_for_crtc(
    card: &Card,
    res: &control::ResourceHandles,
    crtc: crtc::Handle,
    format: PixelFormat,
    modifier: Option<DrmModifier>,
) -> std::io::Result<(plane::Handle, PixelFormat)> {
    let planes = card
        .plane_handles()
        .map_err(|e| std::io::Error::new(e.kind(), format!("plane handles: {e}")))?;
    let mut fallback = None;

    for &plane_handle in planes.as_slice() {
        let info = card.get_plane(plane_handle).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("get plane {}: {e}", u32::from(plane_handle)),
            )
        })?;
        let allowed = res.filter_crtcs(info.possible_crtcs());
        if !allowed.contains(&crtc) {
            continue;
        }
        if modifier.is_some() && !plane_is_primary(card, plane_handle) {
            continue;
        }

        let fourcc = format.fourcc() as u32;
        if info.formats().contains(&fourcc) {
            return Ok((plane_handle, format));
        }

        let fb_fourcc = format.fb_format().fourcc() as u32;
        if fallback.is_none() && info.formats().contains(&fb_fourcc) {
            fallback = Some(plane_handle);
        }
    }

    if let Some(plane_handle) = fallback {
        Ok((plane_handle, format.fb_format()))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no compatible plane found for raw format",
        ))
    }
}

fn copy_plane(src: &[u8], dst: &mut [u8], pitch: usize, width: usize, height: usize) {
    for row in 0..height {
        let dst_offset = row * pitch;
        let src_offset = row * width;
        dst[dst_offset..dst_offset + width].copy_from_slice(&src[src_offset..src_offset + width]);
    }
}

fn copy_i420_frame(src: &[u8], dst: &mut [u8], pitch: usize, width: usize, height: usize) {
    let mut offset = 0;
    copy_plane(
        &src[offset..offset + width * height],
        dst,
        pitch,
        width,
        height,
    );
    offset += width * height;

    let u_base = pitch * height;
    let chroma_size = (width / 2) * (height / 2);
    copy_plane(
        &src[offset..offset + chroma_size],
        &mut dst[u_base..],
        pitch,
        width / 2,
        height / 2,
    );
    offset += chroma_size;

    let v_base = u_base + pitch * (height / 2);
    copy_plane(
        &src[offset..offset + chroma_size],
        &mut dst[v_base..],
        pitch,
        width / 2,
        height / 2,
    );
}

fn copy_i422_frame(src: &[u8], dst: &mut [u8], pitch: usize, width: usize, height: usize) {
    let mut offset = 0;
    copy_plane(
        &src[offset..offset + width * height],
        dst,
        pitch,
        width,
        height,
    );
    offset += width * height;

    let u_base = pitch * height;
    let chroma_size = (width / 2) * height;
    copy_plane(
        &src[offset..offset + chroma_size],
        &mut dst[u_base..],
        pitch,
        width / 2,
        height,
    );
    offset += chroma_size;

    let v_base = u_base + pitch * height;
    copy_plane(
        &src[offset..offset + chroma_size],
        &mut dst[v_base..],
        pitch,
        width / 2,
        height,
    );
}

fn copy_i444_frame(src: &[u8], dst: &mut [u8], pitch: usize, width: usize, height: usize) {
    let mut offset = 0;
    copy_plane(
        &src[offset..offset + width * height],
        dst,
        pitch,
        width,
        height,
    );
    offset += width * height;

    let u_base = pitch * height;
    copy_plane(
        &src[offset..offset + width * height],
        &mut dst[u_base..],
        pitch,
        width,
        height,
    );
    offset += width * height;

    let v_base = u_base + pitch * height;
    copy_plane(
        &src[offset..offset + width * height],
        &mut dst[v_base..],
        pitch,
        width,
        height,
    );
}

fn copy_nv12_frame(src: &[u8], dst: &mut [u8], pitch: usize, width: usize, height: usize) {
    let mut offset = 0;
    copy_plane(
        &src[offset..offset + width * height],
        dst,
        pitch,
        width,
        height,
    );
    offset += width * height;

    let uv_base = pitch * height;
    let uv_size = width * (height / 2);
    copy_plane(
        &src[offset..offset + uv_size],
        &mut dst[uv_base..],
        pitch,
        width,
        height / 2,
    );
}

fn copy_yv12_frame(src: &[u8], dst: &mut [u8], pitch: usize, width: usize, height: usize) {
    let mut offset = 0;
    copy_plane(
        &src[offset..offset + width * height],
        dst,
        pitch,
        width,
        height,
    );
    offset += width * height;

    let v_base = pitch * height;
    let chroma_size = (width / 2) * (height / 2);
    copy_plane(
        &src[offset..offset + chroma_size],
        &mut dst[v_base..],
        pitch,
        width / 2,
        height / 2,
    );
    offset += chroma_size;

    let u_base = v_base + pitch * (height / 2);
    copy_plane(
        &src[offset..offset + chroma_size],
        &mut dst[u_base..],
        pitch,
        width / 2,
        height / 2,
    );
}

fn copy_i420_to_nv12(src: &[u8], dst: &mut [u8], pitch: usize, width: usize, height: usize) {
    let mut offset = 0;
    copy_plane(
        &src[offset..offset + width * height],
        dst,
        pitch,
        width,
        height,
    );
    offset += width * height;

    let chroma_size = (width / 2) * (height / 2);
    let u_plane = &src[offset..offset + chroma_size];
    offset += chroma_size;
    let v_plane = &src[offset..offset + chroma_size];
    let uv_base = pitch * height;

    for row in 0..(height / 2) {
        let dst_offset = uv_base + row * pitch;
        for column in 0..(width / 2) {
            let u = u_plane[row * (width / 2) + column];
            let v = v_plane[row * (width / 2) + column];
            let dst_index = dst_offset + 2 * column;
            dst[dst_index] = u;
            dst[dst_index + 1] = v;
        }
    }
}

fn copy_packed_frame(
    src: &[u8],
    dst: &mut [u8],
    pitch: usize,
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
) {
    let row_size = width * bytes_per_pixel;
    for row in 0..height {
        let dst_offset = row * pitch;
        let src_offset = row * row_size;
        dst[dst_offset..dst_offset + row_size]
            .copy_from_slice(&src[src_offset..src_offset + row_size]);
    }
}

fn copy_frame(
    src: &[u8],
    dst: &mut [u8],
    pitch: usize,
    width: usize,
    height: usize,
    format: PixelFormat,
) {
    match format {
        PixelFormat::I420 => copy_i420_frame(src, dst, pitch, width, height),
        PixelFormat::I422 => copy_i422_frame(src, dst, pitch, width, height),
        PixelFormat::I444 => copy_i444_frame(src, dst, pitch, width, height),
        PixelFormat::NV12 | PixelFormat::NV21 => copy_nv12_frame(src, dst, pitch, width, height),
        PixelFormat::YV12 => copy_yv12_frame(src, dst, pitch, width, height),
        PixelFormat::RGB => copy_packed_frame(src, dst, pitch, width, height, 3),
        PixelFormat::BGRA | PixelFormat::RGBA | PixelFormat::AYUV => {
            copy_packed_frame(src, dst, pitch, width, height, 4)
        }
        PixelFormat::YUY2 => copy_packed_frame(src, dst, pitch, width, height, 2),
    }
}

fn create_raw_dumb_and_fb(
    card: &Card,
    width: u32,
    height: u32,
    format: PixelFormat,
    modifier: Option<DrmModifier>,
) -> std::io::Result<RawFbBundle> {
    let buffer_height = format.buffer_height(height);
    let mut db = card.create_dumb_buffer((width, buffer_height), format.fourcc(), format.bpp())?;

    {
        let frame = vec![0u8; format.frame_size(width, height)];
        let pitch = db.pitch() as usize;
        let mut mapping = card.map_dumb_buffer(&mut db)?;
        copy_frame(
            &frame,
            mapping.as_mut(),
            pitch,
            width as usize,
            height as usize,
            format,
        );
    }

    let wrapper = RawDumbWrapper {
        db: &db,
        width,
        height,
        format,
        modifier,
    };
    let flags = if modifier.is_some() {
        FbCmd2Flags::MODIFIERS
    } else {
        FbCmd2Flags::empty()
    };
    let fb = card.add_planar_framebuffer(&wrapper, flags)?;

    Ok(RawFbBundle { db, fb })
}

fn create_mode_blob(
    card: &Card,
    mode: &control::Mode,
) -> std::io::Result<(property::Value<'static>, u64)> {
    let blob_value = card.create_property_blob(mode)?;
    let blob_id = match blob_value {
        property::Value::Blob(id) => id,
        _ => unreachable!(),
    };

    Ok((blob_value, blob_id))
}

fn build_raw_modeset_request(
    card: &Card,
    conn: &connector::Info,
    crtc: crtc::Handle,
    plane: plane::Handle,
    fb: framebuffer::Handle,
    mode: &control::Mode,
    mode_blob: property::Value<'static>,
) -> std::io::Result<AtomicModeReq> {
    let mut req = AtomicModeReq::new();
    let name = |value: &str| CString::new(value).unwrap();

    let conn_crtc = find_prop(card, conn.handle(), &name("CRTC_ID"))?;
    req.add_property(conn.handle(), conn_crtc, property::Value::CRTC(Some(crtc)));

    let crtc_mode = find_prop(card, crtc, &name("MODE_ID"))?;
    let crtc_active = find_prop(card, crtc, &name("ACTIVE"))?;
    req.add_property(crtc, crtc_mode, mode_blob);
    req.add_property(crtc, crtc_active, property::Value::Boolean(true));

    let plane_crtc = find_prop(card, plane, &name("CRTC_ID"))?;
    let plane_fb = find_prop(card, plane, &name("FB_ID"))?;
    let plane_src_x = find_prop(card, plane, &name("SRC_X"))?;
    let plane_src_y = find_prop(card, plane, &name("SRC_Y"))?;
    let plane_src_w = find_prop(card, plane, &name("SRC_W"))?;
    let plane_src_h = find_prop(card, plane, &name("SRC_H"))?;
    let plane_crtc_x = find_prop(card, plane, &name("CRTC_X"))?;
    let plane_crtc_y = find_prop(card, plane, &name("CRTC_Y"))?;
    let plane_crtc_w = find_prop(card, plane, &name("CRTC_W"))?;
    let plane_crtc_h = find_prop(card, plane, &name("CRTC_H"))?;

    req.add_property(plane, plane_crtc, property::Value::CRTC(Some(crtc)));
    req.add_property(plane, plane_fb, property::Value::Framebuffer(Some(fb)));

    let (width, height) = mode.size();
    let width = width as u32;
    let height = height as u32;
    req.add_property(plane, plane_src_x, property::Value::UnsignedRange(0));
    req.add_property(plane, plane_src_y, property::Value::UnsignedRange(0));
    req.add_property(
        plane,
        plane_src_w,
        property::Value::UnsignedRange((width as u64) << 16),
    );
    req.add_property(
        plane,
        plane_src_h,
        property::Value::UnsignedRange((height as u64) << 16),
    );
    req.add_property(plane, plane_crtc_x, property::Value::SignedRange(0));
    req.add_property(plane, plane_crtc_y, property::Value::SignedRange(0));
    req.add_property(
        plane,
        plane_crtc_w,
        property::Value::UnsignedRange(width as u64),
    );
    req.add_property(
        plane,
        plane_crtc_h,
        property::Value::UnsignedRange(height as u64),
    );

    Ok(req)
}

fn scan_raw_display_info(
    card_path: &str,
    config: RawDisplayConfig,
) -> std::io::Result<DisplayInfo> {
    let card = open_card(card_path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("open card: {e}")))?;
    let is_vc4 = driver_is_vc4(&card);
    enable_atomic(&card)
        .map_err(|e| std::io::Error::new(e.kind(), format!("enable atomic: {e}")))?;
    let res = card
        .resource_handles()
        .map_err(|e| std::io::Error::new(e.kind(), format!("get resources: {e}")))?;
    let conn = pick_connected_connector(&card, &res, is_vc4)?;
    let (_, crtc) = pick_encoder_and_crtc(&card, &conn)?;
    let mode = pick_raw_mode(
        &conn,
        config.frame_width,
        config.frame_height,
        config.preferred_mode,
    )?;
    let modifier = {
        #[cfg(feature = "rpi")]
        {
            if is_vc4 {
                Some(DrmModifier::from(DRM_FORMAT_MOD_BROADCOM_SAND128))
            } else {
                None
            }
        }
        #[cfg(not(feature = "rpi"))]
        {
            None
        }
    };
    let (plane, _fb_format) = find_raw_plane_for_crtc(&card, &res, crtc, config.format, modifier)?;

    Ok(build_display_info(card_path, &conn, crtc, plane, mode))
}

struct DisplayAttemptError {
    source: io::Error,
    desc: PrimeDesc,
}

impl DisplayAttemptError {
    fn into_parts(self) -> (io::Error, PrimeDesc) {
        (self.source, self.desc)
    }
}

impl DisplayInner {
    fn new(
        card_path: &str,
        preferred_mode: Option<(u32, u32, u32)>,
        listener: LocalPid,
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
        let (enc_info, crtc) = pick_encoder_and_crtc(&card, &conn)?;
        log!(
            "Selected encoder: id={}, type={:?}",
            u32::from(enc_info.handle()),
            enc_info.kind()
        );
        #[cfg(not(feature = "verbose"))]
        let _ = &enc_info;

        log!("Selected CRTC: id={}", u32::from(crtc));
        let mode = pick_mode(&conn, preferred_mode)?;
        log!(
            "Selected mode: {}x{}@{}",
            mode.size().0,
            mode.size().1,
            mode.vrefresh()
        );

        // -------- Plane selection --------
        let plane = pick_plane(&card, &res, crtc, is_vc4)?;
        log!(
            "Selected plane: id={}, type_val={:?}",
            u32::from(plane),
            plane_type_value(&card, plane)
        );

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

        // Optional props
        let prop_zpos = find_prop(&card, plane, &name("ZPOS")).ok();

        let info = build_display_info(card_path, &conn, crtc, plane, mode);

        Ok((
            Self {
                card,
                listener,
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
                prop_zpos,
                mode_blob: blob_id,
                setup: false,
                stale: None,
                in_flight: None,
            },
            info,
        ))
    }

    #[allow(clippy::result_large_err)]
    fn display(&mut self, desc: PrimeDesc) -> Result<(), DisplayAttemptError> {
        let width = desc.width as u64;
        let height = desc.height as u64;
        let submit_started = Instant::now();
        let trace_token = desc.trace_token.clone();

        let new_fb = match self.add_fb_from_prime_desc(&desc) {
            Ok(new_fb) => new_fb,
            Err(e) => {
                log!("Add fb from prime error: {}", e);
                if let Some(token) = trace_token.clone()
                    && token.sampled
                {
                    send_trace_event(
                        &self.listener,
                        native_submit_error(),
                        token,
                        Some(submit_started.elapsed().as_nanos() as u64),
                    );
                }
                return Err(DisplayAttemptError {
                    source: io::Error::other(e),
                    desc,
                });
            }
        };

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
            // Source coordinates (in 16.16 fixed point)
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
                property::Value::UnsignedRange(width << 16),
            );
            req.add_property(
                self.plane,
                self.prop_src_h,
                property::Value::UnsignedRange(height << 16),
            );
            // Destination on CRTC
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
                property::Value::UnsignedRange(width),
            );
            req.add_property(
                self.plane,
                self.prop_crtc_h,
                property::Value::UnsignedRange(height),
            );
            // Optional helpers: ZPOS and YUV color props if present
            if let Some(zpos) = self.prop_zpos {
                req.add_property(self.plane, zpos, property::Value::UnsignedRange(1));
            }
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
            eprintln!("atomic_commit error: {e:?}");
            let _ = self.card.destroy_framebuffer(new_fb.fb);
            let _ = close_unique_handles(&new_fb.handles, |h| self.card.close_buffer(h));
            if let Some(token) = trace_token.clone()
                && token.sampled
            {
                send_trace_event(
                    &self.listener,
                    native_submit_error(),
                    token,
                    Some(submit_started.elapsed().as_nanos() as u64),
                );
            }
            return Err(DisplayAttemptError {
                source: std::io::Error::new(e.kind(), format!("atomic commit: {e}")),
                desc,
            });
        };

        if let Some(stale_fb) = self.stale.take() {
            log!("Dropping stale framebuffer {:?}\n", stale_fb.fbwh);
            // 1) Drop the KMS FB
            let _ = self.card.destroy_framebuffer(stale_fb.fbwh.fb);
            // 2) Close handles associated with the stale framebuffer
            let _ = close_unique_handles(&stale_fb.fbwh.handles, |h| self.card.close_buffer(h));
            self.on_displayed(
                stale_fb.keepalive,
                &stale_fb.owner_pid,
                stale_fb.trace_token,
                native_release_displayed(),
            );
        }

        self.stale = self.in_flight.take();
        self.in_flight = Some(DisplayUnit {
            fbwh: new_fb,
            keepalive: desc.keepalive,
            owner_pid: desc.owner_pid,
            trace_token: desc.trace_token.clone(),
        });
        if let Some(token) = desc.trace_token
            && token.sampled
        {
            send_trace_event(
                &self.listener,
                native_submit(),
                token,
                Some(submit_started.elapsed().as_nanos() as u64),
            );
        }
        Ok(())
    }

    /// Create a KMS framebuffer from your PrimeDesc using drm-rs 0.14.
    fn add_fb_from_prime_desc(&mut self, prime: &PrimeDesc) -> Result<FbWithHandles> {
        if prime.planes.is_empty() {
            bail!("PrimeDesc has no planes");
        }

        // Import planes (dmabuf -> GEM)
        let mut obj_handles: [Option<GemHandle>; 4] = [None, None, None, None];
        let mut handles: [Option<GemHandle>; 4] = [None, None, None, None];
        let mut pitches: [u32; 4] = [0; 4];
        let mut offsets: [u32; 4] = [0; 4];

        // Collect per-plane modifiers, then collapse to one
        let mut mods_raw: [Option<u64>; 4] = [None, None, None, None];

        let result = (|| -> Result<FbWithHandles> {
            for i in 0..prime.objects.len() {
                let o = &prime.objects[i];
                let gem = self
                    .card
                    .prime_fd_to_buffer(o.fd.as_fd())
                    .with_context(|| format!("prime_fd_to_buffer failed on plane {}", i))?;
                obj_handles[i] = Some(gem);
                mods_raw[i] = o.modifier;
            }

            for i in 0..prime.planes.len() {
                let p = &prime.planes[i];
                handles[i] = obj_handles[p.obj_idx as usize];
                pitches[i] = p.pitch;
                offsets[i] = p.offset;
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
            let modifier = common_mod.map(DrmModifier::from);
            // On RPi/VC4 we DO NOT force a SAND modifier automatically anymore.

            // Build PlanarBuffer
            let pb = PrimePlanarBuf {
                w: prime.width,
                h: prime.height,
                fourcc: prime.format.0,
                pitches,
                offsets,
                handles,
                modifier,
            };

            // Use MODIFIERS flag iff we actually have one **and** the device reports support
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
        })();

        if result.is_err() {
            let _ = close_unique_handles(&obj_handles, |h| self.card.close_buffer(h));
        }

        result
    }

    fn on_displayed(
        &self,
        ka: FrozenTerm,
        owner_pid: &LocalPid,
        trace_token: Option<TraceToken>,
        stage: Atom,
    ) {
        if let Some(token) = trace_token
            && token.sampled
        {
            send_trace_event(&self.listener, stage, token, None);
        }
        send_keepalive_message(ka, owner_pid);
    }
}

impl Drop for DisplayInner {
    fn drop(&mut self) {
        if let Some(du) = self.stale.take() {
            let _ = self.card.destroy_framebuffer(du.fbwh.fb);
            let _ = close_unique_handles(&du.fbwh.handles, |h| self.card.close_buffer(h));
            self.on_displayed(
                du.keepalive,
                &du.owner_pid,
                du.trace_token,
                native_release_displayed(),
            );
        }
        if let Some(du) = self.in_flight.take() {
            let _ = self.card.destroy_framebuffer(du.fbwh.fb);
            let _ = close_unique_handles(&du.fbwh.handles, |h| self.card.close_buffer(h));
            self.on_displayed(
                du.keepalive,
                &du.owner_pid,
                du.trace_token,
                native_release_displayed(),
            );
        }
        let _ = self.card.destroy_property_blob(self.mode_blob);
    }
}

struct Display {
    queue: Arc<DisplayQueue>,
    handle: Option<thread::JoinHandle<()>>,
}

struct DisplayQueue {
    state: Mutex<DisplayQueueState>,
    wakeup: Condvar,
}

struct DisplayQueueState {
    pending: Option<PrimeDesc>,
    releases: Vec<PrimeDesc>,
    closed: bool,
}

struct ActiveDisplay {
    inner: DisplayInner,
    info: DisplayInfo,
}

impl DisplayQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(DisplayQueueState {
                pending: None,
                releases: Vec::new(),
                closed: false,
            }),
            wakeup: Condvar::new(),
        }
    }

    fn enqueue(&self, desc: PrimeDesc) -> std::io::Result<()> {
        let mut state = self.state.lock().map_err(|_| io::Error::other("lock"))?;
        if state.closed {
            return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        }

        if let Some(desc) = state.pending.replace(desc) {
            state.releases.push(desc);
        }

        self.wakeup.notify_one();
        Ok(())
    }

    fn take_pending(&self) -> std::io::Result<Option<PrimeDesc>> {
        let mut state = self.state.lock().map_err(|_| io::Error::other("lock"))?;
        Ok(state.pending.take())
    }

    fn restore_or_release(&self, desc: PrimeDesc) -> std::io::Result<()> {
        let mut state = self.state.lock().map_err(|_| io::Error::other("lock"))?;
        if state.closed || state.pending.is_some() {
            state.releases.push(desc);
        } else {
            state.pending = Some(desc);
        }

        self.wakeup.notify_one();
        Ok(())
    }

    fn take_releases(&self) -> std::io::Result<Vec<PrimeDesc>> {
        let mut state = self.state.lock().map_err(|_| io::Error::other("lock"))?;
        Ok(std::mem::take(&mut state.releases))
    }

    fn wait_timeout(&self, timeout: Duration) -> std::io::Result<bool> {
        let state = self.state.lock().map_err(|_| io::Error::other("lock"))?;
        if state.closed || state.pending.is_some() || !state.releases.is_empty() {
            return Ok(state.closed);
        }

        let (state, _) = self
            .wakeup
            .wait_timeout(state, timeout)
            .map_err(|_| io::Error::other("lock"))?;
        Ok(state.closed)
    }

    fn close(&self) -> std::io::Result<()> {
        let mut state = self.state.lock().map_err(|_| io::Error::other("lock"))?;
        state.closed = true;

        self.wakeup.notify_all();
        Ok(())
    }

    fn is_closed(&self) -> std::io::Result<bool> {
        let state = self.state.lock().map_err(|_| io::Error::other("lock"))?;
        Ok(state.closed)
    }
}

impl Display {
    fn release_queued_descs(queue: &DisplayQueue, listener: &LocalPid) -> bool {
        match queue.take_releases() {
            Ok(descs) => {
                for desc in descs {
                    release_prime_desc(desc, listener, native_release_replaced());
                }
                true
            }
            Err(_) => false,
        }
    }

    fn release_pending_desc(queue: &DisplayQueue, listener: &LocalPid) -> bool {
        match queue.take_pending() {
            Ok(Some(desc)) => {
                release_prime_desc(desc, listener, native_release_pending());
                true
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    fn new(
        card_path: &str,
        preferred_mode: Option<(u32, u32, u32)>,
        listener: LocalPid,
    ) -> std::io::Result<(Self, Option<DisplayInfo>)> {
        let queue = Arc::new(DisplayQueue::new());
        let (active, info, wait_reason) =
            match DisplayInner::new(card_path, preferred_mode, listener) {
                Ok((inner, info)) => (
                    Some(ActiveDisplay {
                        inner,
                        info: info.clone(),
                    }),
                    Some(info),
                    None,
                ),
                Err(err) => (None, None, Some(err.to_string())),
            };

        let queue_clone = Arc::clone(&queue);
        let card_path = card_path.to_owned();
        let handle = thread::spawn(move || {
            Self::run(
                queue_clone,
                listener,
                card_path,
                preferred_mode,
                active,
                wait_reason,
            )
        });

        Ok((
            Self {
                queue,
                handle: Some(handle),
            },
            info,
        ))
    }

    fn display(&self, desc: PrimeDesc) -> std::io::Result<()> {
        self.queue.enqueue(desc)
    }

    fn run(
        queue: Arc<DisplayQueue>,
        listener: LocalPid,
        card_path: String,
        preferred_mode: Option<(u32, u32, u32)>,
        mut active: Option<ActiveDisplay>,
        initial_wait_reason: Option<String>,
    ) {
        let reconnect_interval = Duration::from_millis(250);
        let hotplug_interval = Duration::from_millis(750);
        let mut next_connect_attempt = Instant::now();
        let mut next_hotplug_check = Instant::now() + hotplug_interval;

        if active.is_none() {
            notify_display_waiting(
                &listener,
                initial_wait_reason.unwrap_or_else(|| "display unavailable".to_string()),
            );
        }

        loop {
            if !Self::release_queued_descs(&queue, &listener) {
                break;
            }

            match queue.is_closed() {
                Ok(true) => {
                    if !Self::release_pending_desc(&queue, &listener) {
                        break;
                    }

                    let _ = Self::release_queued_descs(&queue, &listener);
                    break;
                }
                Ok(false) => {}
                Err(_) => break,
            }

            if let Some(current) = active.as_mut() {
                let mut disconnect_reason = None;

                if Instant::now() >= next_hotplug_check {
                    match scan_display_info(&card_path, preferred_mode) {
                        Ok(info) if info == current.info => {
                            next_hotplug_check = Instant::now() + hotplug_interval;
                        }
                        Ok(_) => {
                            disconnect_reason = Some("display topology changed".to_string());
                        }
                        Err(err) => {
                            disconnect_reason = Some(err.to_string());
                        }
                    }
                }

                if disconnect_reason.is_none() {
                    match queue.take_pending() {
                        Ok(Some(desc)) => match current.inner.display(desc) {
                            Ok(()) => continue,
                            Err(err) => {
                                let (err, desc) = err.into_parts();
                                let _ = queue.restore_or_release(desc);
                                disconnect_reason = Some(err.to_string());
                            }
                        },
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }

                if let Some(reason) = disconnect_reason {
                    notify_display_disconnected(&listener, reason);
                    active = None;
                    next_connect_attempt = Instant::now();
                    continue;
                }

                let timeout = next_hotplug_check.saturating_duration_since(Instant::now());
                match queue.wait_timeout(timeout) {
                    Ok(true) | Err(_) => continue,
                    Ok(false) => continue,
                }
            }

            if Instant::now() >= next_connect_attempt {
                match DisplayInner::new(&card_path, preferred_mode, listener) {
                    Ok((inner, info)) => {
                        notify_display_connected(&listener, info.clone());
                        active = Some(ActiveDisplay { inner, info });
                        next_hotplug_check = Instant::now() + hotplug_interval;
                        continue;
                    }
                    Err(_) => {
                        next_connect_attempt = Instant::now() + reconnect_interval;
                    }
                }
            }

            let timeout = next_connect_attempt.saturating_duration_since(Instant::now());
            match queue.wait_timeout(timeout) {
                Ok(true) | Err(_) => continue,
                Ok(false) => {}
            }
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.queue.close();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone, Copy)]
struct RawDisplayConfig {
    format: PixelFormat,
    frame_width: u32,
    frame_height: u32,
    preferred_mode: Option<(u32, u32, u32)>,
}

impl RawDisplayConfig {
    fn frame_size(self) -> usize {
        self.format.frame_size(self.frame_width, self.frame_height)
    }
}

struct RawActiveDisplay {
    card: Card,
    blob_id: u64,
    buffers: Vec<RawFbBundle>,
    plane: plane::Handle,
    prop_fb: property::Handle,
    prop_crtc: property::Handle,
    crtc: crtc::Handle,
    width: u32,
    height: u32,
    format: PixelFormat,
    fb_format: PixelFormat,
    current: usize,
}

impl RawActiveDisplay {
    fn new(card_path: &str, config: RawDisplayConfig) -> std::io::Result<(Self, DisplayInfo)> {
        let format = config.format;
        let frame_width = config.frame_width;
        let frame_height = config.frame_height;
        let preferred_mode = config.preferred_mode;
        let card = open_card(card_path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("open card: {e}")))?;
        let is_vc4 = driver_is_vc4(&card);
        enable_atomic(&card)
            .map_err(|e| std::io::Error::new(e.kind(), format!("enable atomic: {e}")))?;
        let res = card
            .resource_handles()
            .map_err(|e| std::io::Error::new(e.kind(), format!("get resources: {e}")))?;
        let conn = pick_connected_connector(&card, &res, is_vc4)?;
        let (_enc_info, crtc) = pick_encoder_and_crtc(&card, &conn)?;
        let mode = pick_raw_mode(&conn, frame_width, frame_height, preferred_mode)?;
        let (mode_width, mode_height) = mode.size();
        let (mode_width, mode_height) = (u32::from(mode_width), u32::from(mode_height));

        let modifier = {
            #[cfg(feature = "rpi")]
            {
                if is_vc4 {
                    Some(DrmModifier::from(DRM_FORMAT_MOD_BROADCOM_SAND128))
                } else {
                    None
                }
            }
            #[cfg(not(feature = "rpi"))]
            {
                None
            }
        };

        let (plane, fb_format) = find_raw_plane_for_crtc(&card, &res, crtc, format, modifier)?;
        let mut buffers = Vec::with_capacity(3);
        for _ in 0..3 {
            buffers.push(create_raw_dumb_and_fb(
                &card,
                mode_width,
                mode_height,
                fb_format,
                modifier,
            )?);
        }

        let (mode_blob, blob_id) = create_mode_blob(&card, &mode)?;
        let req =
            build_raw_modeset_request(&card, &conn, crtc, plane, buffers[0].fb, &mode, mode_blob)?;
        card.atomic_commit(control::AtomicCommitFlags::ALLOW_MODESET, req)?;

        let name = |value: &str| CString::new(value).unwrap();
        let prop_fb = find_prop(&card, plane, &name("FB_ID"))?;
        let prop_crtc = find_prop(&card, plane, &name("CRTC_ID"))?;
        let info = build_display_info(card_path, &conn, crtc, plane, mode);

        Ok((
            Self {
                card,
                blob_id,
                buffers,
                plane,
                prop_fb,
                prop_crtc,
                crtc,
                width: mode_width,
                height: mode_height,
                format,
                fb_format,
                current: 0,
            },
            info,
        ))
    }

    fn choose_writable_slot(
        &self,
        pending_slot: Option<usize>,
        committing_slot: Option<usize>,
    ) -> std::io::Result<usize> {
        if let Some(slot) = pending_slot {
            return Ok(slot);
        }

        for slot in 0..self.buffers.len() {
            if slot != self.current && Some(slot) != committing_slot {
                return Ok(slot);
            }
        }

        Err(std::io::Error::other(
            "no writable raw scanout buffer available",
        ))
    }

    fn upload_frame(&mut self, slot: usize, frame: &[u8]) -> std::io::Result<()> {
        if frame.len() != self.format.frame_size(self.width, self.height) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid raw frame size",
            ));
        }

        let buf = self.buffers.get_mut(slot).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid raw buffer slot")
        })?;
        let pitch = buf.db.pitch() as usize;
        let mut mapping = self.card.map_dumb_buffer(&mut buf.db)?;

        if self.format == self.fb_format {
            copy_frame(
                frame,
                mapping.as_mut(),
                pitch,
                self.width as usize,
                self.height as usize,
                self.format,
            );
            Ok(())
        } else if self.format == PixelFormat::I420 && self.fb_format == PixelFormat::NV12 {
            copy_i420_to_nv12(
                frame,
                mapping.as_mut(),
                pitch,
                self.width as usize,
                self.height as usize,
            );
            Ok(())
        } else {
            Err(std::io::Error::other(
                "unsupported raw framebuffer conversion",
            ))
        }
    }

    fn commit_slot(&mut self, slot: usize) -> std::io::Result<()> {
        let fb = self.buffers.get(slot).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid raw buffer slot")
        })?;
        let mut req = AtomicModeReq::new();
        req.add_property(
            self.plane,
            self.prop_crtc,
            property::Value::CRTC(Some(self.crtc)),
        );
        req.add_property(
            self.plane,
            self.prop_fb,
            property::Value::Framebuffer(Some(fb.fb)),
        );
        self.card
            .atomic_commit(control::AtomicCommitFlags::empty(), req)?;
        self.current = slot;
        Ok(())
    }
}

impl Drop for RawActiveDisplay {
    fn drop(&mut self) {
        let _ = self.card.destroy_property_blob(self.blob_id);
        for RawFbBundle { db, fb } in self.buffers.drain(..) {
            let _ = self.card.destroy_framebuffer(fb);
            let _ = self.card.destroy_dumb_buffer(db);
        }
    }
}

struct RawDisplay {
    shared: Arc<RawDisplayShared>,
    handle: Option<thread::JoinHandle<()>>,
    frame_size: usize,
}

struct RawDisplayShared {
    state: Mutex<RawDisplayState>,
    wakeup: Condvar,
}

struct RawDisplayState {
    active: Option<Arc<Mutex<RawActiveDisplay>>>,
    info: Option<DisplayInfo>,
    pending_slot: Option<usize>,
    disconnected_frame: Option<Box<[u8]>>,
    committing_slot: Option<usize>,
    closed: bool,
}

impl RawDisplayShared {
    fn new(active: Option<RawActiveDisplay>, info: Option<DisplayInfo>) -> Self {
        Self {
            state: Mutex::new(RawDisplayState {
                active: active.map(|active| Arc::new(Mutex::new(active))),
                info,
                pending_slot: None,
                disconnected_frame: None,
                committing_slot: None,
                closed: false,
            }),
            wakeup: Condvar::new(),
        }
    }

    fn wait_timeout(&self, timeout: Duration) -> std::io::Result<bool> {
        let state = self.state.lock().map_err(|_| io::Error::other("lock"))?;
        if state.closed || state.pending_slot.is_some() || state.disconnected_frame.is_some() {
            return Ok(state.closed);
        }

        let (state, _) = self
            .wakeup
            .wait_timeout(state, timeout)
            .map_err(|_| io::Error::other("lock"))?;
        Ok(state.closed)
    }

    fn close(&self) -> std::io::Result<()> {
        let mut state = self.state.lock().map_err(|_| io::Error::other("lock"))?;
        state.closed = true;
        self.wakeup.notify_all();
        Ok(())
    }

    fn is_closed(&self) -> std::io::Result<bool> {
        let state = self.state.lock().map_err(|_| io::Error::other("lock"))?;
        Ok(state.closed)
    }
}

impl RawDisplay {
    fn new(
        card_path: &str,
        config: RawDisplayConfig,
        listener: LocalPid,
    ) -> std::io::Result<(Self, Option<DisplayInfo>)> {
        let (active, info, wait_reason) = match RawActiveDisplay::new(card_path, config) {
            Ok((active, info)) => (Some(active), Some(info), None),
            Err(err) => (None, None, Some(err.to_string())),
        };
        let frame_size = config.frame_size();
        let shared = Arc::new(RawDisplayShared::new(active, info.clone()));
        let shared_clone = Arc::clone(&shared);
        let card_path = card_path.to_owned();
        let handle = thread::spawn(move || {
            Self::run(shared_clone, listener, card_path, config, wait_reason)
        });

        Ok((
            Self {
                shared,
                handle: Some(handle),
                frame_size,
            },
            info,
        ))
    }

    fn display_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        if frame.len() != self.frame_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid raw frame size",
            ));
        }

        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| io::Error::other("lock"))?;
        if state.closed {
            return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        }

        if let Some(active) = state.active.as_ref().cloned() {
            let slot = {
                let mut active = active.lock().map_err(|_| io::Error::other("lock"))?;
                let slot =
                    active.choose_writable_slot(state.pending_slot, state.committing_slot)?;
                active.upload_frame(slot, frame)?;
                slot
            };
            state.pending_slot = Some(slot);
            state.disconnected_frame = None;
        } else {
            state.disconnected_frame = Some(frame.to_vec().into_boxed_slice());
        }

        self.shared.wakeup.notify_one();
        Ok(())
    }

    fn run(
        shared: Arc<RawDisplayShared>,
        listener: LocalPid,
        card_path: String,
        config: RawDisplayConfig,
        initial_wait_reason: Option<String>,
    ) {
        let reconnect_interval = Duration::from_millis(250);
        let hotplug_interval = Duration::from_millis(750);
        let mut next_connect_attempt = Instant::now();
        let mut next_hotplug_check = Instant::now() + hotplug_interval;

        if matches!(shared.is_closed(), Ok(true)) {
            return;
        }

        if matches!(
            shared
                .state
                .lock()
                .map(|state| state.active.is_none())
                .map_err(|_| io::Error::other("lock")),
            Ok(true)
        ) {
            notify_display_waiting(
                &listener,
                initial_wait_reason.unwrap_or_else(|| "display unavailable".to_string()),
            );
        }

        loop {
            match shared.is_closed() {
                Ok(true) | Err(_) => break,
                Ok(false) => {}
            }

            let active = match shared.state.lock() {
                Ok(state) => state.active.clone(),
                Err(_) => break,
            };

            if let Some(active) = active {
                let mut disconnect_reason = None;

                if Instant::now() >= next_hotplug_check {
                    let current_info = match shared.state.lock() {
                        Ok(state) => state.info.clone(),
                        Err(_) => break,
                    };

                    match scan_raw_display_info(&card_path, config) {
                        Ok(info) if current_info.as_ref() == Some(&info) => {
                            next_hotplug_check = Instant::now() + hotplug_interval;
                        }
                        Ok(_) => {
                            disconnect_reason = Some("display topology changed".to_string());
                        }
                        Err(err) => {
                            disconnect_reason = Some(err.to_string());
                        }
                    }
                }

                if disconnect_reason.is_none() {
                    let maybe_disconnected_frame = match shared.state.lock() {
                        Ok(mut state) => {
                            if state.pending_slot.is_none() {
                                state.disconnected_frame.take()
                            } else {
                                None
                            }
                        }
                        Err(_) => break,
                    };

                    if let Some(frame) = maybe_disconnected_frame {
                        let mut state = match shared.state.lock() {
                            Ok(state) => state,
                            Err(_) => break,
                        };
                        let upload_result = {
                            let mut active = match active.lock() {
                                Ok(active) => active,
                                Err(_) => break,
                            };
                            let slot = match active
                                .choose_writable_slot(state.pending_slot, state.committing_slot)
                            {
                                Ok(slot) => slot,
                                Err(err) => {
                                    disconnect_reason = Some(err.to_string());
                                    0
                                }
                            };

                            if disconnect_reason.is_none() {
                                active.upload_frame(slot, &frame).map(|_| slot)
                            } else {
                                Ok(slot)
                            }
                        };

                        match upload_result {
                            Ok(slot) if disconnect_reason.is_none() => {
                                state.pending_slot = Some(slot);
                            }
                            Ok(_) => {
                                state.disconnected_frame = Some(frame);
                            }
                            Err(err) => {
                                state.disconnected_frame = Some(frame);
                                disconnect_reason = Some(err.to_string());
                            }
                        }
                    }
                }

                if disconnect_reason.is_none() {
                    let slot_to_commit = match shared.state.lock() {
                        Ok(mut state) => {
                            if state.committing_slot.is_none() {
                                if let Some(slot) = state.pending_slot.take() {
                                    state.committing_slot = Some(slot);
                                    Some(slot)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        Err(_) => break,
                    };

                    if let Some(slot) = slot_to_commit {
                        let commit_result = match active.lock() {
                            Ok(mut active) => active.commit_slot(slot),
                            Err(_) => Err(io::Error::other("lock")),
                        };

                        let mut state = match shared.state.lock() {
                            Ok(state) => state,
                            Err(_) => break,
                        };
                        state.committing_slot = None;

                        match commit_result {
                            Ok(()) => {
                                next_hotplug_check = Instant::now() + hotplug_interval;
                                continue;
                            }
                            Err(err) => {
                                state.active = None;
                                state.info = None;
                                state.pending_slot = None;
                                disconnect_reason = Some(err.to_string());
                            }
                        }
                    }
                }

                if let Some(reason) = disconnect_reason {
                    if let Ok(mut state) = shared.state.lock() {
                        state.active = None;
                        state.info = None;
                        state.pending_slot = None;
                        state.committing_slot = None;
                    }
                    notify_display_disconnected(&listener, reason);
                    next_connect_attempt = Instant::now();
                    continue;
                }

                let timeout = next_hotplug_check.saturating_duration_since(Instant::now());
                match shared.wait_timeout(timeout) {
                    Ok(true) | Err(_) => continue,
                    Ok(false) => continue,
                }
            }

            if Instant::now() >= next_connect_attempt {
                match RawActiveDisplay::new(&card_path, config) {
                    Ok((active, info)) => {
                        if let Ok(mut state) = shared.state.lock() {
                            state.active = Some(Arc::new(Mutex::new(active)));
                            state.info = Some(info.clone());
                            state.pending_slot = None;
                            state.committing_slot = None;
                        } else {
                            break;
                        }
                        notify_display_connected(&listener, info);
                        next_hotplug_check = Instant::now() + hotplug_interval;
                        continue;
                    }
                    Err(_) => {
                        next_connect_attempt = Instant::now() + reconnect_interval;
                    }
                }
            }

            let timeout = next_connect_attempt.saturating_duration_since(Instant::now());
            match shared.wait_timeout(timeout) {
                Ok(true) | Err(_) => continue,
                Ok(false) => {}
            }
        }
    }
}

impl Drop for RawDisplay {
    fn drop(&mut self) {
        let _ = self.shared.close();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

enum ManagedDisplay {
    Prime(Display),
    Raw(RawDisplay),
}

struct DisplayRes(Mutex<Option<ManagedDisplay>>);

unsafe impl Send for DisplayRes {}
unsafe impl Sync for DisplayRes {}

#[rustler::resource_impl]
impl rustler::Resource for DisplayRes {}

fn nif_err<E: std::fmt::Display>(e: E) -> rustler::Error {
    rustler::Error::Term(Box::new(format!("{e}")))
}

#[rustler::nif]
fn init_display(
    card_path: String,
    preferred_mode: Option<(u32, u32, u32)>,
    listener: LocalPid,
) -> NifResult<(Atom, Option<DisplayInfo>, ResourceArc<DisplayRes>)> {
    let path = if card_path.is_empty() {
        find_vc4_card().map_err(nif_err)?
    } else {
        card_path
    };
    let (display, info) = Display::new(&path, preferred_mode, listener).map_err(nif_err)?;
    Ok((
        ok(),
        info,
        ResourceArc::new(DisplayRes(Mutex::new(Some(ManagedDisplay::Prime(display))))),
    ))
}

#[rustler::nif]
fn init_raw_display<'a>(
    env: Env<'a>,
    card_path: String,
    pixel_format: Atom,
    frame_width: u32,
    frame_height: u32,
    preferred_mode: Option<(u32, u32, u32)>,
    listener: LocalPid,
) -> NifResult<(Atom, Option<DisplayInfo>, ResourceArc<DisplayRes>)> {
    let path = if card_path.is_empty() {
        find_vc4_card().map_err(nif_err)?
    } else {
        card_path
    };
    let pixel_format = pixel_format
        .to_term(env)
        .atom_to_string()
        .map_err(|e| nif_err(format!("{e:?}")))?;
    let pixel_format =
        PixelFormat::from_str(&pixel_format).ok_or_else(|| nif_err("unknown pixel format"))?;
    let config = RawDisplayConfig {
        format: pixel_format,
        frame_width,
        frame_height,
        preferred_mode,
    };
    let (display, info) = RawDisplay::new(&path, config, listener).map_err(nif_err)?;
    Ok((
        ok(),
        info,
        ResourceArc::new(DisplayRes(Mutex::new(Some(ManagedDisplay::Raw(display))))),
    ))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn display_prime(res: ResourceArc<DisplayRes>, desc: PrimeDesc) -> NifResult<Atom> {
    let mut guard = res.0.lock().map_err(|_| nif_err("lock"))?;
    if let Some(display) = guard.as_mut() {
        match display {
            ManagedDisplay::Prime(display) => {
                let res = display.display(desc);
                if let Err(err) = res {
                    let _ = guard.take();
                    Err(nif_err(err))
                } else {
                    Ok(ok())
                }
            }
            ManagedDisplay::Raw(_) => Err(nif_err("display kind mismatch")),
        }
    } else {
        Err(nif_err("display closed"))
    }
}

#[rustler::nif(schedule = "DirtyCpu")]
fn display_frame(res: ResourceArc<DisplayRes>, frame: Binary) -> NifResult<Atom> {
    let mut guard = res.0.lock().map_err(|_| nif_err("lock"))?;
    if let Some(display) = guard.as_mut() {
        match display {
            ManagedDisplay::Prime(_) => Err(nif_err("display kind mismatch")),
            ManagedDisplay::Raw(display) => {
                let res = display.display_frame(frame.as_slice());
                if let Err(err) = res {
                    let _ = guard.take();
                    Err(nif_err(err))
                } else {
                    Ok(ok())
                }
            }
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

rustler::init!("Elixir.Membrane.Display.Sink.Native");
