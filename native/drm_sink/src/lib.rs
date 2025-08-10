use drm::ClientCapability; // caps to enable atomic / universal planes
use drm::Device as _; // for set_client_capability()
use drm::buffer::Buffer; // trait for .pitch()
use drm::control as dc;
use drm::control::FbCmd2Flags;
use drm::control::atomic::AtomicModeReq;
use drm::control::dumbbuffer as dumbbuf;
use drm::control::{AtomicCommitFlags, Device as _, connector, crtc, encoder, plane, property};
use rustler::{Binary, NifResult, ResourceArc};
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::Error;
use std::os::fd::AsFd;
use std::sync::Arc;

type Result<T> = std::result::Result<T, Error>;

// compile-time log toggle
#[cfg(feature = "verbose")]
macro_rules! log { ($($t:tt)*) => { println!($($t)*); } }
#[cfg(not(feature = "verbose"))]
macro_rules! log {
    ($($t:tt)*) => {};
}

/// Thin wrapper around a `File` so drm-rs trait impls apply to the card.
struct Card(File);
impl AsFd for Card {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl dc::Device for Card {}

/// Convenience helper to build an `Error` from a static string.
fn err(msg: &str) -> Error {
    Error::other(msg)
}

// ---------- helpers ----------
/// Enable universal planes and atomic modesetting capabilities.
///
/// These caps must be set before querying any plane or property state.
fn enable_atomic_caps(card: &Card) -> Result<()> {
    card.set_client_capability(ClientCapability::UniversalPlanes, true)?;
    card.set_client_capability(ClientCapability::Atomic, true)?;
    log!("Enabled client caps: UNIVERSAL_PLANES + ATOMIC");
    Ok(())
}

/// Open the DRM device at `path` for read/write access.
fn open_card(path: &str) -> Result<Card> {
    log!("Opening DRM device: {path}");
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    Ok(Card(file))
}

/// Fetch basic DRM resources (connectors, CRTCs, etc.) from the device.
fn get_resources(card: &Card) -> Result<dc::ResourceHandles> {
    card.resource_handles()
}

/// Pick the first connected connector that has at least one mode.
fn pick_connected_connector(card: &Card, res: &dc::ResourceHandles) -> Result<connector::Info> {
    for &conn_h in res.connectors() {
        let info = card.get_connector(conn_h, true)?;
        if info.state() == connector::State::Connected && !info.modes().is_empty() {
            log!(
                "Selected connector: id={}, type={:?}, modes={}",
                u32::from(info.handle()),
                info.interface(),
                info.modes().len()
            );
            return Ok(info);
        }
    }
    Err(err("No connected connector"))
}

/// Given a connector, retrieve its first encoder and corresponding CRTC.
fn pick_encoder_and_crtc(
    card: &Card,
    conn: &connector::Info,
) -> Result<(encoder::Info, crtc::Handle)> {
    let enc_h = conn
        .encoders()
        .first()
        .copied()
        .ok_or_else(|| err("connector has no encoders"))?;
    let enc = card.get_encoder(enc_h)?;
    let crtc_h = enc.crtc().ok_or_else(|| err("encoder has no crtc"))?;
    log!(
        "Selected encoder: id={}, type={:?}, CRTC id={}",
        u32::from(enc.handle()),
        enc.kind(),
        u32::from(crtc_h)
    );
    Ok((enc, crtc_h))
}

/// Choose the first available mode from the connector's list.
fn pick_mode(conn: &connector::Info) -> Result<dc::Mode> {
    let mode = *conn
        .modes()
        .first()
        .ok_or_else(|| err("connector has no modes"))?;
    log!(
        "Mode: {}x{}@{}Hz",
        mode.size().0,
        mode.size().1,
        mode.vrefresh()
    );
    Ok(mode)
}

/// Convenience wrapper bundling a dumb buffer with its framebuffer handle.
struct FbBundle {
    db: dumbbuf::DumbBuffer,
    fb: dc::framebuffer::Handle,
}

/// Wrapper implementing `PlanarBuffer` for an NV12 dumb buffer.
struct Nv12Dumb<'a> {
    db: &'a dumbbuf::DumbBuffer,
    w: u32,
    h: u32,
}

