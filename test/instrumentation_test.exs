defmodule MembraneDRM.InstrumentationTest do
  use ExUnit.Case, async: false

  alias Membrane.Instrumentation
  alias Membrane.Instrumentation.FrameTrace
  alias Membrane.Instrumentation.SessionConfig
  alias Membrane.Instrumentation.WindowedStats

  setup do
    Application.put_env(:membrane_video_linux, Instrumentation,
      average_windows: [:timer.seconds(1), :timer.seconds(5), :timer.seconds(30)],
      bucket_resolution: 250,
      snapshot_interval: :timer.seconds(1),
      callback_shards: 2,
      snapshot_file: nil,
      callback_metrics: [],
      nif_metrics: [],
      frame_metrics: []
    )

    Instrumentation.ensure_started()

    on_exit(fn ->
      if Process.whereis(Membrane.Instrumentation.Manager) do
        for session <- Instrumentation.list_sessions() do
          :ok = Instrumentation.stop_session(session)
        end
      end
    end)

    :ok
  end

  test "frame traces derive shared trace ids and keep origin time" do
    parent = FrameTrace.new(pts: 1_000)
    child = FrameTrace.derive(parent, pts: 2_000)

    assert child.trace_id == parent.trace_id
    assert child.frame_id != parent.frame_id
    assert child.created_at_ns == parent.created_at_ns
    assert child.pts == 2_000
  end

  test "windowed stats expose avg min and max across windows" do
    stats = WindowedStats.new(:timer, [1_000, 5_000], 250)
    now_ms = System.monotonic_time(:millisecond)

    snapshot =
      stats
      |> WindowedStats.record(now_ms - 100, 1_000_000)
      |> WindowedStats.record(now_ms - 50, 3_000_000)
      |> WindowedStats.snapshot(now_ms)

    assert snapshot[1_000].avg_ms == 2.0
    assert snapshot[1_000].min_ms == 1.0
    assert snapshot[1_000].max_ms == 3.0
    assert snapshot[1_000].samples == 2
  end

  test "session collects callback metrics with configured rolling windows" do
    {:ok, :video_session} =
      Instrumentation.start_session(
        name: :video_session,
        pipeline: self(),
        average_windows: [1_000, 5_000, 30_000],
        bucket_resolution: 250,
        callback_metrics: [
          [component: :video_decoder, callbacks: [:handle_buffer], sample_rate: 1]
        ]
      )

    path = [pipeline_segment(self()), ":video_decoder"]

    :telemetry.execute(
      [:membrane, :element, :handle_buffer, :stop],
      %{duration: System.convert_time_unit(4, :millisecond, :native)},
      %{component_path: path}
    )

    Process.sleep(10)
    assert {:ok, snapshot} = Instrumentation.snapshot(:video_session)
    metric = snapshot.callback_metrics[{:video_decoder, :handle_buffer}]
    assert metric[1_000].avg_ms == 4.0
    assert metric[1_000].min_ms == 4.0
    assert metric[1_000].max_ms == 4.0
  end

  test "session collects custom nif timings for configured components" do
    {:ok, :nif_session} =
      Instrumentation.start_session(
        name: :nif_session,
        pipeline: self(),
        average_windows: [1_000],
        bucket_resolution: 250,
        nif_metrics: [
          [component: :video_decoder, metric: :decode, sample_rate: 1]
        ]
      )

    Instrumentation.emit(
      [:nif, :h265_prime_decoder, :decode, :stop],
      %{duration_ns: 2_500_000},
      %{component_path: [pipeline_segment(self()), ":video_decoder"], result: :ok}
    )

    Process.sleep(10)
    assert {:ok, snapshot} = Instrumentation.snapshot(:nif_session)
    metric = snapshot.nif_metrics[{:video_decoder, :decode}]
    assert metric[1_000].avg_ms == 2.5
    assert metric[1_000].min_ms == 2.5
    assert metric[1_000].max_ms == 2.5
  end

  test "session writes snapshots to configured file" do
    tmp_dir = Path.join(System.tmp_dir!(), "membrane_drm_session_snapshot_test")
    snapshot_file = Path.join(tmp_dir, "snapshot.txt")
    File.rm_rf!(tmp_dir)

    {:ok, :file_session} =
      Instrumentation.start_session(
        name: :file_session,
        pipeline: self(),
        average_windows: [1_000],
        bucket_resolution: 250,
        snapshot_interval: 10,
        snapshot_file: snapshot_file,
        callback_metrics: [
          [component: :video_player, callbacks: [:handle_buffer], sample_rate: 1]
        ]
      )

    :telemetry.execute(
      [:membrane, :element, :handle_buffer, :stop],
      %{duration: System.convert_time_unit(1, :millisecond, :native)},
      %{component_path: [pipeline_segment(self()), ":video_player"]}
    )

    Process.sleep(30)
    assert File.exists?(snapshot_file)
    assert {:ok, contents} = File.read(snapshot_file)
    assert contents =~ "Session :file_session"
    assert contents =~ "{:video_player, :handle_buffer}"
  end

  test "session config normalizes runtime windows" do
    config =
      SessionConfig.normalize(
        [
          name: :session,
          average_windows: [:timer.seconds(5), :timer.seconds(1), :timer.seconds(30)],
          callback_metrics: [[component: :video_decoder, callbacks: [:handle_buffer]]]
        ],
        []
      )

    assert config.average_windows_ms == [1_000, 5_000, 30_000]
  end

  defp pipeline_segment(pid) do
    pid
    |> :erlang.pid_to_list()
    |> to_string()
    |> Kernel.<>("/")
  end
end
