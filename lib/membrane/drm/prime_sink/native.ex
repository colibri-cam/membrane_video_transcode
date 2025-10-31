defmodule Membrane.DRM.PrimeSink.Native do
  @rustler_opts Mix.Project.config()[:rustler_opts]
  @features ["verbose" | Keyword.get(@rustler_opts, :features, [])]

  use Rustler,
      Keyword.merge(
        @rustler_opts,
        [
          otp_app: :membrane_drm_sink,
          crate: "drm_prime_sink",
          features: @features
        ]
      )

  def init_display(_card_path, _preferred_mode), do: :erlang.nif_error(:nif_not_loaded)
  def display_prime(_handle, _desc), do: :erlang.nif_error(:nif_not_loaded)
  def close_display(_handle), do: :erlang.nif_error(:nif_not_loaded)
end
