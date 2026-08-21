defmodule Membrane.H265.Decoder.ReleaseDispatcherCustodian do
  @moduledoc false

  use GenServer

  alias Membrane.H265.Decoder.Native

  @type status :: :active | {:quarantined, term()}

  @spec start(pid()) :: {:ok, pid(), reference()} | {:error, term()}
  def start(owner) when is_pid(owner) do
    with :ok <- validate_owner(owner) do
      case GenServer.start(__MODULE__, owner) do
        {:ok, custodian} ->
          case call(custodian, :publish_dispatcher, :infinity) do
            {:ok, dispatcher} when is_reference(dispatcher) ->
              {:ok, custodian, dispatcher}

            {:error, reason} ->
              {:error, reason}

            other ->
              {:error, {:invalid_dispatcher_publication_receipt, other}}
          end

        {:error, reason} ->
          {:error, reason}
      end
    end
  end

  @spec release_joined(pid()) :: :ok | {:error, term()}
  def release_joined(custodian) when is_pid(custodian), do: call(custodian, :release_joined)

  @doc false
  @spec status(pid()) :: status()
  def status(custodian) when is_pid(custodian), do: GenServer.call(custodian, :status)

  @impl true
  def init(owner) do
    owner_monitor = Process.monitor(owner)

    case Native.start_release_dispatcher() do
      {:ok, dispatcher} when is_reference(dispatcher) ->
        {:ok,
         %{
           dispatcher: dispatcher,
           owner: owner,
           owner_monitor: owner_monitor,
           status: :active
         }}

      {:error, reason} ->
        Process.demonitor(owner_monitor, [:flush])
        {:stop, {:release_dispatcher_start_failed, reason}}

      other ->
        Process.demonitor(owner_monitor, [:flush])
        {:stop, {:invalid_release_dispatcher_start_receipt, other}}
    end
  end

  @impl true
  def handle_call(:publish_dispatcher, {owner, _tag}, %{owner: owner, status: :active} = state) do
    {:reply, {:ok, state.dispatcher}, state}
  end

  def handle_call(:publish_dispatcher, _from, state),
    do: {:reply, {:error, {:not_active_owner, state.status}}, state}

  def handle_call(:release_joined, {owner, _tag}, %{owner: owner, status: :active} = state) do
    Process.demonitor(state.owner_monitor, [:flush])
    {:stop, :normal, :ok, %{state | dispatcher: nil, owner_monitor: nil}}
  end

  def handle_call(:release_joined, _from, state),
    do: {:reply, {:error, {:not_active_owner, state.status}}, state}

  def handle_call(:status, _from, state), do: {:reply, state.status, state}

  @impl true
  def handle_info(
        {:DOWN, owner_monitor, :process, owner, reason},
        %{owner: owner, owner_monitor: owner_monitor} = state
      ) do
    _newly_quarantined? = Native.quarantine_release_dispatchers()

    {:noreply, %{state | owner: nil, owner_monitor: nil, status: {:quarantined, reason}},
     :hibernate}
  end

  def handle_info(_message, state), do: {:noreply, state}

  defp validate_owner(owner) do
    if owner == self(), do: :ok, else: {:error, :owner_must_be_caller}
  end

  defp call(custodian, request, timeout \\ 5_000) do
    GenServer.call(custodian, request, timeout)
  catch
    :exit, reason -> {:error, {:custodian_call_failed, reason}}
  end
end
