defmodule Membrane.DRM.Sink.Native do
  @rustler_opts Mix.Project.config()[:rustler_opts]

  use Rustler,
      Keyword.merge(
        [
          otp_app: :membrane_drm_sink,
          crate: "drm_sink"
        ],
        @rustler_opts
      )

  def init_display(_card_path, _pixel_format), do: :erlang.nif_error(:nif_not_loaded)
  def display_frame(_handle, _frame), do: :erlang.nif_error(:nif_not_loaded)
  def close_display(_handle), do: :erlang.nif_error(:nif_not_loaded)
end
