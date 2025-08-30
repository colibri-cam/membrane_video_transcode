# Raspberry Pi 5 Support Plan

This document outlines proposed changes required for the H265 decoder, H265 prime decoder, DRM sink, and DRM prime sink NIFs to run on a Raspberry Pi 5.

## Build and Toolchain
- Cross-compile all native crates for `aarch64-unknown-linux-gnu` since Raspberry Pi 5 defaults to a 64-bit kernel.
- Update `Cross.toml` files to include an explicit target for Raspberry Pi 5 and verify that `cross` uses a recent `aarch64` GCC toolchain.
- Ensure `ffmpeg` is built with the `v4l2-m2m` and `drm` backends enabled so hardware blocks are available.

## Decoder NIF Changes
- Replace the hard coded VAAPI path (`/dev/dri/renderD128`) with detection logic that prefers the `rpivid` or `v4l2` decoder nodes exposed by the Pi. This applies to both the regular and prime H265 decoder NIFs.
- Fall back to software decoding when no hardware decoder is available.
- Handle the NV12 pixel format produced by the Raspberry Pi hardware decoder without additional copies.

## Sink NIF Changes
- Select the `vc7` DRM driver used on the Pi 5 instead of assuming `card0`.
- Add plane and connector selection logic based on `vc7` DRM capabilities so the correct HDMI output is used.
- Support modifiers required by the Raspberry Pi framebuffer, e.g., `DRM_FORMAT_MOD_BROADCOM_SAND128` for direct scan‑out. These updates cover both the standard DRM sink and the DRM prime sink NIFs.

## Testing
- Integrate the Pi into CI by running the decoder and sink elements inside a Nerves deployment on real hardware.
- Exercise end-to-end playback with `mix test` to confirm the NIFs operate correctly on the board.

