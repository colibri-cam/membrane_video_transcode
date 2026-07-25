defmodule Membrane.H265.Decoder.Native do
  @rustler_opts Mix.Project.config()[:rustler_opts]

  defmodule DMABufFrame do
    @moduledoc false
    @enforce_keys [:width, :height, :modifier, :descriptor, :keepalive]
    defstruct @enforce_keys
  end

  use Rustler,
      Keyword.merge(
        [
          otp_app: :membrane_video_linux,
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
  def release_frame(_keepalive), do: :erlang.nif_error(:nif_not_loaded)
end
