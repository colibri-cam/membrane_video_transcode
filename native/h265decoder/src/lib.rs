use std::ffi::CString;
use std::ptr;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::sys;
use ffmpeg_next::{
    codec, format,
    software::scaling::{context::Context as Scaler, flag::Flags},
    util::{error::EAGAIN, frame::Video},
};
use rustler::{Atom, Binary, Env, Error, NifResult, OwnedBinary, ResourceArc, Term};

struct DecoderInner {
    decoder: codec::decoder::Video,
    target: format::Pixel,
    scaler: Option<Scaler>,
}

pub struct Decoder {
    inner: Mutex<DecoderInner>,
}

unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}

fn init_decoder(target: format::Pixel) -> Result<Decoder> {
    ffmpeg::init().context("ffmpeg init failed")?;
    let hevc = codec::decoder::find(codec::Id::HEVC).ok_or_else(|| anyhow!("no hevc codec"))?;
    let mut decoder = codec::decoder::new();
    unsafe {
        let mut hw_device_ctx = ptr::null_mut();
        let path = CString::new("/dev/dri/renderD128").context("device path")?;
        if sys::av_hwdevice_ctx_create(
            &mut hw_device_ctx,
            sys::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            path.as_ptr(),
            ptr::null_mut(),
            0,
        ) >= 0
        {
            (*decoder.as_mut_ptr()).hw_device_ctx = sys::av_buffer_ref(hw_device_ctx);
        }
    }
    let opened = decoder.open_as(hevc).context("open codec")?;
    let video = opened.video().context("video decoder")?;
    Ok(Decoder {
        inner: Mutex::new(DecoderInner {
            decoder: video,
            target,
            scaler: None,
        }),
    })
}

#[rustler::nif]
fn create(format: Atom) -> NifResult<ResourceArc<Decoder>> {
    let pix = pixel_from_atom(format).ok_or(Error::Atom("bad_pixel_format"))?;
    init_decoder(pix)
        .map(ResourceArc::new)
        .map_err(|_| Error::Atom("create_failed"))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn decode<'a>(
    env: Env<'a>,
    state: ResourceArc<Decoder>,
    data: Binary<'a>,
    pts: i64,
    dts: i64,
) -> NifResult<(Vec<i64>, Vec<Binary<'a>>)> {
    let mut packet = Packet::copy(data.as_slice());
    packet.set_pts(Some(pts));
    packet.set_dts(Some(dts));

    let mut inner = state.inner.lock().map_err(|_| Error::Atom("lock"))?;
    inner
        .decoder
        .send_packet(&packet)
        .map_err(|_| Error::Atom("send_packet"))?;

    let mut frames = Vec::new();
    let mut pts_list = Vec::new();
    let mut decoded = Video::empty();
    let mut mapped = Video::empty();
    let mut converted = Video::empty();

    loop {
        match inner.decoder.receive_frame(&mut decoded) {
            Ok(_) => {
                let mut src_ref: &Video = &decoded;
                if decoded.format() == format::Pixel::VAAPI {
                    let res = unsafe {
                        sys::av_hwframe_transfer_data(mapped.as_mut_ptr(), decoded.as_ptr(), 0)
                    };
                    if res < 0 {
                        return Err(Error::Atom("map_frame"));
                    }
                    src_ref = &mapped;
                }

                if src_ref.format() != inner.target {
                    if inner.scaler.is_none() {
                        let scaler = Scaler::get(
                            src_ref.format(),
                            src_ref.width(),
                            src_ref.height(),
                            inner.target,
                            src_ref.width(),
                            src_ref.height(),
                            Flags::FAST_BILINEAR,
                        )
                        .map_err(|_| Error::Atom("scaler"))?;
                        inner.scaler = Some(scaler);
                    }
                    let scaler = inner.scaler.as_mut().unwrap();
                    scaler
                        .run(src_ref, &mut converted)
                        .map_err(|_| Error::Atom("scale"))?;
                    src_ref = &converted;
                }

                let format: sys::AVPixelFormat = inner.target.into();
                let buf_size = unsafe {
                    sys::av_image_get_buffer_size(
                        format,
                        src_ref.width() as i32,
                        src_ref.height() as i32,
                        1,
                    )
                };
                if buf_size < 0 {
                    return Err(Error::Atom("buffer_size"));
                }
                let mut out =
                    OwnedBinary::new(buf_size as usize).ok_or(Error::Atom("alloc_binary"))?;
                let copy_res = unsafe {
                    sys::av_image_copy_to_buffer(
                        out.as_mut_slice().as_mut_ptr(),
                        buf_size,
                        (*src_ref.as_ptr()).data.as_ptr() as *const *const u8,
                        (*src_ref.as_ptr()).linesize.as_ptr(),
                        format,
                        src_ref.width() as i32,
                        src_ref.height() as i32,
                        1,
                    )
                };
                if copy_res < 0 {
                    return Err(Error::Atom("copy"));
                }
                pts_list.push(src_ref.timestamp().unwrap_or(0));
                frames.push(out.release(env));
                unsafe {
                    sys::av_frame_unref(decoded.as_mut_ptr());
                    sys::av_frame_unref(mapped.as_mut_ptr());
                    sys::av_frame_unref(converted.as_mut_ptr());
                }
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(ffmpeg::Error::Other { errno }) if errno == EAGAIN => break,
            Err(_) => return Err(Error::Atom("decode")),
        }
    }

    Ok((pts_list, frames))
}

#[rustler::nif]
fn get_metadata(state: ResourceArc<Decoder>) -> NifResult<(u32, u32, Atom)> {
    let inner = state.inner.lock().map_err(|_| Error::Atom("lock"))?;
    let width = inner.decoder.width();
    let height = inner.decoder.height();
    let atom = atom_from_pixel(inner.target).ok_or(Error::Atom("pixel_format"))?;
    Ok((width, height, atom))
}

mod atoms {
    rustler::atoms! {
        nv12,
        yuv420p,
        rgb24
    }
}

fn pixel_from_atom(atom: Atom) -> Option<format::Pixel> {
    if atom == atoms::nv12() {
        Some(format::Pixel::NV12)
    } else if atom == atoms::yuv420p() {
        Some(format::Pixel::YUV420P)
    } else if atom == atoms::rgb24() {
        Some(format::Pixel::RGB24)
    } else {
        None
    }
}

fn atom_from_pixel(pix: format::Pixel) -> Option<Atom> {
    if pix == format::Pixel::NV12 {
        Some(atoms::nv12())
    } else if pix == format::Pixel::YUV420P {
        Some(atoms::yuv420p())
    } else if pix == format::Pixel::RGB24 {
        Some(atoms::rgb24())
    } else {
        None
    }
}

#[allow(non_local_definitions)]
fn on_load(env: Env, _info: Term) -> bool {
    let _ = rustler::resource!(Decoder, env);
    true
}

rustler::init!("Elixir.Membrane.H265Decoder.Native", load = on_load);
