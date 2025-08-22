defmodule Membrane.H265.Decoder.Native do
  use Rustler, otp_app: :drm_experiments, crate: "h265_decoder"

  def create(_format), do: :erlang.nif_error(:nif_not_loaded)
  def decode(_state, _data, _pts, _dts), do: :erlang.nif_error(:nif_not_loaded)
  def flush(_state), do: :erlang.nif_error(:nif_not_loaded)
  def get_metadata(_state), do: :erlang.nif_error(:nif_not_loaded)
  def close(_state), do: :erlang.nif_error(:nif_not_loaded)
end
