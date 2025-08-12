defmodule DrmPrime.Native do
  use Rustler, otp_app: :drm_experiments, crate: "drm_prime"

  def init_display(_card_path), do: :erlang.nif_error(:nif_not_loaded)
  def display_prime(_handle, _desc), do: :erlang.nif_error(:nif_not_loaded)
  def close_display(_handle), do: :erlang.nif_error(:nif_not_loaded)
end

defmodule DrmPrime do
  alias DrmPrime.Native

  def init_display(), do: init_display("/dev/dri/card0")

  def init_display(card_path) do
    case Native.init_display(card_path) do
      {:error, error} -> {:error, error}
      display -> {:ok, display}
    end
  end

  defdelegate display_prime(handle, desc), to: Native
  defdelegate close_display(handle), to: Native
end
