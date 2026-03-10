defmodule Membrane.Instrumentation.Supervisor do
  @moduledoc false

  use Supervisor

  alias Membrane.Instrumentation.Manager

  @spec start_link(keyword()) :: Supervisor.on_start()
  def start_link(opts \\ []) do
    Supervisor.start_link(__MODULE__, opts, name: __MODULE__)
  end

  @impl true
  def init(_opts) do
    children = [
      {DynamicSupervisor,
       strategy: :one_for_one, name: Membrane.Instrumentation.SessionSupervisor},
      {Manager, []}
    ]

    Supervisor.init(children, strategy: :one_for_all)
  end
end