impl<'a> drm::buffer::PlanarBuffer for Nv12Dumb<'a> {
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    fn format(&self) -> drm::buffer::DrmFourcc {
        drm::buffer::DrmFourcc::Nv12
    }
    fn modifier(&self) -> Option<drm::buffer::DrmModifier> {
        None
    }
    fn pitches(&self) -> [u32; 4] {
        [self.db.pitch(), self.db.pitch(), 0, 0]
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        [Some(self.db.handle()), Some(self.db.handle()), None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [0, self.db.pitch() * self.h, 0, 0]
    }
}

/// Copy a single NV12 frame into a mapped dumb buffer.
fn copy_nv12_frame(src: &[u8], dst: &mut [u8], pitch: usize, w: usize, h: usize) {
    let mut off = 0;
    for y in 0..h {
        let dst_off = y * pitch;
        dst[dst_off..dst_off + w].copy_from_slice(&src[off..off + w]);
        off += w;
    }
    let uv_base = pitch * h;
    for y in 0..(h / 2) {
        let dst_off = uv_base + y * pitch;
        dst[dst_off..dst_off + w].copy_from_slice(&src[off..off + w]);
        off += w;
    }
}

/// Create an NV12 dumb buffer, initialize it to zero, and register a framebuffer.
fn create_dumb_and_fb(card: &Card, w: u32, h: u32) -> Result<FbBundle> {
    // Allocate space for Y and interleaved UV planes.
    let db_height = h + h / 2;
    let mut db = card.create_dumb_buffer((w, db_height), drm::buffer::DrmFourcc::Nv12, 8)?;

    let pitch = db.pitch() as usize;
    let frame_size = (w as usize * h as usize * 3) / 2;
    {
        let frame = vec![0u8; frame_size];
        let mut mapping = card.map_dumb_buffer(&mut db)?;
        copy_nv12_frame(&frame, mapping.as_mut(), pitch, w as usize, h as usize);
        // mapping dropped here
    }

    let wrapper = Nv12Dumb { db: &db, w, h };
    let fb = card.add_planar_framebuffer(&wrapper, FbCmd2Flags::empty())?;
    log!("Created framebuffer id={}", u32::from(fb));

    Ok(FbBundle { db, fb })
}

/// Find the first plane compatible with the target CRTC.
fn find_plane_for_crtc(
    card: &Card,
    res: &dc::ResourceHandles,
    crtc_h: crtc::Handle,
) -> Result<plane::Handle> {
    let planes = card.plane_handles()?;
    for &ph in planes.as_slice() {
        let pinfo = card.get_plane(ph)?;
        // Use helper to turn filter into a list of allowed CRTCs.
        let allowed = res.filter_crtcs(pinfo.possible_crtcs());
        if allowed.contains(&crtc_h) {
            log!("Selected plane id={}", u32::from(ph));
            return Ok(ph);
        }
    }
    Err(err("No compatible plane found"))
}

/// Create a property blob encapsulating the chosen mode.
fn create_mode_blob(card: &Card, mode: &dc::Mode) -> Result<(property::Value<'static>, u64)> {
    let blob_val = card.create_property_blob(mode)?;
    let blob_id = match blob_val {
        property::Value::Blob(id) => id,
        _ => unreachable!(),
    };
    log!("Created mode blob id={}", blob_id);
    Ok((blob_val, blob_id))
}

/// Look up a property handle by name on a DRM object.
fn find_prop(card: &Card, obj: impl dc::ResourceHandle, name: &CStr) -> Result<property::Handle> {
    let props = card.get_properties(obj)?;
    for (handle, _raw) in props.iter() {
        let info = card.get_property(*handle)?;
        if info.name().to_bytes() == name.to_bytes() {
            return Ok(*handle);
        }
    }
    Err(err("property not found"))
}

/// Build an atomic modesetting request hooking the plane, CRTC, and connector together.
fn build_atomic_request(
    card: &Card,
    conn: &connector::Info,
    crtc_h: crtc::Handle,
    plane_h: plane::Handle,
    fb_h: dc::framebuffer::Handle,
    mode: &dc::Mode,
    mode_blob: property::Value<'static>,
) -> Result<AtomicModeReq> {
    let mut req = AtomicModeReq::new();
    let name = |s: &str| CString::new(s).unwrap();

    // Connector: attach CRTC
    let conn_crtc = find_prop(card, conn.handle(), &name("CRTC_ID"))?;
    req.add_property(
        conn.handle(),
        conn_crtc,
        property::Value::CRTC(Some(crtc_h)),
    );

    // CRTC: set mode + active
    let crtc_mode = find_prop(card, crtc_h, &name("MODE_ID"))?;
    let crtc_active = find_prop(card, crtc_h, &name("ACTIVE"))?;
    req.add_property(crtc_h, crtc_mode, mode_blob);
    req.add_property(crtc_h, crtc_active, property::Value::Boolean(true));

    // Plane: hook it up to CRTC+FB and set src/crtc rectangles
    let p_crtc = find_prop(card, plane_h, &name("CRTC_ID"))?;
    let p_fb = find_prop(card, plane_h, &name("FB_ID"))?;
    let p_src_x = find_prop(card, plane_h, &name("SRC_X"))?;
    let p_src_y = find_prop(card, plane_h, &name("SRC_Y"))?;
    let p_src_w = find_prop(card, plane_h, &name("SRC_W"))?;
    let p_src_h = find_prop(card, plane_h, &name("SRC_H"))?;
    let p_crtc_x = find_prop(card, plane_h, &name("CRTC_X"))?;
    let p_crtc_y = find_prop(card, plane_h, &name("CRTC_Y"))?;
    let p_crtc_w = find_prop(card, plane_h, &name("CRTC_W"))?;
    let p_crtc_h = find_prop(card, plane_h, &name("CRTC_H"))?;

    req.add_property(plane_h, p_crtc, property::Value::CRTC(Some(crtc_h)));
    req.add_property(plane_h, p_fb, property::Value::Framebuffer(Some(fb_h)));

    let (w16, h16) = mode.size();
    let (w, h) = (w16 as u32, h16 as u32);
    req.add_property(plane_h, p_src_x, property::Value::UnsignedRange(0));
    req.add_property(plane_h, p_src_y, property::Value::UnsignedRange(0));
    req.add_property(
        plane_h,
        p_src_w,
        property::Value::UnsignedRange((w as u64) << 16),
    );
    req.add_property(
        plane_h,
        p_src_h,
        property::Value::UnsignedRange((h as u64) << 16),
    );
    req.add_property(plane_h, p_crtc_x, property::Value::SignedRange(0));
    req.add_property(plane_h, p_crtc_y, property::Value::SignedRange(0));
    req.add_property(plane_h, p_crtc_w, property::Value::UnsignedRange(w as u64));
    req.add_property(plane_h, p_crtc_h, property::Value::UnsignedRange(h as u64));

    Ok(req)
}

/// Commit the previously built atomic request to program the display.
fn commit_atomic(card: &Card, req: AtomicModeReq) -> Result<()> {
    log!("Committing atomic modeset...");
    card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)?;
    log!("Commit ok.");
    Ok(())
}

