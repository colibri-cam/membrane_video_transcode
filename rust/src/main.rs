use drm::buffer::Buffer; // trait for .pitch()
use drm::control as dc;
use drm::control::atomic::AtomicModeReq;
use drm::control::dumbbuffer as dumbbuf;
use drm::control::FbCmd2Flags;
use drm::control::{connector, crtc, encoder, plane, property, AtomicCommitFlags, Device as _};
use drm::ClientCapability; // caps to enable atomic / universal planes
use drm::Device as _; // for set_client_capability()
use drm::{VblankWaitFlags, VblankWaitTarget};
use std::env;
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Read, Result, Seek, SeekFrom};
use std::os::fd::AsFd;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// Number of seconds to keep playing before exiting.
const DISPLAY_TIME: u64 = 10;

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

/// Helper managing vblank-synced presentation of NV12 frames.
struct FrameDisplay {
    tx: mpsc::Sender<Vec<u8>>,
    handle: Option<thread::JoinHandle<()>>,
    frame_size: usize,
    db: Arc<std::sync::Mutex<Option<dumbbuf::DumbBuffer>>>,
}

impl FrameDisplay {
    /// Spawn a worker thread that copies frames to the dumb buffer on each vblank.
    fn start(
        card: Arc<Card>,
        db: dumbbuf::DumbBuffer,
        w: u32,
        h: u32,
        crtc_idx: u32,
    ) -> Result<Self> {
        let pitch = db.pitch() as usize;
        let frame_size = (w as usize * h as usize * 3) / 2;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let db_arc = Arc::new(std::sync::Mutex::new(Some(db)));
        let db_thread = Arc::clone(&db_arc);
        let card_thread = Arc::clone(&card);
        let handle = thread::spawn(move || {
            let mut db = match db_thread.lock().ok().and_then(|mut g| g.take()) {
                Some(d) => d,
                None => return,
            };
            if let Ok(mut mapping) = card_thread.map_dumb_buffer(&mut db) {
                loop {
                    if card_thread
                        .wait_vblank(
                            VblankWaitTarget::Relative(1),
                            VblankWaitFlags::empty(),
                            crtc_idx,
                            0,
                        )
                        .is_err()
                    {
                        break;
                    }
                    let mut latest = None;
                    let mut disconnected = false;
                    loop {
                        match rx.try_recv() {
                            Ok(f) => latest = Some(f),
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => {
                                disconnected = true;
                                break;
                            }
                        }
                    }
                    if let Some(frame) = latest {
                        copy_nv12_frame(&frame, mapping.as_mut(), pitch, w as usize, h as usize);
                    }
                    if disconnected {
                        break;
                    }
                }
                drop(mapping);
            }
            if let Ok(mut g) = db_thread.lock() {
                *g = Some(db);
            }
        });
        Ok(Self {
            tx,
            handle: Some(handle),
            frame_size,
            db: db_arc,
        })
    }

    /// Queue a frame to be shown on the next vblank.
    fn display_frame(&self, frame: &[u8]) -> Result<()> {
        if frame.len() != self.frame_size {
            return Err(err("invalid frame size"));
        }
        self.tx
            .send(frame.to_vec())
            .map_err(|_| err("display thread closed"))
    }

    /// Stop the worker thread and return the underlying dumb buffer.
    fn stop(self) -> Result<dumbbuf::DumbBuffer> {
        drop(self.tx);
        if let Some(handle) = self.handle {
            handle.join().map_err(|_| err("thread join failed"))?;
        }
        Arc::try_unwrap(self.db)
            .map_err(|_| err("buffer still in use"))?
            .into_inner()
            .map_err(|_| err("mutex poisoned"))?
            .ok_or_else(|| err("buffer missing"))
    }
}

/// Create an NV12 dumb buffer, populate it with the first frame, and register a framebuffer.
fn create_dumb_and_fb_nv12(card: &Card, w: u32, h: u32, video: &mut File) -> Result<FbBundle> {
    // Allocate space for Y and interleaved UV planes.
    let db_height = h + h / 2;
    let mut db = card.create_dumb_buffer((w, db_height), drm::buffer::DrmFourcc::Nv12, 8)?;

    let pitch = db.pitch() as usize;
    let frame_size = (w as usize * h as usize * 3) / 2;
    {
        let mut frame = vec![0u8; frame_size];
        video.read_exact(&mut frame)?;
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

/// Clean up DRM resources allocated for the demo.
fn teardown(
    card: &Card,
    blob_id: u64,
    fb: dc::framebuffer::Handle,
    db: dumbbuf::DumbBuffer,
) -> Result<()> {
    card.destroy_property_blob(blob_id).ok();
    card.destroy_framebuffer(fb).ok();
    card.destroy_dumb_buffer(db).ok();
    Ok(())
}

// ---------- main orchestration ----------
fn main() -> Result<()> {
    let card_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/dri/card0".to_string());
    let video_path = env::args()
        .nth(2)
        .ok_or_else(|| err("NV12 file required"))?;
    let mut video = File::open(&video_path)?;

    let card = Arc::new(open_card(&card_path)?);
    enable_atomic_caps(&card)?;
    let res = get_resources(&card)?;
    let conn = pick_connected_connector(&card, &res)?;
    let (_enc, crtc_h) = pick_encoder_and_crtc(&card, &conn)?;
    let crtc_idx = res
        .crtcs()
        .iter()
        .position(|h| *h == crtc_h)
        .ok_or_else(|| err("CRTC handle not found"))? as u32;

    #[cfg(feature = "verbose")]
    {
        let crtc_info = card.get_crtc(crtc_h)?;
        log!(
            "Using CRTC: id={}, fb={}, x={}, y={}",
            u32::from(crtc_info.handle()),
            crtc_info.framebuffer().map(u32::from).unwrap_or(0),
            crtc_info.position().0,
            crtc_info.position().1
        );
    }

    let mode = pick_mode(&conn)?;
    let (w, h) = mode.size();

    let FbBundle { db, fb } = create_dumb_and_fb_nv12(&card, w as u32, h as u32, &mut video)?;
    let plane_h = find_plane_for_crtc(&card, &res, crtc_h)?;

    let (mode_blob, blob_id) = create_mode_blob(&card, &mode)?;

    let req = build_atomic_request(&card, &conn, crtc_h, plane_h, fb, &mode, mode_blob)?;
    commit_atomic(&card, req)?;
    video.seek(SeekFrom::Start(0))?;

    let display = FrameDisplay::start(Arc::clone(&card), db, w as u32, h as u32, crtc_idx)?;
    let frame_size = (w as usize * h as usize * 3) / 2;
    let mut frame = vec![0u8; frame_size];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(DISPLAY_TIME) {
        if let Err(e) = video.read_exact(&mut frame) {
            if e.kind() == ErrorKind::UnexpectedEof {
                video.seek(SeekFrom::Start(0))?;
                video.read_exact(&mut frame)?;
            } else {
                return Err(e);
            }
        }
        display.display_frame(&frame)?;
        thread::sleep(Duration::from_millis(15));
    }

    let db = display.stop()?;
    teardown(&card, blob_id, fb, db)?;
    Ok(())
}
