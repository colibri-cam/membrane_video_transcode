defmodule MembraneVideoTranscodeTest do
  use ExUnit.Case, async: false

  alias Membrane.H265.Decoder.Native
  alias VideoInterop.{AbandonmentGuard, LeaseOwner}

  test "package exposes codec elements without legacy presentation modules" do
    assert Code.ensure_loaded?(Membrane.H265.Decoder)
    refute Code.ensure_loaded?(Membrane.Display.Sink)
    refute Code.ensure_loaded?(Membrane.PrimeFormat)
  end

  test "guarded decoder leases release and drain before dispatcher shutdown" do
    assert {:ok, dispatcher} = Native.start_release_dispatcher()
    test_pid = self()

    guard_factory = fn owner, token, holder ->
      Native.new_abandonment_guard(dispatcher, owner, token, holder)
    end

    assert {:ok, owner} =
             LeaseOwner.start_link(
               producer: self(),
               release: fn backend ->
                 send(test_pid, {:released, backend})
                 :ok
               end,
               abandonment_guard_factory: guard_factory
             )

    assert {:ok, lease} = LeaseOwner.issue(owner, :decoded_frame)
    assert AbandonmentGuard.valid?(lease.abandonment_guard)
    assert :ok = VideoInterop.release(lease)
    assert_receive {:released, :decoded_frame}, 1_000
    assert :ok = LeaseOwner.drain(owner)
    assert {:ok, true} = Native.close_release_dispatcher(dispatcher, 1_000)
  end

  test "native abandonment guards are authority verified and dispatcher backed" do
    assert {:ok, dispatcher} = Native.start_release_dispatcher()
    token = make_ref()
    holder = make_ref()

    guard =
      case Native.new_abandonment_guard(dispatcher, self(), token, holder) do
        {:ok, guard} -> guard
        other -> flunk("unexpected guard result: #{inspect(other)}")
      end

    assert AbandonmentGuard.valid?(guard)

    guard = nil
    :erlang.garbage_collect(self())
    assert guard == nil
    assert_receive {:video_interop_abandoned, ^token, ^holder}, 1_000
    assert {:ok, true} = Native.close_release_dispatcher(dispatcher, 1_000)
  end
end
