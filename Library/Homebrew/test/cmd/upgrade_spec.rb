# frozen_string_literal: true

require "cmd/shared_examples/args_parse"
require "cmd/upgrade"
require "cmd/shared_examples/reinstall_pkgconf_if_needed"

RSpec.describe Homebrew::Cmd::UpgradeCmd do
  include FileUtils

  it_behaves_like "parseable arguments"

  it "upgrades a Formula and cleans up old versions", :integration_test do
    setup_test_formula "testball"
    (HOMEBREW_CELLAR/"testball/0.0.1/foo").mkpath

    expect { brew "upgrade" }.to be_a_success

    expect(HOMEBREW_CELLAR/"testball/0.1").to be_a_directory
    expect(HOMEBREW_CELLAR/"testball/0.0.1").not_to exist
  end

  it "links newer version when upgrade was interrupted", :integration_test do
    setup_test_formula "testball"

    (HOMEBREW_CELLAR/"testball/0.1/foo").mkpath

    expect { brew "upgrade" }.to be_a_success

    expect(HOMEBREW_CELLAR/"testball/0.1").to be_a_directory
    expect(HOMEBREW_PREFIX/"opt/testball").to be_a_symlink
    expect(HOMEBREW_PREFIX/"var/homebrew/linked/testball").to be_a_symlink
  end

  it "upgrades with asking for user prompts", :integration_test do
    setup_test_formula "testball"
    (HOMEBREW_CELLAR/"testball/0.0.1/foo").mkpath

    expect do
      brew "upgrade", "--ask"
    end.to output(/.*Formula\s*\(1\):\s*testball.*/).to_stdout.and not_to_output.to_stderr

    expect(HOMEBREW_CELLAR/"testball/0.1").to be_a_directory
    expect(HOMEBREW_CELLAR/"testball/0.0.1").not_to exist
  end

  it "refuses to upgrades a forbidden formula", :integration_test do
    setup_test_formula "testball"
    (HOMEBREW_CELLAR/"testball/0.0.1/foo").mkpath

    expect { brew "upgrade", "testball", { "HOMEBREW_FORBIDDEN_FORMULAE" => "testball" } }
      .to not_to_output(%r{#{HOMEBREW_CELLAR}/testball/0\.1}o).to_stdout
      .and output(/testball was forbidden/).to_stderr
      .and be_a_failure
    expect(HOMEBREW_CELLAR/"testball/0.1").not_to exist
  end

  it_behaves_like "reinstall_pkgconf_if_needed"
end
