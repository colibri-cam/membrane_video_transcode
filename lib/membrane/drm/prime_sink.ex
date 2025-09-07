defmodule Membrane.DRM.PrimeSink do
  @moduledoc """
  Sink that receives DRM Prime descriptors and scans them out directly using a
  native NIF (`Membrane.DRM.PrimeSink.Native`) without copying frame data.
  """

  use Membrane.Sink

  require Membrane.Logger

  alias Membrane.Buffer
  alias Membrane.PrimeFormat
  alias Membrane.DRM.PrimeSink.Native

  def_options(
    card: [
      spec: String.t(),
      default: "/dev/dri/card0",
      description: "Graphic card to use"
    ],
    preferred_mode: [
      spec: {pos_integer(), pos_integer(), pos_integer()} | nil,
      default: nil,
      description: "Preferred video mode as {width, height, framerate}"
    ],
    ignore_pts: [
      spec: boolean,
      default: false,
      description: "Consume frames as fast as possible, skips frames beetween vblanks"
    ]
  )

  def_input_pad(:input,
    accepted_format: %PrimeFormat{},
    flow_control: :manual,
    demand_unit: :buffers
  )

  @impl true
  def handle_init(_ctx, opts) do
    {[],
     %{
       ignore_pts: opts.ignore_pts,
       display: nil,
       last_pts: nil,
       last_desc: nil,
       card: opts.card,
       preferred_mode: opts.preferred_mode
     }}
  end

  @impl true
  def handle_setup(_ctx, state) do
    {:ok, info, display} = Native.init_display(state.card, state.preferred_mode)
    {w, h, r} = info.mode

    Membrane.Logger.info(
      "Using card #{info.card_path}, connector #{info.connector_id} (#{info.connector_type}), " <>
        "plane #{info.plane_id}, mode #{w}x#{h}@#{r}"
    )

    {[], %{state | display: display}}
  end

  @impl true
  #def handle_stream_format(:input, %PrimeFormat{}, _ctx, %{display: nil} = state) do
  #  {:ok, info, display} = Native.init_display(state.card, state.preferred_mode)
  #  {w, h, r} = info.mode

  #  Membrane.Logger.info(
  #    "Using card #{info.card_path}, connector #{info.connector_id} (#{info.connector_type}), " <>
  #      "plane #{info.plane_id}, mode #{w}x#{h}@#{r}"
  #  )

  #  {[], %{state | display: display}}
  #end

  def handle_stream_format(:input, _, _ctx, state), do: {[], state}

  @impl true
  def handle_start_of_stream(:input, _ctx, state) do
    {[demand: :input], state}
  end

  @impl true
  def handle_buffer(
        :input,
        %Buffer{pts: pts, metadata: %{drm_prime: desc}},
        _ctx,
        %{ignore_pts: true} = state
      ) do
    :erlang.garbage_collect(self())
    case Native.display_prime(state.display, desc) do
      :ok ->
        Membrane.Logger.debug("Displayed frame: #{inspect(desc)}")
        {[demand: :input], %{state | last_pts: pts, last_desc: desc}}

      {:error, reason} ->
        Membrane.Logger.error("Failed to display frame: #{inspect(reason)}")
        raise "Failed to display frame: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_buffer(
        :input,
        %Buffer{pts: pts, metadata: %{drm_prime: desc}},
        _ctx,
        state
      ) do
    :erlang.garbage_collect(self())
    actions =
      case state do
        %{last_pts: nil, last_desc: nil} ->
          case Native.display_prime(state.display, desc) do
            :ok ->
              Membrane.Logger.debug("Displayed frame: #{inspect(desc)}")
              [demand: :input, start_timer: {:demand_timer, :no_interval}]

            {:error, reason} ->
              Membrane.Logger.error("Failed to display frame: #{inspect(reason)}")
              raise "Failed to display frame: #{inspect(reason)}"
          end

        %{last_pts: last_pts} ->
          [timer_interval: {:demand_timer, pts - last_pts}]
      end

    {actions, %{state | last_pts: pts, last_desc: desc}}
  end

  @impl true
  def handle_tick(:demand_timer, _ctx, %{display: nil} = state) do
    {[], state}
  end

  def handle_tick(:demand_timer, _ctx, state) do
    :erlang.garbage_collect(self())
    case Native.display_prime(state.display, state.last_desc) do
      :ok ->
        Membrane.Logger.debug("Displayed frame: #{inspect(state.last_desc)}")
        {[timer_interval: {:demand_timer, :no_interval}, demand: :input], %{state| last_desc: nil}}

      {:error, reason} ->
        Membrane.Logger.error("Failed to display frame: #{inspect(reason)}")
        raise "Failed to display frame: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_end_of_stream(:input, _ctx, %{display: display} = state) do
    if display do
      case Native.close_display(display) do
        :ok -> :ok
        {:error, reason} -> Membrane.Logger.error("Failed to close display: #{inspect(reason)}")
      end
    end

    actions =
      if state.ignore_pts,
        do: [],
        else: [stop_timer: :demand_timer]

    :erlang.garbage_collect(self())
    {actions, %{state | display: nil}}
  end

  @impl true
  def handle_terminate_request(_ctx, %{display: display} = state) do
    if display do
      case Native.close_display(display) do
        :ok -> :ok
        {:error, reason} -> Membrane.Logger.warning("Failed to close display: #{inspect(reason)}")
      end
    end

    {[terminate: :normal], %{state | display: nil}}
  end
end
