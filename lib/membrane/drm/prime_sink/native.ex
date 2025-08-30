defmodule Membrane.DRM.PrimeSink.Native do
  @rustler_opts Mix.Project.config()[:rustler_opts]

  use Rustler,
      Keyword.merge(
        [
          otp_app: :drm_experiments,
          crate: "drm_prime_sink"
        ],
        @rustler_opts
      )

  def init_display(_card_path, _preferred_mode), do: :erlang.nif_error(:nif_not_loaded)
  def display_prime(_handle, _desc), do: :erlang.nif_error(:nif_not_loaded)
  def close_display(_handle), do: :erlang.nif_error(:nif_not_loaded)
end
