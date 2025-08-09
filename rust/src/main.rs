use anyhow::{anyhow, Context, Result};
use drm::buffer::Buffer;                   // trait for .pitch()
use drm::control as dc;
use drm::control::atomic::AtomicModeReq;
use drm::control::{connector, crtc, encoder, plane, property, AtomicCommitFlags, Device as _};
use drm::control::dumbbuffer as dumbbuf;
use drm::Device as _;                     // for set_client_capability()
use drm::ClientCapability;                 // caps to enable atomic / universal planes
use std::env;
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::os::fd::AsFd;
use std::thread;
use std::time::Duration;

const DISPLAY_TIME: u64 = 10; // seconds

// Thin wrapper so drm-rs trait impls apply
struct Card(File);
impl AsFd for Card { fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> { self.0.as_fd() } }
impl drm::Device for Card {}
impl dc::Device for Card {}

// ---------- helpers ----------
fn enable_atomic_caps(card: &Card) -> Result<()> {
    // Must be enabled *before* querying planes/properties
    card.set_client_capability(ClientCapability::UniversalPlanes, true)
        .context("enable DRM_CLIENT_CAP_UNIVERSAL_PLANES")?;
    card.set_client_capability(ClientCapability::Atomic, true)
        .context("enable DRM_CLIENT_CAP_ATOMIC")?;
    println!("Enabled client caps: UNIVERSAL_PLANES + ATOMIC");
    Ok(())
}


fn open_card(path: &str) -> Result<Card> {
    println!("Opening DRM device: {path}");
    let file = OpenOptions::new().read(true).write(true).open(path)
        .with_context(|| format!("open {path}"))?;
    Ok(Card(file))
}

fn get_resources(card: &Card) -> Result<dc::ResourceHandles> {
    Ok(card.resource_handles().context("get resources")?)
}

fn pick_connected_connector(card: &Card, res: &dc::ResourceHandles) -> Result<connector::Info> {
    for &conn_h in res.connectors() {
        let info = card.get_connector(conn_h, true).context("get connector")?;
        if info.state() == connector::State::Connected && !info.modes().is_empty() {
            println!(
                "Selected connector: id={}, type={:?}, modes={}",
                u32::from(info.handle()), info.interface(), info.modes().len()
            );
            return Ok(info);
        }
    }
    Err(anyhow!("No connected connector"))
}

fn pick_encoder_and_crtc(card: &Card, conn: &connector::Info) -> Result<(encoder::Info, crtc::Handle)> {
    let enc_h = conn.encoders().first().copied()
        .ok_or_else(|| anyhow!("connector has no encoders"))?;
    let enc = card.get_encoder(enc_h).context("get encoder")?;
    let crtc_h = enc.crtc().ok_or_else(|| anyhow!("encoder has no crtc"))?;
    println!(
        "Selected encoder: id={}, type={:?}, CRTC id={}",
        u32::from(enc.handle()), enc.kind(), u32::from(crtc_h)
    );
    Ok((enc, crtc_h))
}

fn crtc_index(res: &dc::ResourceHandles, crtc_h: crtc::Handle) -> Result<u32> {
    let idx = res.crtcs().iter().position(|&h| h == crtc_h)
        .ok_or_else(|| anyhow!("CRTC not in resources"))? as u32;
    Ok(idx)
}

fn pick_mode(conn: &connector::Info) -> Result<dc::Mode> {
    let mode = *conn.modes().first().ok_or_else(|| anyhow!("connector has no modes"))?;
    println!("Mode: {}x{}@{}Hz", mode.size().0, mode.size().1, mode.vrefresh());
    Ok(mode)
}

struct FbBundle {
    db: dumbbuf::DumbBuffer,
    fb: dc::framebuffer::Handle
}

fn create_dumb_and_fb(card: &Card, w: u32, h: u32) -> Result<FbBundle> {
    // drm-rs 0.14 signature: (size, format, bpp)
    let mut db = card
        .create_dumb_buffer((w, h), drm::buffer::DrmFourcc::Xrgb8888, 32)
        .context("create dumb")?;

    let pitch = db.pitch();
    {
        // Map, draw, then drop the mapping before creating FB to avoid borrow conflicts
        let mut mapping = card.map_dumb_buffer(&mut db).context("map dumb")?;
        // draw gradient (XRGB8888 little-endian: B,G,R,X)
        let buf = unsafe { std::slice::from_raw_parts_mut(mapping.as_mut_ptr(), (pitch * h) as usize) };
        let pitch_usize = pitch as usize;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let r = ((x * 255) / w as usize) as u8;
                let g = ((y * 255) / h as usize) as u8;
                let b = 128u8;
                let off = y * pitch_usize + x * 4;
                buf[off + 0] = b;
                buf[off + 1] = g;
                buf[off + 2] = r;
                buf[off + 3] = 0x00;
            }
        }
        // mapping dropped here
    }

    let fb = card.add_framebuffer(&db, 24, 32).context("add fb")?;
    println!("Created framebuffer id={}", u32::from(fb));

    Ok(FbBundle { db, fb})
}

fn find_plane_for_crtc(card: &Card, res: &dc::ResourceHandles, crtc_h: crtc::Handle) -> Result<plane::Handle> {
    let idx = crtc_index(res, crtc_h)?; // still handy for logs
    let planes = card.plane_handles().context("plane handles")?;
    for &ph in planes.as_slice() {
        let pinfo = card.get_plane(ph).context("get plane")?;
        // Use the helper to turn filter into a list of allowed CRTCs
        let allowed = res.filter_crtcs(pinfo.possible_crtcs());
        if allowed.contains(&crtc_h) {
            println!(
                "Selected plane id={}, possible_crtcs_mask_index_includes={}",
                u32::from(ph), idx
            );
            return Ok(ph);
        }
    }
    Err(anyhow!("No compatible plane found"))
}

