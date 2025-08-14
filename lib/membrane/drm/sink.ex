defmodule Membrane.DRM.Sink do
  @moduledoc """
  Membrane sink that renders raw video frames on a DRM display.

  The sink supports a range of pixel formats and uses a native NIF implemented
  in `Membrane.DRM.Sink.Native` to present frames on screen.

  ## Options

    * `:pixel_format` - raw video format to expect. When not provided, the
      pixel format from the incoming stream format will be used.
    * `:card` - path to the DRM card device (defaults to `"/dev/dri/card0"`)
  """

  use Membrane.Sink

  require Membrane.Logger

  alias Membrane.Buffer
  alias Membrane.RawVideo
  alias Membrane.DRM.Sink.Native

  @formats [
    :I420,
    :I422,
    :I444,
    :RGB,
    :BGRA,
    :RGBA,
    :NV12,
    :NV21,
    :YV12,
    :AYUV,
    :YUY2
  ]

  def_input_pad(:input,
    accepted_format: %RawVideo{pixel_format: fmt} when fmt in @formats,
    flow_control: :manual,
    demand_unit: :buffers
  )

  @impl true
  def handle_init(opts, _ctx) do
    card = opts[:card] || "/dev/dri/card0"

    {[],
     %{
       display: nil,
       last_pts: nil,
       last_payload: nil,
       pixel_format: opts[:pixel_format],
       card: card
     }}
  end

  @impl true
  def handle_setup(_ctx, state) do
    {[], state}
  end

  @impl true
  def handle_stream_format(:input, %RawVideo{pixel_format: fmt}, _ctx, state) do
    cond do
      state.display && fmt != state.pixel_format ->
        raise "Stream pixel format changed while playing. This is not supported."

      state.display ->
        {[], state}

      true ->
        pixel_format = state.pixel_format || fmt
        {:ok, display} = Native.init_display(state.card, pixel_format)
        {[], %{state | display: display, pixel_format: pixel_format}}
    end
  end

  @impl true
  def handle_start_of_stream(:input, _ctx, state) do
    {[demand: :input, start_timer: {:demand_timer, :no_interval}], state}
  end

  @impl true
  def handle_buffer(:input, %Buffer{payload: payload, pts: pts}, _ctx, state) do
    payload = Membrane.Payload.to_binary(payload)

    actions =
      case state do
        %{last_pts: nil, last_payload: nil} ->
          case Native.display_frame(state.display, payload) do
            :ok -> [demand: :input]
            {:error, reason} -> raise "Failed to display frame: #{inspect(reason)}"
          end

        %{last_pts: last_pts} ->
          [timer_interval: {:demand_timer, pts - last_pts}]
      end

    {actions, %{state | last_pts: pts, last_payload: payload}}
  end

  @impl true
  def handle_end_of_stream(:input, _ctx, %{display: display} = state) do
    if display do
      case Native.close_display(display) do
        :ok ->
          :ok

        {:error, reason} ->
          Membrane.Logger.warning("Failed to close display: #{inspect(reason)}")
      end
    end

    {[stop_timer: :demand_timer], %{state | display: nil}}
  end

  @impl true
  def handle_tick(:demand_timer, _ctx, %{display: nil} = state) do
    {[], state}
  end

  def handle_tick(:demand_timer, _ctx, state) do
    case Native.display_frame(state.display, state.last_payload) do
      :ok ->
        {[timer_interval: {:demand_timer, :no_interval}, demand: :input], state}

      {:error, reason} ->
        raise "Failed to display frame: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_terminate_request(_ctx, %{display: display} = state) do
    if display do
      case Native.close_display(display) do
        :ok ->
          :ok

        {:error, reason} ->
          Membrane.Logger.warning("Failed to close display: #{inspect(reason)}")
      end
    end

    {[terminate: :normal], %{state | display: nil}}
  end
end
