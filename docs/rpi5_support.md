# Raspberry Pi 5 Decoder Support

Raspberry Pi 5 support is hardware-qualified separately from the desktop VAAPI path.
`membrane_video_transcode` remains codec-only: display, DRM/KMS, and presentation concerns belong to
consumers outside this package.

## Build requirements

- Cross-compile `native/video_decoder` for `aarch64-unknown-linux-gnu` when the target system uses
  the standard 64-bit Raspberry Pi kernel.
- Use the `ffmpeg-rpi` libraries and headers supplied by `colibri-cam/nerves_system_gs`, or an
  equivalent Raspberry Pi-patched FFmpeg build, through the Nerves sysroot. Stock upstream FFmpeg
  does not currently provide the same V4L2 Request, DRM PRIME, and Raspberry Pi pixel-format
  contract.
- Build `ffmpeg-rpi` with V4L2 Request, DRM, and SAND support. Enable any V4L2 M2M codec used by a
  selected deployment backend as well.
- Keep the `video-interop` Rust source aligned with the Elixir `video_interop` dependency.

## Runtime contract

Select `:v4l2request` or `:v4l2m2m` explicitly after confirming the codec exposed by the target
FFmpeg build. DMA-BUF output must satisfy the same contract as VAAPI output:

- NV12 `%VideoInterop.Format{}` stream format;
- `%VideoInterop.Frame{}` buffer payloads;
- complete DMA-BUF allocation sizes and exact planes;
- one uniform explicit modifier per stream;
- a concrete acquire sync-file exported from the DMA-BUF reservation object;
- one bounded lease that retires all native frame, descriptor, and synchronization resources.

The `rpi` Cargo feature forwards to `ffmpeg-next/rpi`. This is required when compiling against
`ffmpeg-rpi` headers so `ffmpeg-next` recognizes the additional SAND and RPI4 pixel formats. The
feature does not build or install FFmpeg; the Nerves system must supply the matching patched
libraries. It also does not add display or scanout behavior.

## Qualification

On physical Raspberry Pi 5 hardware:

1. Decode both H.264 and H.265 fixtures with the selected backend.
2. Validate every stream format and frame with `VideoInterop.validate/1`.
3. Confirm the advertised modifier imports on the intended consumer GPU.
4. Release every frame and verify the decoder lease owner drains at EOS and shutdown.
5. Exercise held-frame backpressure and abandonment without leaking DMA-BUF or sync-file FDs.
6. Run `cargo test`, Clippy with warnings denied, a release build, and the Elixir test suite against
   the target FFmpeg build.
