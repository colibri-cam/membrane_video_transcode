defmodule Membrane.DRM.Player do
  @moduledoc """
  Membrane sink that renders raw video frames on a DRM display.

  The sink supports a range of pixel formats and uses a native NIF implemented
  in `DrmSink` to present frames on screen.

  ## Options

    * `:pixel_format` - raw video format to expect (defaults to `:I420`)
    * `:card` - path to the DRM card device (defaults to `"/dev/dri/card0"`)
  """

  use Membrane.Sink

  require Membrane.Logger

  alias Membrane.{Buffer, Time}
  alias Membrane.RawVideo

  @latency 20 |> Time.milliseconds()

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
    pixel_format = opts[:pixel_format] || :I420
    card = opts[:card] || "/dev/dri/card0"

    {[latency: @latency],
     %{
       display: nil,
       last_pts: nil,
       last_payload: nil,
       pixel_format: pixel_format,
       card: card
     }}
  end

  @impl true
  def handle_setup(_ctx, %{pixel_format: pixel_format, card: card} = state) do
    {:ok, display} = DrmSink.init_display(card, pixel_format)
    {[], %{state | display: display}}
  end

  @impl true
  def handle_stream_format(:input, stream_format, ctx, state) do
    %{input: input} = ctx.pads

    if !input.stream_format || stream_format == input.stream_format do
      {[], state}
    else
      raise "Stream format have changed while playing. This is not supported."
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
          :ok = DrmSink.display_frame(state.display, payload)
          [demand: :input]

        %{last_pts: last_pts} ->
          [timer_interval: {:demand_timer, pts - last_pts}]
      end

    {actions, %{state | last_pts: pts, last_payload: payload}}
  end

  @impl true
  def handle_tick(:demand_timer, _ctx, state) do
    :ok = DrmSink.display_frame(state.display, state.last_payload)
    {[timer_interval: {:demand_timer, :no_interval}, demand: :input], state}
  end

  @impl true
  def handle_terminate_request(_ctx, %{display: display} = state) do
    if display do
      :ok = DrmSink.close_display(display)
    end

    {[terminate: :normal], %{state | display: nil}}
  end
end
