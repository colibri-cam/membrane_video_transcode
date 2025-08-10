defmodule Membrane.H265Decoder.Native do
  use Rustler, otp_app: :drm_experiments, crate: "h265decoder"

  def create(_format), do: :erlang.nif_error(:nif_not_loaded)
  def decode(_state, _data, _pts, _dts), do: :erlang.nif_error(:nif_not_loaded)
  def get_metadata(_state), do: :erlang.nif_error(:nif_not_loaded)
end
