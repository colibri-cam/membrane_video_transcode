use std::ffi::CString;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::sys;
use ffmpeg_next::{
    codec, format,
    util::{error::EAGAIN, frame::Video},
};
use rustler::{Atom, Binary, Encoder, Env, NifResult, ResourceArc, Term};

rustler::atoms! {
    nv12
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

struct DecoderInner {
    decoder: codec::decoder::Video,
    width: u32,
    height: u32,
}

struct Decoder {
    inner: Mutex<DecoderInner>,
}

unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.decoder.flush();
        }
    }
}

fn init_decoder() -> Result<Decoder> {
    ffmpeg::init().context("ffmpeg init failed")?;
    let hevc = codec::decoder::find(codec::Id::HEVC).ok_or_else(|| anyhow!("no hevc codec"))?;
    let mut decoder = codec::decoder::new();
    unsafe {
        let mut hw_device_ctx = std::ptr::null_mut();
        let path = CString::new("/dev/dri/renderD128").context("device path")?;
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
    rustler::resource!(Decoder, env)
}

#[rustler::nif]
fn create(_format: Atom) -> NifResult<ResourceArc<Decoder>> {
    init_decoder()
        .map(ResourceArc::new)
        .map_err(|_| rustler::Error::Atom("create_failed"))
}

fn export_drm_prime(frame: &Video) -> Result<PrimeDesc> {
    let mut drm = Video::new(format::Pixel::DRM_PRIME, frame.width(), frame.height());
    let res = unsafe { sys::av_hwframe_transfer_data(drm.as_mut_ptr(), frame.as_ptr(), 0) };
    if res < 0 {
        return Err(anyhow!("transfer"));
    }
    let desc_ptr = unsafe { (*drm.as_ptr()).data[0] as *const sys::AVDRMFrameDescriptor };
    if desc_ptr.is_null() {
        return Err(anyhow!("no_desc"));
    }
    let desc = unsafe { &*desc_ptr };
    if desc.nb_objects == 0 || desc.nb_layers == 0 {
        return Err(anyhow!("empty_desc"));
    }
    let fd = unsafe { libc::dup(desc.objects[0].fd) };
    if fd < 0 {
        return Err(anyhow!("dup"));
    }
    let layer = &desc.layers[0];
    let mut pitches = Vec::new();
    let mut offsets = Vec::new();
    for i in 0..layer.nb_planes as usize {
        let p = layer.planes[i];
        pitches.push(p.pitch as u32);
        offsets.push(p.offset as u32);
    }
    Ok(PrimeDesc {
        fd,
        width: frame.width(),
        height: frame.height(),
        pixel_format: nv12(),
        pitches,
        offsets,
    })
}

fn decode_frames<'a>(
    env: Env<'a>,
    inner: &mut DecoderInner,
) -> NifResult<(Vec<i64>, Vec<Term<'a>>)> {
    let mut frames = Vec::new();
    let mut pts_list = Vec::new();
    let mut decoded = Video::empty();
    loop {
        match inner.decoder.receive_frame(&mut decoded) {
            Ok(_) => {
                inner.width = decoded.width();
                inner.height = decoded.height();
                let desc =
                    export_drm_prime(&decoded).map_err(|_| rustler::Error::Atom("export"))?;
                pts_list.push(decoded.timestamp().unwrap_or(0));
                frames.push(desc.encode(env));
                unsafe { sys::av_frame_unref(decoded.as_mut_ptr()) };
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(ffmpeg::Error::Other { errno }) if errno == EAGAIN => break,
            Err(_) => return Err(rustler::Error::Atom("decode")),
        }
    }
    Ok((pts_list, frames))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn decode<'a>(
    env: Env<'a>,
    state: ResourceArc<Decoder>,
    data: Binary<'a>,
    pts: i64,
    dts: i64,
) -> NifResult<(Vec<i64>, Vec<Term<'a>>)> {
    let mut packet = Packet::copy(data.as_slice());
    packet.set_pts(Some(pts));
    packet.set_dts(Some(dts));

    let mut inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    inner
        .decoder
        .send_packet(&packet)
        .map_err(|_| rustler::Error::Atom("send_packet"))?;
    decode_frames(env, &mut inner)
}

#[rustler::nif(schedule = "DirtyCpu")]
fn flush<'a>(env: Env<'a>, state: ResourceArc<Decoder>) -> NifResult<(Vec<i64>, Vec<Term<'a>>)> {
    let mut inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    inner
        .decoder
        .send_eof()
        .map_err(|_| rustler::Error::Atom("send_eof"))?;
    decode_frames(env, &mut inner)
}

#[rustler::nif]
fn get_metadata(state: ResourceArc<Decoder>) -> NifResult<(u32, u32, Atom)> {
    let inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    Ok((inner.width, inner.height, nv12()))
}

rustler::init!("Elixir.Membrane.H265.PrimeDecoder.Native", load = load);