fn create_mode_blob(card: &Card, mode: &dc::Mode) -> Result<(property::Value<'static>, u64)> {
    let blob_val = card.create_property_blob(mode).context("create blob")?;
    let blob_id = match blob_val { property::Value::Blob(id) => id, _ => unreachable!() };
    println!("Created mode blob id={}", blob_id);
    Ok((blob_val, blob_id))
}

fn find_prop(card: &Card, obj: impl dc::ResourceHandle, name: &CStr) -> Result<property::Handle> {
    let props = card.get_properties(obj).context("get properties")?;
    for (handle, _raw) in props.iter() {
        let info = card.get_property(*handle).context("get property")?;
        if info.name().to_bytes() == name.to_bytes() {
            return Ok(*handle);
        }
    }
    Err(anyhow!("property {:?} not found", name))
}

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
    req.add_property(conn.handle(), conn_crtc, property::Value::CRTC(Some(crtc_h)));

    // CRTC: set mode + active
    let crtc_mode = find_prop(card, crtc_h, &name("MODE_ID"))?;
    let crtc_active = find_prop(card, crtc_h, &name("ACTIVE"))?;
    req.add_property(crtc_h, crtc_mode, mode_blob);
    req.add_property(crtc_h, crtc_active, property::Value::Boolean(true));

    // Plane: hook it up to CRTC+FB and set src/crtc rectangles
    let p_crtc   = find_prop(card, plane_h, &name("CRTC_ID"))?;
    let p_fb     = find_prop(card, plane_h, &name("FB_ID"))?;
    let p_src_x  = find_prop(card, plane_h, &name("SRC_X"))?;
    let p_src_y  = find_prop(card, plane_h, &name("SRC_Y"))?;
    let p_src_w  = find_prop(card, plane_h, &name("SRC_W"))?;
    let p_src_h  = find_prop(card, plane_h, &name("SRC_H"))?;
    let p_crtc_x = find_prop(card, plane_h, &name("CRTC_X"))?;
    let p_crtc_y = find_prop(card, plane_h, &name("CRTC_Y"))?;
    let p_crtc_w = find_prop(card, plane_h, &name("CRTC_W"))?;
    let p_crtc_h = find_prop(card, plane_h, &name("CRTC_H"))?;

    req.add_property(plane_h, p_crtc, property::Value::CRTC(Some(crtc_h)));
    req.add_property(plane_h, p_fb,   property::Value::Framebuffer(Some(fb_h)));

    let (w16, h16) = mode.size();
    let (w, h) = (w16 as u32, h16 as u32);
    req.add_property(plane_h, p_src_x,  property::Value::UnsignedRange(0));
    req.add_property(plane_h, p_src_y,  property::Value::UnsignedRange(0));
    req.add_property(plane_h, p_src_w,  property::Value::UnsignedRange((w as u64) << 16));
    req.add_property(plane_h, p_src_h,  property::Value::UnsignedRange((h as u64) << 16));
    req.add_property(plane_h, p_crtc_x, property::Value::SignedRange(0));
    req.add_property(plane_h, p_crtc_y, property::Value::SignedRange(0));
    req.add_property(plane_h, p_crtc_w, property::Value::UnsignedRange(w as u64));
    req.add_property(plane_h, p_crtc_h, property::Value::UnsignedRange(h as u64));

    Ok(req)
}

fn commit_atomic(card: &Card, req: AtomicModeReq) -> Result<()> {
    println!("Committing atomic modeset...");
    card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
        .context("atomic commit")?;
    println!("Commit ok. Showing for {DISPLAY_TIME}s");
    Ok(())
}

fn teardown(card: &Card, blob_id: u64, fb: dc::framebuffer::Handle, db: dumbbuf::DumbBuffer) -> Result<()> {
    card.destroy_property_blob(blob_id).ok();
    card.destroy_framebuffer(fb).ok();
    card.destroy_dumb_buffer(db).ok();
    Ok(())
}

// ---------- main orchestration ----------
fn main() -> Result<()> {
    let card_path = env::args().nth(1).unwrap_or_else(|| "/dev/dri/card0".to_string());

    let card = open_card(&card_path)?;
    enable_atomic_caps(&card)?;
    let res = get_resources(&card)?;
    let conn = pick_connected_connector(&card, &res)?;
    let (_enc, crtc_h) = pick_encoder_and_crtc(&card, &conn)?;

    let crtc_info = card.get_crtc(crtc_h).context("get crtc")?;
    println!(
        "Using CRTC: id={}, fb={}, x={}, y={}",
        u32::from(crtc_info.handle()),
        crtc_info.framebuffer().map(u32::from).unwrap_or(0),
        crtc_info.position().0,
        crtc_info.position().1
    );

    let mode = pick_mode(&conn)?;
    let (w, h) = mode.size();

    let fbundle = create_dumb_and_fb(&card, w as u32, h as u32)?;
    let plane_h = find_plane_for_crtc(&card, &res, crtc_h)?;

    let (mode_blob, blob_id) = create_mode_blob(&card, &mode)?;

    let req = build_atomic_request(&card, &conn, crtc_h, plane_h, fbundle.fb, &mode, mode_blob)?;
    commit_atomic(&card, req)?;

    thread::sleep(Duration::from_secs(DISPLAY_TIME));

    teardown(&card, blob_id, fbundle.fb, fbundle.db)?;
    Ok(())
}
