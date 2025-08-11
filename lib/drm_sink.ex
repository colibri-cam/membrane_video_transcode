defmodule DrmSink.Native do
  use Rustler, otp_app: :drm_experiments, crate: "drm_sink"

  def init_display(_card_path, _pixel_format), do: :erlang.nif_error(:nif_not_loaded)
  def display_frame(_handle, _frame), do: :erlang.nif_error(:nif_not_loaded)
  def close_display(_handle), do: :erlang.nif_error(:nif_not_loaded)
end

defmodule DrmSink do
  alias DrmSink.Native

  def init_display(pixel_format), do: init_display("/dev/dri/card0", pixel_format)

  def init_display(card_path, pixel_format) do
    case Native.init_display(card_path, pixel_format) do
      {:error, error} -> {:error, error}
      display -> {:ok, display}
    end
  end

  defdelegate display_frame(handle, frame), to: Native
  defdelegate close_display(handle), to: Native
end
