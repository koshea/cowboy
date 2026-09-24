# Homebrew formula for cowboy, for a tap (`brew tap koshea/cowboy`).
#
# Installs the prebuilt Apple silicon release rather than building from source: the
# release binary has the web UI embedded, which a source build would need trunk and
# a wasm toolchain for. Copy this into the tap repository's `Formula/` directory and
# bump `version` and `sha256` for each release — the sha256 is the one in the
# release's SHA256SUMS for the `aarch64-apple-darwin` archive.
#
# A formula download is not quarantined, so Gatekeeper does not block the unsigned
# (ad-hoc signed) binaries the way it would a browser download.
class Cowboy < Formula
  desc "Opinionated local coding agent in a kernel-enforced sandbox"
  homepage "https://github.com/koshea/cowboy"
  version "0.1.0"
  license "MIT"

  depends_on arch: :arm64
  depends_on macos: :tahoe

  url "https://github.com/koshea/cowboy/releases/download/v#{version}/cowboy-#{version}-aarch64-apple-darwin.tar.gz"
  sha256 "REPLACE_WITH_THE_RELEASE_SHA256"

  def install
    # `cowboy` and `cowboyd` are version-locked to each other; install both.
    bin.install "cowboy", "cowboyd"
  end

  def caveats
    <<~EOS
      Run `cowboy doctor` to check that this Mac can run the sandbox, then
      `cowboy init` in a project.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/cowboy --version")
  end
end
