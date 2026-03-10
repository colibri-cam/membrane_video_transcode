defmodule Membrane.DRM.Instrumentation.Manager do
  @moduledoc false

  use GenServer

  alias Membrane.DRM.Instrumentation
  alias Membrane.DRM.Instrumentation.Router
  alias Membrane.DRM.Instrumentation.Session
  alias Membrane.DRM.Instrumentation.SessionConfig

  @handler_id "membrane-drm-runtime-router"
  @snapshot_table :membrane_drm_instrumentation_snapshots

  defstruct sessions: %{}

  @spec start_link(keyword()) :: GenServer.on_start()
  def start_link(opts \\ []) do
    GenServer.start_link(__MODULE__, opts, name: __MODULE__)
  end

  @spec start_session(keyword()) :: {:ok, term()} | {:error, term()}
  def start_session(opts), do: GenServer.call(__MODULE__, {:start_session, opts})

  @spec update_session(term(), keyword()) :: :ok | {:error, term()}
  def update_session(name, opts), do: GenServer.call(__MODULE__, {:update_session, name, opts})

  @spec stop_session(term()) :: :ok
  def stop_session(name), do: GenServer.call(__MODULE__, {:stop_session, name})

  @spec list_sessions() :: [term()]
  def list_sessions, do: GenServer.call(__MODULE__, :list_sessions)

  @spec snapshot(term()) :: {:ok, map()} | {:error, :not_found}
  def snapshot(name), do: GenServer.call(__MODULE__, {:snapshot, name})

  @spec runtime_state() :: map()
  def runtime_state, do: Router.routes()

  @impl true
  def init(_opts) do
    ensure_snapshot_table()

    case Instrumentation.attach_default(@handler_id, &Router.handle_event/4, nil) do
      :ok -> :ok
      {:error, :already_exists} -> :ok
    end

    Router.put_routes(empty_routes())
    {:ok, %__MODULE__{}}
  end

  @impl true
  def terminate(_reason, _state) do
    Instrumentation.detach(@handler_id)
    Router.put_routes(empty_routes())
    :ok
  end

  @impl true
  def handle_call({:start_session, opts}, _from, state) do
    defaults = Instrumentation.config()
    config = SessionConfig.normalize(opts, defaults)

    if Map.has_key?(state.sessions, config.name) do
      {:reply, {:error, :already_started}, state}
    else
      child_spec = Supervisor.child_spec({Session, config: config}, id: {:session, config.name})

      with {:ok, pid} <-
             DynamicSupervisor.start_child(Instrumentation.SessionSupervisor, child_spec) do
        route_info = Session.route_info(pid)
        next_state = put_session(state, opts, config, pid, route_info)
        {:reply, {:ok, config.name}, rebuild_routes(next_state)}
      end
    end
  end

  def handle_call({:update_session, name, opts}, _from, state) do
    case state.sessions[name] do
      nil ->
        {:reply, {:error, :not_found}, state}

      %{pid: pid, opts: current_opts} ->
        merged_opts = Keyword.merge(current_opts, opts) |> Keyword.put(:name, name)
        config = SessionConfig.normalize(merged_opts, Instrumentation.config())
        :ok = Session.update_config(pid, config)
        route_info = Session.route_info(pid)
        demonitor_session(state.sessions[name])
        next_state = put_session(state, merged_opts, config, pid, route_info)
        {:reply, :ok, rebuild_routes(next_state)}
    end
  end

  def handle_call({:stop_session, name}, _from, state) do
    case Map.pop(state.sessions, name) do
      {nil, _sessions} ->
        {:reply, :ok, state}

      {%{pid: pid}, sessions} ->
        demonitor_session(state.sessions[name])
        :ok = DynamicSupervisor.terminate_child(Instrumentation.SessionSupervisor, pid)
        :ets.delete(@snapshot_table, name)
        next_state = %{state | sessions: sessions}
        {:reply, :ok, rebuild_routes(next_state)}
    end
  end

  def handle_call(:list_sessions, _from, state) do
    {:reply, Map.keys(state.sessions), state}
  end

  def handle_call({:snapshot, name}, _from, state) do
    case state.sessions[name] do
      nil ->
        {:reply, {:error, :not_found}, state}

      %{pid: pid} ->
        {:reply, {:ok, Session.snapshot(pid)}, state}
    end
  end

  @impl true
  def handle_info({:DOWN, ref, :process, _pid, _reason}, state) do
    case Enum.find(state.sessions, fn {_name, session} -> session.monitor_ref == ref end) do
      nil ->
        {:noreply, state}

      {name, %{pid: pid}} ->
        _ = DynamicSupervisor.terminate_child(Instrumentation.SessionSupervisor, pid)
        :ets.delete(@snapshot_table, name)
        next_state = %{state | sessions: Map.delete(state.sessions, name)}
        {:noreply, rebuild_routes(next_state)}
    end
  end

  defp put_session(state, opts, config, pid, route_info) do
    monitor_ref = if config.pipeline_pid, do: Process.monitor(config.pipeline_pid)

    %{
      state
      | sessions:
          Map.put(state.sessions, config.name, %{
            opts: opts,
            config: config,
            pid: pid,
            route_info: route_info,
            monitor_ref: monitor_ref
          })
    }
  end

  defp rebuild_routes(state) do
    routing =
      Enum.reduce(state.sessions, empty_routes(), fn {_name, session}, acc ->
        acc
        |> merge_routes(:callbacks, session.route_info.callbacks)
        |> merge_routes(:nifs, session.route_info.nifs)
        |> merge_routes(:frames, session.route_info.frames)
      end)
      |> Map.put(:active_sessions, map_size(state.sessions))
      |> Map.put(
        :custom_metrics?,
        Enum.any?(state.sessions, fn {_name, session} -> session.route_info.nifs != [] end)
      )
      |> Map.put(
        :frame_metrics?,
        Enum.any?(state.sessions, fn {_name, session} -> session.route_info.frames != [] end)
      )

    Router.put_routes(routing)
    state
  end

  defp merge_routes(acc, key, routes) do
    merged =
      Enum.reduce(routes, acc[key], fn route, route_map ->
        Map.update(route_map, {route.event_key, route.component_key}, [route], &[route | &1])
      end)

    Map.put(acc, key, merged)
  end

  defp empty_routes do
    %{
      active_sessions: 0,
      custom_metrics?: false,
      frame_metrics?: false,
      callbacks: %{},
      nifs: %{},
      frames: %{}
    }
  end

  defp ensure_snapshot_table do
    case :ets.whereis(@snapshot_table) do
      :undefined ->
        :ets.new(@snapshot_table, [:named_table, :public, :set, read_concurrency: true])

      _table ->
        :ok
    end
  end

  defp demonitor_session(%{monitor_ref: nil}), do: :ok

  defp demonitor_session(%{monitor_ref: ref}) do
    Process.demonitor(ref, [:flush])
    :ok
  end
end
