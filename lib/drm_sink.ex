defmodule DrmSink do
  use Rustler, otp_app: :drm_experiments, crate: "drm_sink"

  def init_display(_card_path \\ "/dev/dri/card0"), do: :erlang.nif_error(:nif_not_loaded)
  def display_frame(_handle, _frame), do: :erlang.nif_error(:nif_not_loaded)
end