// ---------- NIF bindings ----------

/// Holds DRM state and triple framebuffers for atomic flips.
struct Display {
    card: Arc<Card>,
    blob_id: u64,
    buffers: Vec<FbBundle>,
    plane_h: plane::Handle,
    prop_fb: property::Handle,
    prop_crtc: property::Handle,
    crtc_h: crtc::Handle,
    w: u32,
    h: u32,
    cur: usize,
}

impl Display {
    fn new(card_path: &str) -> Result<(Self, u32, u32)> {
        let card = Arc::new(open_card(card_path)?);
        enable_atomic_caps(&card)?;
        let res = get_resources(&card)?;
        let conn = pick_connected_connector(&card, &res)?;
        let (_enc, crtc_h) = pick_encoder_and_crtc(&card, &conn)?;
        let mode = pick_mode(&conn)?;
        let (w16, h16) = mode.size();
        let (w, h) = (w16 as u32, h16 as u32);

        // Create three framebuffers for triple buffering.
        let mut buffers = Vec::with_capacity(3);
        for _ in 0..3 {
            buffers.push(create_dumb_and_fb(&card, w, h)?);
        }

        let plane_h = find_plane_for_crtc(&card, &res, crtc_h)?;
        let (mode_blob, blob_id) = create_mode_blob(&card, &mode)?;
        let req = build_atomic_request(
            &card,
            &conn,
            crtc_h,
            plane_h,
            buffers[0].fb,
            &mode,
            mode_blob,
        )?;
        commit_atomic(&card, req)?;

        let name = |s: &str| CString::new(s).unwrap();
        let prop_fb = find_prop(&card, plane_h, &name("FB_ID"))?;
        let prop_crtc = find_prop(&card, plane_h, &name("CRTC_ID"))?;

        Ok((
            Self {
                card,
                blob_id,
                buffers,
                plane_h,
                prop_fb,
                prop_crtc,
                crtc_h,
                w,
                h,
                cur: 0,
            },
            w,
            h,
        ))
    }

