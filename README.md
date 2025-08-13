# DrmExperiments

Elixir experiment demonstrating how to drive a DRM device from a Rust NIF.
The project provides four Membrane elements, each paired with a Rust
implementation built using [Rustler](https://github.com/rusterlium/rustler):

  * `Membrane.H265.Decoder` – decodes H265 into raw video (`native/h265_decoder`)
  * `Membrane.DRM.Sink` – renders raw video frames (`native/drm_sink`)
  * `Membrane.H265.PrimeDecoder` – outputs DRM Prime (DMABUF) descriptors
    (`native/h265_prime_decoder`)
  * `Membrane.DRM.PrimeSink` – scans out DRM Prime descriptors
    (`native/drm_prime_sink`)

## Installation

If [available in Hex](https://hex.pm/docs/publish), the package can be installed
by adding `drm_experiments` to your list of dependencies in `mix.exs`:

```elixir
def deps do
  [
    {:drm_experiments, "~> 0.1.0"}
  ]
end
```

Documentation can be generated with [ExDoc](https://github.com/elixir-lang/ex_doc)
and published on [HexDocs](https://hexdocs.pm). Once published, the docs can
be found at <https://hexdocs.pm/drm_experiments>.

## Cross‑compiling for Nerves

When building for Nerves targets, the Mix project automatically configures the
Rust compiler based on the `CC` and `NERVES_SDK_SYSROOT` environment variables
exported by the Nerves toolchain. The mapping covers common ARM targets:

```
"armv6-nerves-linux-gnueabihf"  -> "arm-unknown-linux-gnueabihf"
"armv7-nerves-linux-gnueabihf" -> "armv7-unknown-linux-gnueabihf"
"aarch64-nerves-linux-gnu"     -> "aarch64-unknown-linux-gnu"
```

Ensure the `cross` utility is installed and invoke it through `mix` when
compiling for non-host architectures, e.g.:

```
MIX_TARGET=rpi mix deps.get
MIX_TARGET=rpi mix compile
```

`Cross.toml` passes through `RUSTFLAGS` and `RUSTLER_NIF_VERSION` so that NIFs
build correctly under the cross environment.

