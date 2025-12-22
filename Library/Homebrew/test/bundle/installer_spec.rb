# frozen_string_literal: true

require "bundle"
require "bundle/dsl"
require "bundle/installer"

RSpec.describe Homebrew::Bundle::Installer do
  let(:formula_entry) { Homebrew::Bundle::Dsl::Entry.new(:brew, "mysql") }
  let(:cask_options) { { args: {}, full_name: "homebrew/cask/google-chrome" } }
  let(:cask_entry) { Homebrew::Bundle::Dsl::Entry.new(:cask, "google-chrome", cask_options) }

  before do
    allow(Homebrew::Bundle::Skipper).to receive(:skip?).and_return(false)
    allow(Homebrew::Bundle::FormulaInstaller).to receive_messages(formula_upgradable?: false, install!: true)
    allow(Homebrew::Bundle::CaskInstaller).to receive_messages(cask_upgradable?: false, install!: true)
  end

  it "prefetches installable formulae and casks before installing" do
    allow(Homebrew::Bundle::FormulaInstaller).to receive(:formula_installed_and_up_to_date?)
      .with("mysql", no_upgrade: false).and_return(false)
    allow(Homebrew::Bundle::CaskInstaller).to receive(:installable_or_upgradable?)
      .with("google-chrome", no_upgrade: false, **cask_options).and_return(true)

    expect(Homebrew::Bundle).to receive(:brew)
      .with("fetch", "mysql", "homebrew/cask/google-chrome", verbose: false)
      .ordered
      .and_return(true)
    expect(Homebrew::Bundle::FormulaInstaller).to receive(:preinstall!)
      .with("mysql", no_upgrade: false, verbose: false)
      .ordered
      .and_return(true)
    expect(Homebrew::Bundle::CaskInstaller).to receive(:preinstall!)
      .with("google-chrome", **cask_options, no_upgrade: false, verbose: false)
      .ordered
      .and_return(true)

    described_class.install!([formula_entry, cask_entry], verbose: false, force: false, quiet: true)
  end

  it "skips fetching when no formulae or casks need installation or upgrade" do
    allow(Homebrew::Bundle::FormulaInstaller).to receive(:formula_installed_and_up_to_date?)
      .with("mysql", no_upgrade: true).and_return(true)
    allow(Homebrew::Bundle::FormulaInstaller).to receive(:preinstall!).and_return(false)

    expect(Homebrew::Bundle).not_to receive(:brew).with("fetch", any_args)

    described_class.install!([formula_entry], no_upgrade: true, quiet: true)
  end
end
