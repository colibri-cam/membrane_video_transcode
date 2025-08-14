defmodule Membrane.DRM.PrimeSink.Native do
  use Rustler, otp_app: :drm_experiments, crate: "drm_prime_sink"

  def init_display(_card_path), do: :erlang.nif_error(:nif_not_loaded)
  def display_prime(_handle, _desc), do: :erlang.nif_error(:nif_not_loaded)
  def close_display(_handle), do: :erlang.nif_error(:nif_not_loaded)
end