    fn display_frame(&mut self, frame: &[u8]) -> Result<()> {
        let frame_size = (self.w as usize * self.h as usize * 3) / 2;
        if frame.len() != frame_size {
            return Err(err("invalid frame size"));
        }
        let next = (self.cur + 1) % self.buffers.len();
        let buf = &mut self.buffers[next];
        let pitch = buf.db.pitch() as usize;
        {
            let mut mapping = self.card.map_dumb_buffer(&mut buf.db)?;
            copy_nv12_frame(
                frame,
                mapping.as_mut(),
                pitch,
                self.w as usize,
                self.h as usize,
            );
        }
        let mut req = AtomicModeReq::new();
        req.add_property(
            self.plane_h,
            self.prop_crtc,
            property::Value::CRTC(Some(self.crtc_h)),
        );
        req.add_property(
            self.plane_h,
            self.prop_fb,
            property::Value::Framebuffer(Some(buf.fb)),
        );
        // Commit asynchronously so the caller isn't blocked waiting for vblank.
        self.card.atomic_commit(AtomicCommitFlags::NONBLOCK, req)?;
        self.cur = next;
        Ok(())
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        self.card.destroy_property_blob(self.blob_id).ok();
        for FbBundle { db, fb } in self.buffers.drain(..) {
            self.card.destroy_framebuffer(fb).ok();
            self.card.destroy_dumb_buffer(db).ok();
        }
    }
}

struct DisplayResource(std::sync::Mutex<Display>);

unsafe impl Send for DisplayResource {}
unsafe impl Sync for DisplayResource {}

fn nif_error<E: std::fmt::Display>(e: E) -> rustler::Error {
    rustler::Error::Term(Box::new(format!("{e}")))
}

#[rustler::nif]
fn init_display(card_path: Option<String>) -> NifResult<ResourceArc<DisplayResource>> {
    let path = card_path.unwrap_or_else(|| "/dev/dri/card0".to_string());
    let (display, _w, _h) = Display::new(&path).map_err(nif_error)?;
    Ok(ResourceArc::new(DisplayResource(std::sync::Mutex::new(
        display,
    ))))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn display_frame(res: ResourceArc<DisplayResource>, frame: Binary) -> NifResult<()> {
    let mut guard = res.0.lock().map_err(|_| nif_error("lock poisoned"))?;
    guard.display_frame(frame.as_slice()).map_err(nif_error)
}

#[allow(non_local_definitions)]
fn load(env: rustler::Env, _info: rustler::Term) -> bool {
    rustler::resource!(DisplayResource, env)
}

rustler::init!("Elixir.DrmSink", load = load);
