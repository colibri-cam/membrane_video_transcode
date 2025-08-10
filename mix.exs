defmodule DrmExperiments.MixProject do
  use Mix.Project

  @nerves_rust_target_triple_mapping %{
    "armv6-nerves-linux-gnueabihf" => "arm-unknown-linux-gnueabihf",
    "armv7-nerves-linux-gnueabihf" => "armv7-unknown-linux-gnueabihf",
    "aarch64-nerves-linux-gnu" => "aarch64-unknown-linux-gnu"
  }

  def project do
    maybe_set_rustler_target()

    [
      app: :drm_experiments,
      version: "0.1.0",
      elixir: "~> 1.18",
      start_permanent: Mix.env() == :prod,
      deps: deps(),
      rustler_crates: [
        drm_sink: [
          path: "native/drm_sink",
          mode: if(Mix.env() == :prod, do: :release, else: :debug)
        ],
        h265decoder: [
          path: "native/h265decoder",
          mode: if(Mix.env() == :prod, do: :release, else: :debug)
        ]
      ]
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
      {:castore, "~> 1.0 or ~> 0.1"},
      {:membrane_core, "~> 1.2.4"},
      {:membrane_h265_format, "~> 0.2"},
      {:membrane_raw_video_format, "~> 0.4"}
    ]
  end

  defp maybe_set_rustler_target do
    if System.get_env("NERVES_SDK_SYSROOT") do
      cc = System.get_env("CC")

      if cc do
        System.put_env("RUSTFLAGS", "-C linker=#{cc}")

        target_triple =
          cc
          |> Path.basename()
          |> String.split("-")
          |> Enum.drop(-1)
          |> Enum.join("-")

        if mapping = @nerves_rust_target_triple_mapping[target_triple] do
          System.put_env("RUSTLER_TARGET", mapping)
        end
      end
    end
  end
end
