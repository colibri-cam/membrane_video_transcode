defmodule Membrane.H265.Decoder.Native do
  @rustler_opts Mix.Project.config()[:rustler_opts]

  use Rustler,
      Keyword.merge(
        [
          otp_app: :membrane_drm_sink,
          crate: "h265_decoder"
        ],
        @rustler_opts
      )

  def create(_format, _decoder), do: :erlang.nif_error(:nif_not_loaded)
  def decode(_state, _data, _pts, _dts), do: :erlang.nif_error(:nif_not_loaded)
  def flush(_state), do: :erlang.nif_error(:nif_not_loaded)
  def get_metadata(_state), do: :erlang.nif_error(:nif_not_loaded)
  def close(_state), do: :erlang.nif_error(:nif_not_loaded)
end
