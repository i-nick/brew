# frozen_string_literal: true

require "extend/ENV"
require "cmd/reinstall"
require "cmd/shared_examples/args_parse"

RSpec.describe Homebrew::Cmd::Reinstall do
  it_behaves_like "parseable arguments"

  it "reinstalls a Formula", :aggregate_failures, :integration_test do
    setup_test_formula "testball", tab_attributes: { installed_on_request: true }

    testball_bin = HOMEBREW_CELLAR/"testball/0.1/bin"
    expect(testball_bin).not_to exist

    expect { brew "reinstall", "testball" }
      .to output(/Reinstalling testball/).to_stdout
      .and output(/✔︎.*/m).to_stderr
      .and be_a_success
    expect(testball_bin).to exist

    FileUtils.rm_r(testball_bin)

    expect { brew "reinstall", "--ask", "testball" }
      .to output(/.*Formula\s*\(1\):\s*testball.*/).to_stdout
      .and output(/✔︎.*/m).to_stderr
      .and be_a_success
    expect(testball_bin).to exist

    FileUtils.rm_r(testball_bin)

    expect { brew "reinstall", "testball", { "HOMEBREW_FORBIDDEN_FORMULAE" => "testball" } }
      .to not_to_output(%r{#{HOMEBREW_CELLAR}/testball/0\.1}o).to_stdout
      .and output(/testball was forbidden/).to_stderr
      .and be_a_failure

    expect(testball_bin).not_to exist
  end
end
