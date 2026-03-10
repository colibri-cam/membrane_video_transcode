defmodule Membrane.H265.Decoder.Native do
  @rustler_opts Mix.Project.config()[:rustler_opts]

  use Rustler,
      Keyword.merge(
        [
          otp_app: :membrane_linux_video,
          crate: "h265_prime_decoder"
        ],
        @rustler_opts
      )

  def create(_output, _output_format, _hw_device, _decoder),
    do: :erlang.nif_error(:nif_not_loaded)

  def decode(_state, _data, _pts, _dts), do: :erlang.nif_error(:nif_not_loaded)
  def flush(_state), do: :erlang.nif_error(:nif_not_loaded)
  def get_metadata(_state), do: :erlang.nif_error(:nif_not_loaded)
  def close(_state), do: :erlang.nif_error(:nif_not_loaded)
  def keepalive_release(_keepalive), do: :erlang.nif_error(:nif_not_loaded)
end
