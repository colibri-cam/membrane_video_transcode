defmodule Membrane.DRM.Sink.Native do
  use Rustler, otp_app: :drm_experiments, crate: "drm_sink"

  def init_display(_card_path, _pixel_format), do: :erlang.nif_error(:nif_not_loaded)
  def display_frame(_handle, _frame), do: :erlang.nif_error(:nif_not_loaded)
  def close_display(_handle), do: :erlang.nif_error(:nif_not_loaded)
end
