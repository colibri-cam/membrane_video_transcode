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
          otp_app: :membrane_video_transcode,
          crate: "h265_decoder"
        ],
        @rustler_opts
      )

  def create(_output, _output_format, _hw_device, _decoder),
    do: :erlang.nif_error(:nif_not_loaded)

  def decode(_state, _data, _pts, _dts), do: :erlang.nif_error(:nif_not_loaded)
  def flush(_state), do: :erlang.nif_error(:nif_not_loaded)
  def get_metadata(_state), do: :erlang.nif_error(:nif_not_loaded)
  def close(_state), do: :erlang.nif_error(:nif_not_loaded)
  def release_frame(_keepalive), do: :erlang.nif_error(:nif_not_loaded)

  def start_release_dispatcher, do: :erlang.nif_error(:nif_not_loaded)
  def quarantine_release_dispatchers, do: :erlang.nif_error(:nif_not_loaded)
  def release_dispatcher_quarantined, do: :erlang.nif_error(:nif_not_loaded)

  def close_release_dispatcher(_dispatcher, _timeout_ms),
    do: :erlang.nif_error(:nif_not_loaded)

  def new_abandonment_guard(dispatcher, owner, token, holder) do
    with {:ok, resource} <- new_abandonment_guard_resource(dispatcher, owner, token, holder) do
      {:ok, VideoInterop.AbandonmentGuard.new(resource, __MODULE__)}
    end
  end

  def new_abandonment_guard_resource(_dispatcher, _owner, _token, _holder),
    do: :erlang.nif_error(:nif_not_loaded)

  @behaviour VideoInterop.AbandonmentGuard
  @impl true
  def video_interop_abandonment_guard?(resource), do: abandonment_guard_resource(resource)

  def abandonment_guard_resource(_resource), do: :erlang.nif_error(:nif_not_loaded)
end
