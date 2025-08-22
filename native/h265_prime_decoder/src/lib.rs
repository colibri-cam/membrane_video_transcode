use std::ffi::CString;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use drm_fourcc::DrmFourcc;
use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::sys;
use ffmpeg_next::{
    codec,
    util::{error::EAGAIN, frame::Video},
};
use rustler::{Atom, Binary, Encoder, Env, NifResult, ResourceArc, Term};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

rustler::atoms! {
    ok
}

const NO_PTS: i64 = i64::MIN;

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
    fd: Fd,
    pitch: u32,
    offset: u32,
    modifier: Option<u64>,
}

#[derive(Debug)]
struct Fourcc(DrmFourcc);

impl Encoder for Fourcc {
    fn encode<'a>(&self, env: Env<'a>) -> Term<'a> {
        (self.0 as u32).encode(env)
    }
}

impl<'a> rustler::Decoder<'a> for Fourcc {
    fn decode(term: Term<'a>) -> NifResult<Self> {
        let val: u32 = term.decode()?;
        Ok(Fourcc(
            DrmFourcc::try_from(val).map_err(|_| rustler::Error::BadArg)?,
        ))
    }
}

#[derive(rustler::NifStruct)]
#[module = "Membrane.PrimeDesc"]
struct PrimeDesc {
    width: u32,
    height: u32,
    format: Fourcc,
    planes: Vec<PrimePlane>,
}

impl Encoder for Fd {
    fn encode<'a>(&self, env: Env<'a>) -> Term<'a> {
        let dup_fd = unsafe { libc::dup(self.0.as_raw_fd()) };
        dup_fd.encode(env)
    }
}

impl<'a> rustler::Decoder<'a> for Fd {
    fn decode(term: Term<'a>) -> NifResult<Self> {
        let fd: i32 = term.decode()?;
        if fd < 0 {
            Err(rustler::Error::BadArg)
        } else {
            Ok(Fd(unsafe { OwnedFd::from_raw_fd(fd) }))
        }
    }
}

struct DecoderInner {
    decoder: codec::decoder::Video,
    width: u32,
    height: u32,
}

struct Decoder {
    inner: Mutex<DecoderInner>,
}

#[allow(dead_code)]
struct Keepalive {
    resource: Video,
}

unsafe impl Send for Keepalive {}
unsafe impl Sync for Keepalive {}

unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.decoder.flush();
        }
    }
}

fn init_decoder(hw_device: String) -> Result<Decoder> {
    ffmpeg::init().context("ffmpeg init failed")?;
    let hevc = codec::decoder::find(codec::Id::HEVC).ok_or_else(|| anyhow!("no hevc codec"))?;
    let mut decoder = codec::decoder::new();

    unsafe {
        let mut hw_device_ctx = std::ptr::null_mut();
        let path = CString::new(hw_device).context("device path")?;
        if sys::av_hwdevice_ctx_create(
            &mut hw_device_ctx,
            sys::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            path.as_ptr(),
            std::ptr::null_mut(),
            0,
        ) >= 0
        {
            (*decoder.as_mut_ptr()).hw_device_ctx = sys::av_buffer_ref(hw_device_ctx);
            sys::av_buffer_unref(&mut hw_device_ctx);
        }
    }
    let opened = decoder.open_as(hevc).context("open codec")?;
    let video = opened.video().context("video decoder")?;
    Ok(Decoder {
        inner: Mutex::new(DecoderInner {
            decoder: video,
            width: 0,
            height: 0,
        }),
    })
}

#[allow(non_local_definitions)]
fn load(env: rustler::Env, _info: rustler::Term) -> bool {
    assert!(
        rustler::resource!(Keepalive, env),
        "regisetr Keepalive resource failed"
    );
    rustler::resource!(Decoder, env)
}

#[rustler::nif]
fn create(hw_device: String) -> NifResult<ResourceArc<Decoder>> {
    init_decoder(hw_device)
        .map(ResourceArc::new)
        .map_err(|_| rustler::Error::Atom("create_failed"))
}

