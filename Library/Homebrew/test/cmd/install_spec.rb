# frozen_string_literal: true

require "cmd/install"
require "cmd/shared_examples/args_parse"

RSpec.describe Homebrew::Cmd::InstallCmd do
  include FileUtils

  let(:testball1_rack) { HOMEBREW_CELLAR/"testball1" }

  it_behaves_like "parseable arguments"

  it "installs a Formula", :integration_test do
    setup_test_formula "testball1"

    expect { brew "install", "testball1" }
      .to output(%r{#{HOMEBREW_CELLAR}/testball1/0\.1}o).to_stdout
      .and output(/✔︎.*/m).to_stderr
      .and be_a_success
    expect(testball1_rack/"0.1/foo/test").not_to be_a_file

    uninstall_test_formula "testball1"

    expect { brew "install", "testball1", "--with-foo" }
      .to output(%r{#{HOMEBREW_CELLAR}/testball1/0\.1}o).to_stdout
      .and output(/✔︎.*/m).to_stderr
      .and be_a_success
    expect(testball1_rack/"0.1/foo/test").to be_a_file

    uninstall_test_formula "testball1"

    expect { brew "install", "testball1", "--debug-symbols", "--build-from-source" }
      .to output(%r{#{HOMEBREW_CELLAR}/testball1/0\.1}o).to_stdout
      .and output(/✔︎.*/m).to_stderr
      .and be_a_success
    expect(testball1_rack/"0.1/bin/test").to be_a_file
    expect(testball1_rack/"0.1/bin/test.dSYM/Contents/Resources/DWARF/test").to be_a_file if OS.mac?
    expect(HOMEBREW_CACHE/"Sources/testball1").to be_a_directory

    uninstall_test_formula "testball1"

    expect { brew "install", "--ask", "testball1" }
      .to output(/.*Formula\s*\(1\):\s*testball1.*/).to_stdout
      .and output(/✔︎.*/m).to_stderr
      .and be_a_success
    expect(testball1_rack/"0.1/bin/test").to be_a_file

    uninstall_test_formula "testball1"

    expect { brew "install", "testball1", { "HOMEBREW_FORBIDDEN_FORMULAE" => "testball1" } }
      .to not_to_output(%r{#{HOMEBREW_CELLAR}/testball1/0\.1}o).to_stdout
      .and output(/testball1 was forbidden/).to_stderr
      .and be_a_failure
    expect(testball1_rack).not_to exist
  end

  it "installs a keg-only Formula", :integration_test do
    setup_test_formula "testball1", <<~RUBY
      version "1.0"

      keg_only "test reason"
    RUBY

    expect { brew "install", "testball1" }
      .to output(%r{#{testball1_rack}/1\.0}o).to_stdout
      .and output(/✔︎.*/m).to_stderr
      .and be_a_success
    expect(testball1_rack/"1.0/foo/test").not_to be_a_file
  end

  it "installs a HEAD Formula", :integration_test do
    repo_path = HOMEBREW_CACHE/"repo"
    (repo_path/"bin").mkpath

    repo_path.cd do
      system "git", "-c", "init.defaultBranch=master", "init"
      system "git", "remote", "add", "origin", "https://github.com/Homebrew/homebrew-foo"
      FileUtils.touch "bin/something.bin"
      FileUtils.touch "README"
      system "git", "add", "--all"
      system "git", "commit", "-m", "Initial repo commit"
    end

    setup_test_formula "testball1", <<~RUBY
      version "1.0"

      head "file://#{repo_path}", :using => :git

      def install
        prefix.install Dir["*"]
      end
    RUBY

    # Ignore dependencies, because we'll try to resolve requirements in build.rb
    # and there will be the git requirement, but we cannot instantiate git
    # formula since we only have testball1 formula.
    expect { brew "install", "testball1", "--HEAD", "--ignore-dependencies", "HOMEBREW_DOWNLOAD_CONCURRENCY" => "1" }
      .to output(%r{#{testball1_rack}/HEAD-d5eb689}o).to_stdout
      .and output(/Cloning into/).to_stderr
      .and be_a_success
    expect(testball1_rack/"HEAD-d5eb689/foo/test").not_to be_a_file
  end
end
