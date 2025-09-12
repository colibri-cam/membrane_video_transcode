defmodule DrmExperiments.MixProject do
  use Mix.Project

  @nerves_rust_target_triple_mapping %{
    "armv6-nerves-linux-gnueabihf" => "arm-unknown-linux-gnueabihf",
    "armv7-nerves-linux-gnueabihf" => "armv7-unknown-linux-gnueabihf",
    "aarch64-nerves-linux-gnu" => "aarch64-unknown-linux-gnu",
    "x86_64-nerves-linux-musl" => "x86_64-unknown-linux-musl"
  }

  def project do
    [
      app: :membrane_drm_sink,
      version: "0.1.0",
      elixir: "~> 1.17",
      start_permanent: Mix.env() == :prod,
      deps: deps(),
      rustler_opts: configure_rustler_cross_compile(System.get_env("NERVES_SDK_SYSROOT"))
    ]
  end

  # Run "mix help compile.app" to learn about applications.
  def application do
    [
      extra_applications: [:logger]
    ]
  end

  # Run "mix help deps" to learn about dependencies.
  defp deps do
    [
      {:rustler, "~> 0.36.2"},
      {:membrane_core, "~> 1.2.4"},
      {:membrane_h265_format, "~> 0.2"},
      {:membrane_raw_video_format, "~> 0.4"}
    ]
  end

  defp configure_rustler_cross_compile(nil), do: []

  defp configure_rustler_cross_compile(_target) do
    cc = System.get_env("CC")

    if cc do
      target_triple =
        cc
        |> Path.basename()
        |> String.split("-")
        |> Enum.drop(-1)
        |> Enum.join("-")
        |> then(&Map.get(@nerves_rust_target_triple_mapping, &1))

      upcase_target_triple =
        target_triple
        |> String.upcase()
        |> String.replace("-", "_")

      [
        target: target_triple,
        features: ["rpi"],
        env: [
          {"CARGO_TARGET_#{upcase_target_triple}_LINKER", cc},
          {"HOST_FFMPEG_DIR", System.get_env("NERVES_TOOLCHAIN")},
          {"FFMPEG_DIR", System.get_env("NERVES_SDK_SYSROOT") <> "/usr"},
          {"CFLAGS", ""},
          {"CC", ""}
        ]
      ]
    end
  end
end