fn export_drm_prime(frame: &Video) -> Result<PrimeDesc> {
    let mut drm = Video::empty();
    unsafe {
        (*drm.as_mut_ptr()).format = sys::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*drm.as_mut_ptr()).width = frame.width() as i32;
        (*drm.as_mut_ptr()).height = frame.height() as i32;
        (*drm.as_mut_ptr()).hw_frames_ctx = sys::av_buffer_ref((*frame.as_ptr()).hw_frames_ctx);
    }
    const AV_HWFRAME_MAP_DRM_PRIME: i32 = 0x0002_0000;
    let flags = (sys::AV_HWFRAME_MAP_READ as i32) | AV_HWFRAME_MAP_DRM_PRIME;
    let res = unsafe { sys::av_hwframe_map(drm.as_mut_ptr(), frame.as_ptr(), flags) };
    if res < 0 {
        return Err(anyhow!("av_hwframe_map failed: {res}"));
    }
    let desc_ptr = unsafe { (*drm.as_ptr()).data[0] as *const sys::AVDRMFrameDescriptor };
    if desc_ptr.is_null() {
        unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
        return Err(anyhow!("no drm descriptor"));
    }
    let desc = unsafe { &*desc_ptr };
    if desc.nb_objects == 0 || desc.nb_layers == 0 {
        unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
        return Err(anyhow!("empty drm descriptor"));
    }
    let mut planes = Vec::new();
    for l in 0..desc.nb_layers as usize {
        let layer = &desc.layers[l];
        for i in 0..layer.nb_planes as usize {
            let p = layer.planes[i];
            let obj = p.object_index as usize;
            if obj >= desc.nb_objects as usize {
                unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
                return Err(anyhow!("invalid object index"));
            }
            let fd = unsafe { libc::dup(desc.objects[obj].fd) };
            if fd < 0 {
                unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
                return Err(anyhow!("dup failed"));
            }
            let obj_desc = desc.objects[obj];
            planes.push(PrimePlane {
                fd: Fd(unsafe { OwnedFd::from_raw_fd(fd) }),
                pitch: p.pitch as u32,
                offset: p.offset as u32,
                modifier: Some(obj_desc.format_modifier),
            });
        }
    }

    unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
    Ok(PrimeDesc {
        width: frame.width(),
        height: frame.height(),
        format: Fourcc(DrmFourcc::Nv12),
        planes,
    })
}

type DecodeResult<'a> = (Vec<i64>, Vec<Term<'a>>, Vec<ResourceArc<Keepalive>>);

fn decode_frames<'a>(env: Env<'a>, inner: &mut DecoderInner) -> NifResult<DecodeResult<'a>> {
    let mut frames = Vec::new();
    let mut pts_list = Vec::new();
    let mut decoded = Video::empty();
    let mut keepalives = Vec::new();

    loop {
        match inner.decoder.receive_frame(&mut decoded) {
            Ok(_) => {
                inner.width = decoded.width();
                inner.height = decoded.height();
                let desc =
                    export_drm_prime(&decoded).map_err(|_| rustler::Error::Atom("export"))?;
                let mut res = Video::empty();
                unsafe {
                    // keepalive now references the same underlying surface
                    sys::av_frame_ref(res.as_mut_ptr(), decoded.as_ptr());
                }
                let keepalive = ResourceArc::new(Keepalive { resource: res });
                keepalives.push(keepalive);
                pts_list.push(decoded.pts().unwrap_or(NO_PTS));
                frames.push(desc.encode(env));
                unsafe { sys::av_frame_unref(decoded.as_mut_ptr()) };
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(ffmpeg::Error::Other { errno }) if errno == EAGAIN => break,
            Err(_) => return Err(rustler::Error::Atom("decode")),
        }
    }
    Ok((pts_list, frames, keepalives))
}

type DecodeResultWithOk<'a> = (Atom, Vec<i64>, Vec<Term<'a>>, Vec<ResourceArc<Keepalive>>);

#[rustler::nif(schedule = "DirtyCpu")]
fn decode<'a>(
    env: Env<'a>,
    state: ResourceArc<Decoder>,
    data: Binary<'a>,
    pts: i64,
    dts: i64,
) -> NifResult<DecodeResultWithOk<'a>> {
    let mut packet = Packet::copy(data.as_slice());
    packet.set_pts((pts != NO_PTS).then_some(pts));
    packet.set_dts((dts != NO_PTS).then_some(dts));

    let mut inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    inner
        .decoder
        .send_packet(&packet)
        .map_err(|_| rustler::Error::Atom("send_packet"))?;
    let (pts_list, frames, keepalives) = decode_frames(env, &mut inner)?;
    Ok((ok(), pts_list, frames, keepalives))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn flush<'a>(env: Env<'a>, state: ResourceArc<Decoder>) -> NifResult<DecodeResultWithOk<'a>> {
    let mut inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    inner
        .decoder
        .send_eof()
        .map_err(|_| rustler::Error::Atom("send_eof"))?;
    let (pts_list, frames, keepalives) = decode_frames(env, &mut inner)?;
    Ok((ok(), pts_list, frames, keepalives))
}

#[rustler::nif]
fn close(state: ResourceArc<Decoder>) -> NifResult<Atom> {
    let mut inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    inner.decoder.flush();
    Ok(ok())
}

#[rustler::nif]
fn get_metadata(state: ResourceArc<Decoder>) -> NifResult<(Atom, u32, u32)> {
    let inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    Ok((ok(), inner.width, inner.height))
}

rustler::init!("Elixir.Membrane.H265.PrimeDecoder.Native", load = load);
