import Config

config :membrane_core,
  telemetry_flags: [
    tracked_callbacks: [
      element: [
        :handle_setup,
        :handle_start_of_stream,
        :handle_stream_format,
        :handle_buffer,
        :handle_tick,
        :handle_info
      ]
    ]
  ]

config :membrane_video_linux, Membrane.Instrumentation,
  average_windows: [:timer.seconds(1), :timer.seconds(5), :timer.seconds(30)],
  bucket_resolution: 250,
  snapshot_interval: :timer.seconds(1),
  callback_shards: 4,
  snapshot_file: nil,
  callback_metrics: [],
  nif_metrics: [],
  frame_metrics: []
